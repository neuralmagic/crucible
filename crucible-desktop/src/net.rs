//! What every controller worker shares: one async runtime, one response cache, and one connection
//! per profile.
//!
//! ```text
//! Net ── runtime, store
//!  └─ Connector (one per profile, cloned into its workers) ── opens once ──▶ Connection
//!                                                                             client + runtime
//! ```

use std::future::Future;
#[cfg(not(target_arch = "wasm32"))]
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use tokio::runtime::Runtime;

#[cfg(not(target_arch = "wasm32"))]
use crate::httpcache::CachedTransport;
use crate::httpcache::Store;
use crate::profile::Target;

const TIMEOUT: Duration = Duration::from_secs(25);

/// Where tasks run: the shared tokio runtime, or the browser's own loop.
#[cfg(not(target_arch = "wasm32"))]
pub type Handle = tokio::runtime::Handle;

#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub struct Handle;

#[cfg(target_arch = "wasm32")]
impl Handle {
    pub fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        wasm_bindgen_futures::spawn_local(task);
    }
}

pub struct Net {
    /// Never shut down.
    #[cfg(not(target_arch = "wasm32"))]
    runtime: Result<ManuallyDrop<Runtime>, String>,
    store: Option<Arc<Store>>,
}

#[cfg(target_arch = "wasm32")]
impl Net {
    pub fn start() -> Net {
        Net { store: None }
    }

