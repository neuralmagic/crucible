//! What the window holds for the profile in use: its workers, what they last read, and what is
//! half done against it. Switching profile replaces it whole, so nothing of the last one stays.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use rune::{TextEdit, Waker};

use crate::draft::Working;
use crate::live::{LiveDetail, LiveView};
use crate::net::Net;
use crate::pack::PackFiles;
use crate::profile::Target;
use crate::{GraphSource, Launcher, admin, captured, finder, live, preview};

pub struct Remote {
    pub target: Target,
    pub live_client: live::Client,
    pub live_seen: u64,
    pub live: Arc<LiveView>,
    pub live_detail: Option<Arc<LiveDetail>>,
    /// The launch key the live worker was last told to read.
    pub live_sent: Option<String>,
    /// The launch the Compare pane reads against, as last asked of the worker.
    pub compare_sent: Option<String>,
    /// That launch's run, once read.
    pub live_base: Option<live::Base>,
    /// Playbooks the controller has registered: the ones a launch form can start.
    pub registered: Vec<String>,
    /// The open launch form, when there is one.
    pub launcher: Option<Launcher>,
    /// The newest worker reply a launch form has taken.
    pub replies: u64,
    /// The newest finished launch already announced.
    pub notified: u64,
    /// The launch whose budget the Dock icon shows.
    pub pinned: Option<String>,
    /// The newest drop reply taken.
    pub dropped_seen: u64,

    pub admin_client: admin::Client,
    pub admin_seen: u64,
    pub admin_sent: Option<admin::Watch>,
    /// Schedules, secrets, shares and decision requests as last read.
    pub admin: admin::AdminView,
    /// The management pages' picks and half-filled forms.
    pub desk: admin::Desk,

    /// The run finder, while open.
    pub finder: Option<finder::Finder>,
    pub finder_client: finder::Client,
    pub finder_seen: u64,

    /// The captured file last asked of the live worker, by run, key and listed size.
    pub file_sent: Option<(String, String, u64)>,
    /// The captured file the live worker last fetched.
    pub opened_file: Option<captured::Opened>,
    /// The fetched file read as a patch, when it is one.
    pub file_diff: rune_kit::diff::DiffState,

    pub revisions_sent: Option<crate::revisions::Wanted>,
    pub revisions: Option<Arc<crate::revisions::Revisions>>,
    pub revisions_diff: rune_kit::diff::DiffState,

    /// The working copy of the draft the shown pack edits, and its files as last read.
    pub working: Option<Working>,
    pub working_pack: Option<PackFiles>,
    /// The working copy as it compiles now.
    pub working_preview: Option<preview::Preview>,
    pub working_edited: bool,
    /// Reads the working copy, the draft it was last pointed at, and its newest read taken.
    pub working_watch: crate::draft::Watcher,
    pub working_sent: Option<String>,
    pub working_seen: u64,
    /// Whether the run views show the run or the pack as it compiles now; `None` shows the run
    /// when there is one.
    pub graph_source: Option<GraphSource>,
    /// A pull or save is on its way, and only replies newer than `draft_after` answer it.
    pub draft_busy: bool,
    pub draft_after: u64,
    /// The drafting agent's pane and its session.
    pub agent: crate::agent::Desk,

    /// A key pasted for the profile, for one the browser cannot reach.
    pub key_paste: TextEdit,
    /// How the last browser sign-in went.
    pub signing_in: bool,
    pub sign_in_error: Option<String>,
    pub sign_in_done: Option<Receiver<Result<(), String>>>,
    /// Stops the sign-in that is waiting on the browser.
    #[cfg(not(target_arch = "wasm32"))]
    pub sign_in_cancel: Option<crate::signin::Cancel>,
}

