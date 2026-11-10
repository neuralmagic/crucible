//! What the window holds for the profile in use: its workers, what they last read, and what is
//! half done against it. Switching profile replaces it whole, so nothing of the last one stays.

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

    /// A key pasted for the profile, for one the browser cannot reach.
    pub key_paste: TextEdit,
    /// How the last browser sign-in went.
    pub signing_in: bool,
    pub sign_in_error: Option<String>,
    pub sign_in_done: Option<Receiver<Result<(), String>>>,
    /// Stops the sign-in that is waiting on the browser.
    pub sign_in_cancel: Option<crate::signin::Cancel>,
}

impl Remote {
    /// Fresh workers on `target`, with nothing read yet.
    pub fn new(target: Target, net: &Net, waker: Option<&Waker>) -> Remote {
        let connector = net.connector(target.clone());
        let remote = Remote {
            target,
            live_client: live::Client::spawn(connector.clone()),
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
            working: None,
            working_pack: None,
            working_preview: None,
            working_edited: false,
            working_watch: crate::draft::Watcher::start(net.handle(), crate::draft::root()),
            working_sent: None,
            working_seen: 0,
            graph_source: None,
            draft_busy: false,
            draft_after: 0,
            key_paste: TextEdit::new(""),
            signing_in: false,
            sign_in_error: None,
            sign_in_done: None,
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
}