    pub fn handle(&self) -> Result<Handle, String> {
        Ok(Handle)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Net {
    pub fn start() -> Net {
        let store = crate::httpcache::cache_root()
            .and_then(|root| Store::open(&root.join("controller.sqlite")).ok())
            .map(Arc::new);
        Net::with_store(store)
    }

    /// With no response cache, so a test reads and writes nothing of the user's.
    #[cfg(test)]
    pub fn uncached_for_tests() -> Net {
        Net::with_store(None)
    }

    fn with_store(store: Option<Arc<Store>>) -> Net {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("crucible-playbooks-net")
            .enable_all()
            .build()
            .map(ManuallyDrop::new)
            .map_err(|error| format!("starting the controller client: {error}"));
        Net { runtime, store }
    }

    pub fn handle(&self) -> Result<Handle, String> {
        match &self.runtime {
            Ok(runtime) => Ok(runtime.handle().clone()),
            Err(why) => Err(why.clone()),
        }
    }
}

impl Net {
    /// `target`'s connection, read through the response cache.
    pub fn connector(&self, target: Target) -> Connector {
        Connector::new(target, self.handle(), self.store.clone())
    }

    /// `target`'s connection with nothing cached.
    pub fn uncached(&self, target: Target) -> Connector {
        Connector::new(target, self.handle(), None)
    }
}

/// One profile's connection, opened on first use and shared by every clone.
#[derive(Clone)]
pub struct Connector {
    target: Target,
    runtime: Result<Handle, String>,
    store: Option<Arc<Store>>,
    offline: Arc<AtomicBool>,
    open: Arc<Mutex<Option<Connection>>>,
}

impl Connector {
    fn new(target: Target, runtime: Result<Handle, String>, store: Option<Arc<Store>>) -> Self {
        Connector {
            target,
            runtime,
            store,
            offline: Arc::default(),
            open: Arc::default(),
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn runtime(&self) -> Result<Handle, String> {
        self.runtime.clone()
    }

    pub fn store(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }

    /// Whether answers are coming from the cache because the controller could not be reached.
    pub fn offline(&self) -> bool {
        self.offline.load(Ordering::Relaxed)
    }

    pub fn connection(&self) -> Result<Connection, String> {
        let mut open = self
            .open
            .lock()
            .map_err(|_| "the controller connection is poisoned".to_string())?;
        if let Some(connection) = open.as_ref() {
            return Ok(connection.clone());
        }
        self.target.ready()?;
        let runtime = self.runtime.clone()?;
        let config = self.target.config()?;
        let wire = crux::client::HttpTransport::with_client(&config, self.target.http()?)
            .map_err(|error| format!("{error:#}"))?;
        let acting = self.target.act_as().cloned();
        let cached = self.store.as_ref().filter(|_| acting.is_none());
        let wire = match acting {
            Some(act_as) => wire.acting_as(act_as),
            None => wire,
        };
        #[cfg(not(target_arch = "wasm32"))]
        let transport: Arc<dyn crux::client::Transport> = match cached {
            Some(store) => Arc::new(CachedTransport::new(
                Arc::new(wire),
                Arc::clone(store),
                Arc::clone(&self.offline),
            )),
            None => Arc::new(wire),
        };
        #[cfg(target_arch = "wasm32")]
        let transport: Arc<dyn crux::client::Transport> = {
            let _ = cached;
            Arc::new(wire)
        };
        let connection = Connection {
            client: crux::Client::new(transport),
            runtime,
        };
        *open = Some(connection.clone());
        Ok(connection)
    }

    /// The connection, opened off the runtime's own threads when it is not open yet.
    pub async fn connect(&self) -> Result<Connection, String> {
        let open = self.open.lock().ok().and_then(|open| open.clone());
        if let Some(connection) = open {
            return Ok(connection);
        }
        let connector = self.clone();
        blocking(move || connector.connection())
            .await
            .ok_or_else(|| "opening the connection stopped early".to_string())?
    }

    /// Forget the open connection, so the next one reads the profile's key afresh.
    pub fn reset(&self) {
        if let Ok(mut open) = self.open.lock() {
            *open = None;
        }
    }
}

/// The crux client and the runtime its calls run on.
#[derive(Clone)]
pub struct Connection {
    pub client: crux::Client,
    runtime: Handle,
}

impl Connection {
    /// Run one call to its end or [`TIMEOUT`], whichever comes first. Blocks, so not for a
    /// runtime thread.
    #[cfg(test)]
    pub fn call<T, E: std::fmt::Display>(
        &self,
        what: &str,
        call: impl Future<Output = Result<T, E>>,
    ) -> Result<T, String> {
        self.runtime.block_on(within(what, call))
    }

    /// Run `work` to its end. Blocks, so not for a runtime thread.
    #[cfg(test)]
    pub fn wait<F: Future>(&self, work: F) -> F::Output {
        self.runtime.block_on(work)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(task);
    }

    #[cfg(target_arch = "wasm32")]
    pub fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        self.runtime.spawn(task);
    }
}

/// A task on the shared runtime that ends when this is dropped.
#[cfg(not(target_arch = "wasm32"))]
pub struct Task(tokio::task::AbortHandle);

#[cfg(not(target_arch = "wasm32"))]
impl Task {
    pub fn spawn(runtime: &Handle, work: impl Future<Output = ()> + Send + 'static) -> Task {
        Task(runtime.spawn(work).abort_handle())
    }
}

#[cfg(target_arch = "wasm32")]
pub struct Task(futures_util::future::AbortHandle);

#[cfg(target_arch = "wasm32")]
impl Task {
    pub fn spawn(runtime: &Handle, work: impl Future<Output = ()> + 'static) -> Task {
        let (work, handle) = futures_util::future::abortable(work);
        runtime.spawn(async move {
            let _ = work.await;
        });
        Task(handle)
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `work` on the blocking pool, or in place in the browser. `None` when it stopped early.
#[cfg(not(target_arch = "wasm32"))]
pub async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(work).await.ok()
}

#[cfg(target_arch = "wasm32")]
pub async fn blocking<T: 'static>(work: impl FnOnce() -> T + 'static) -> Option<T> {
    Some(work())
}

pub struct Elapsed;

/// `work`'s answer, or [`Elapsed`] when `limit` passed first.
#[cfg(not(target_arch = "wasm32"))]
pub async fn timeout<F: Future>(limit: Duration, work: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(limit, work).await.map_err(|_| Elapsed)
}

#[cfg(target_arch = "wasm32")]
pub async fn timeout<F: Future>(limit: Duration, work: F) -> Result<F::Output, Elapsed> {
    tokio::select! {
        answer = work => Ok(answer),
        () = sleep(limit) => Err(Elapsed),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(wait: Duration) {
    tokio::time::sleep(wait).await;
}

#[cfg(target_arch = "wasm32")]
pub async fn sleep(wait: Duration) {
    let millis = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
    gloo_timers::future::TimeoutFuture::new(millis).await;
}

/// Why a controller cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Down {
    /// Nothing answered at its address.
    Unreached(String),
    /// It answered and refused, or there is no key to ask it with.
    Refused(String),
}

impl Down {
    pub fn why(&self) -> &str {
        match self {
            Down::Unreached(why) | Down::Refused(why) => why,
        }
    }
}

impl From<String> for Down {
    fn from(why: String) -> Self {
        Down::Refused(why)
    }
}

/// `call`'s answer, or how it failed: a request that never got an answer is
/// [`Down::Unreached`] with its root cause, anything else [`Down::Refused`].
pub async fn reaching<T>(
    what: &str,
    call: impl Future<Output = anyhow::Result<T>>,
) -> Result<T, Down> {
    match timeout(TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            let unsent = error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
                .any(unsent);
            Err(if unsent {
                Down::Unreached(error.root_cause().to_string())
            } else {
                Down::Refused(format!("{error:#}"))
            })
        }
        Err(_) => Err(Down::Unreached(format!(
            "{what} timed out after {}s",
            TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn unsent(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

#[cfg(target_arch = "wasm32")]
fn unsent(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_request()
}

/// `call`'s answer, or that it timed out after [`TIMEOUT`].
pub async fn within<T, E: std::fmt::Display>(
    what: &str,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match timeout(TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(_) => Err(format!("{what} timed out after {}s", TIMEOUT.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use crux::client::Credential;
    use crux::config::Auth;

    use crate::net::{Down, Net, reaching};
    use crate::profile::{Profile, Target};
    use crate::testserver::{serve, target};

    #[test]
    fn a_target_acting_as_a_team_says_so_on_every_request_and_keeps_a_refusal() {
        use crate::testserver::{Reply, serve_replies};

        let (url, _) = serve_replies(|asked| match asked.header("x-crucible-act-as") {
            Some("team:closed") => Reply {
                status: 403,
                headers: vec![("x-crucible-act-as", "refused".into())],
                body: r#"{"error":"you are not a member of team:closed"}"#.into(),
            },
            Some(team) => Reply::ok(format!(r#"{{"user":"{team}"}}"#)),
            None => Reply::ok(r#"{"user":"wynn"}"#.into()),
        });
        let keys = tempfile::tempdir().unwrap();
        let net = Net::uncached_for_tests();
        let whoami = |target: &Target| {
            let c = net.connector(target.clone()).connection().unwrap();
            c.call::<serde_json::Value, _>("whoami", c.client.get("/api/whoami"))
        };

        let me = target("t", &url, keys.path());
        assert_eq!(whoami(&me).unwrap()["user"], "wynn");

        let infra = me.clone().acting_as(Some("team:infra".into()));
        assert_eq!(whoami(&infra).unwrap()["user"], "team:infra");
        assert_eq!(infra.act_as().and_then(|a| a.refused()), None);

        let closed = me.clone().acting_as(Some("team:closed".into()));
        let refusal = whoami(&closed).unwrap_err();
        assert!(refusal.contains("not a member"), "{refusal}");
        assert_eq!(
            closed.act_as().and_then(|a| a.refused()).as_deref(),
            Some("you are not a member of team:closed")
        );
        assert!(closed.acting_as(None).act_as().is_none());
    }

    #[test]
    fn every_clone_of_a_connector_uses_the_one_connection() {
        let (url, accepted) = serve(|_| "{}".into());
        let keys = tempfile::tempdir().unwrap();
        let net = Net::uncached_for_tests();
        let connector = net.connector(target("t", &url, keys.path()));
        for _ in 0..4 {
            let worker = connector.clone();
            std::thread::spawn(move || {
                let c = worker.connection().unwrap();
                let _: serde_json::Value = c.call("whoami", c.client.get("/api/whoami")).unwrap();
            })
            .join()
            .unwrap();
            let c = connector.connection().unwrap();
            let _: serde_json::Value = c.call("whoami", c.client.get("/api/whoami")).unwrap();
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "eight calls from five threads go over one pooled connection"
        );
    }

    #[test]
    fn a_controller_that_never_answers_is_unreached_and_one_that_answers_badly_is_not() {
        let keys = tempfile::tempdir().unwrap();
        let net = Net::uncached_for_tests();
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let c = net
            .connector(target("gone", &closed, keys.path()))
            .connection()
            .unwrap();
        let gone = c.wait(reaching("whoami", c.client.whoami())).unwrap_err();
        assert!(matches!(gone, Down::Unreached(_)), "{gone:?}");
        assert!(
            !gone.why().contains("/api/whoami"),
            "the root cause only: {gone:?}"
        );

        let (url, _) = serve(|_| "nope".into());
        let c = net
            .connector(target("odd", &url, keys.path()))
            .connection()
            .unwrap();
        let odd = c.wait(reaching("whoami", c.client.whoami())).unwrap_err();
        assert!(matches!(odd, Down::Refused(_)), "{odd:?}");
        assert_eq!(
            Down::from("no key".to_string()),
            Down::Refused("no key".into())
        );
    }

    #[test]
    fn a_reset_connector_reads_the_key_again() {
        let (url, _) = serve(|_| "{}".into());
        let keys = tempfile::tempdir().unwrap();
        let net = Net::uncached_for_tests();
        let target = target("t", &url, keys.path());
        let connector = net.connector(target.clone());
        let before = connector.connection().unwrap();
        assert_eq!(before.client.credential(), Credential::Wire(Auth::None));
        target.save_key("crk_new".into()).unwrap();
        let same = connector.connection().unwrap();
        assert_eq!(
            same.client.credential(),
            Credential::Wire(Auth::None),
            "an open connection keeps the key it was opened with"
        );
        connector.reset();
        let after = connector.connection().unwrap();
        assert_eq!(
            after.client.credential(),
            Credential::Wire(Auth::ApiKey {
                token: "crk_new".into()
            })
        );
    }

    #[test]
    fn a_call_still_answers_after_the_net_is_dropped() {
        let (url, _) = serve(|_| "{}".into());
        let keys = tempfile::tempdir().unwrap();
        let net = Net::uncached_for_tests();
        let c = net
            .connector(target("t", &url, keys.path()))
            .connection()
            .unwrap();
        drop(net);
        let answer: serde_json::Value = c.call("whoami", c.client.get("/api/whoami")).unwrap();
        assert_eq!(answer, serde_json::json!({}));
    }

    #[test]
    fn a_profile_that_is_not_ready_opens_nothing() {
        let net = Net::uncached_for_tests();
        let keys = tempfile::tempdir().unwrap();
        let mut profile = Profile {
            url: "http://127.0.0.1:9".into(),
            ..Profile::default()
        };
        profile.tunnel = Some("exit 7".into());
        let target = Target::new("t", profile, &crate::keys::Store::Dir(keys.path().into()));
        let _keeper = target.start(keys.path(), || {
            Err(std::io::Error::other("no supervisor here"))
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(target.readiness(), crate::profile::Readiness::Failed(_)) {
            assert!(std::time::Instant::now() < until, "the start never failed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let why = net.connector(target).connection().err().unwrap();
        assert!(why.contains("no supervisor here"), "{why}");
    }
}