impl Remote {
    /// Fresh workers on `target`, with nothing read yet.
    pub fn new(
        target: Target,
        net: &Net,
        drafts: Option<PathBuf>,
        waker: Option<&Waker>,
    ) -> Remote {
        let connector = net.connector(target.clone());
        let remote = Remote {
            target,
            live_client: live::Client::spawn(connector.clone(), drafts.clone()),
            live_seen: 0,
            live: Arc::default(),
            live_detail: None,
            live_sent: None,
            compare_sent: None,
            live_base: None,
            registered: Vec::new(),
            launcher: None,
            replies: 0,
            notified: 0,
            pinned: None,
            dropped_seen: 0,
            admin_client: admin::Client::spawn(connector.clone()),
            admin_seen: 0,
            admin_sent: None,
            admin: admin::AdminView::default(),
            desk: admin::Desk::default(),
            finder: None,
            finder_client: finder::Client::spawn(connector),
            finder_seen: 0,
            file_sent: None,
            opened_file: None,
            file_diff: rune_kit::diff::DiffState::default(),
            revisions_sent: None,
            revisions: None,
            revisions_diff: rune_kit::diff::DiffState::default(),
            working: None,
            working_pack: None,
            working_preview: None,
            working_edited: false,
            working_watch: crate::draft::Watcher::start(net.handle(), drafts),
            working_sent: None,
            working_seen: 0,
            graph_source: None,
            draft_busy: false,
            draft_after: 0,
            agent: crate::agent::Desk::default(),
            key_paste: TextEdit::new(""),
            signing_in: false,
            sign_in_error: None,
            sign_in_done: None,
            #[cfg(not(target_arch = "wasm32"))]
            sign_in_cancel: None,
        };
        if let Some(waker) = waker {
            remote.set_waker(waker);
        }
        remote
    }

    pub fn set_waker(&self, waker: &Waker) {
        self.live_client.set_waker(waker.clone());
        self.admin_client.set_waker(waker.clone());
        self.finder_client.set_waker(waker.clone());
        self.working_watch.set_waker(waker.clone());
    }

    /// Keep working copies under `drafts` from now on. The copy shown is read again from there.
    pub fn set_drafts(&mut self, net: &Net, drafts: Option<PathBuf>, waker: Option<&Waker>) {
        self.live_client.set_drafts(drafts.clone());
        self.working_watch = crate::draft::Watcher::start(net.handle(), drafts);
        if let Some(waker) = waker {
            self.working_watch.set_waker(waker.clone());
        }
        self.working = None;
        self.working_pack = None;
        self.working_preview = None;
        self.working_edited = false;
        self.working_sent = None;
        self.working_seen = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use crate::draft::Working;
    use crate::remote::Remote;

    #[test]
    fn working_copies_are_read_from_the_folder_last_set() {
        let (url, _) = crate::testserver::serve(|_| "[]".into());
        let keys = tempfile::tempdir().unwrap();
        let (before, after) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let pack = BTreeMap::from([("crucible.toml".to_string(), "[workflow]\n".to_string())]);
        let old = Working::write(before.path(), "triage-me", 4, &pack).unwrap();
        let new = Working::write(after.path(), "triage-me", 7, &pack).unwrap();

        let net = crate::net::Net::uncached_for_tests();
        let target = crate::testserver::target("t", &url, keys.path());
        let mut remote = Remote::new(target, &net, Some(before.path().to_path_buf()), None);
        let read = |remote: &mut Remote| {
            remote.working_watch.watch(Some("triage-me"));
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some((version, Some(copy))) = remote.working_watch.take(remote.working_seen)
                {
                    remote.working_seen = version;
                    return copy.working.clone();
                }
                assert!(Instant::now() < until, "no working copy was read");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(read(&mut remote), old);
        remote.working = Some(old);
        remote.working_edited = true;
        remote.working_sent = Some("triage-me".into());

        remote.set_drafts(&net, Some(after.path().to_path_buf()), None);
        assert_eq!(remote.working, None);
        assert!(!remote.working_edited);
        assert_eq!(
            (remote.working_sent.as_deref(), remote.working_seen),
            (None, 0)
        );
        assert_eq!(read(&mut remote), new);
    }
}
