//! One app per desktop session on Linux. The desktop opens a `crucible-playbooks://` link by
//! starting the app with the link as an argument; a second process hands the link to the one
//! already running over the session bus and exits.

use std::sync::mpsc::Sender;

use rune::Waker;

const NAME: &str = "dev.crucible.Playbooks";
const PATH: &str = "/dev/crucible/Playbooks";

/// The link the app was started with, if any.
pub fn launch_link(args: &[String]) -> Option<String> {
    args.iter()
        .find(|arg| arg.starts_with("crucible-playbooks://") || arg.starts_with("crucible://"))
        .cloned()
}

/// Hand `link` to the app already running; false when none answered.
pub fn forward(link: &str) -> bool {
    current_thread(async {
        let bus = zbus::Connection::session().await?;
        bus.call_method(Some(NAME), PATH, Some(NAME), "Open", &(link,))
            .await?;
        Ok(())
    })
    .is_some_and(|sent: zbus::Result<()>| sent.is_ok())
}

/// Take the bus name and send each link another process hands over to `links`, waking the app.
/// Holds the name until the process exits.
pub fn serve(links: Sender<String>, waker: Waker) {
    let _ = std::thread::Builder::new()
        .name("crucible-playbooks-instance".into())
        .spawn(move || {
            current_thread(async move {
                let _bus = zbus::connection::Builder::session()
                    .name(NAME)
                    .serve_at(PATH, Opener { links, waker })
                    .build()
                    .await?;
                std::future::pending::<zbus::Result<()>>().await
            });
        });
}

fn current_thread<T>(work: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|runtime| runtime.block_on(work))
}

struct Opener {
    links: Sender<String>,
    waker: Waker,
}

#[zbus::interface(name = "dev.crucible.Playbooks")]
impl Opener {
    fn open(&self, link: String) {
        if self.links.send(link).is_ok() {
            (self.waker)();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use crate::instance::{forward, launch_link, serve};

    #[test]
    fn the_launch_link_is_the_first_argument_in_an_app_scheme() {
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            launch_link(&args(&[
                "--repo",
                "/src",
                "crucible-playbooks://playbook/p"
            ])),
            Some("crucible-playbooks://playbook/p".to_string())
        );
        assert_eq!(
            launch_link(&args(&[
                "crucible://mpp/launches/k",
                "crucible-playbooks://x"
            ])),
            Some("crucible://mpp/launches/k".to_string())
        );
        assert_eq!(launch_link(&args(&["--repo", "https://example.com"])), None);
    }

    /// Needs a session bus: `dbus-run-session -- cargo test`.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the session bus hand-off is Linux's"
    )]
    fn a_second_launch_hands_its_link_to_the_running_app() {
        assert!(
            std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
            "run under dbus-run-session"
        );
        assert!(
            !forward("crucible-playbooks://before"),
            "nothing listens yet"
        );
        let (links, received) = std::sync::mpsc::channel();
        let woken = Arc::new(AtomicUsize::new(0));
        let count = woken.clone();
        serve(
            links,
            Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let until = Instant::now() + Duration::from_secs(10);
        while !forward("crucible-playbooks://playbook/p") {
            assert!(Instant::now() < until, "the app never took the bus name");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            "crucible-playbooks://playbook/p"
        );
        assert_eq!(woken.load(Ordering::SeqCst), 1);
    }
}
