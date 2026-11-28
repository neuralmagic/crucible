//! Starting the app: its first state, its workers, and the poll that folds their answers in.

#[cfg(target_os = "linux")]
use crate::instance;
use crate::link::Link;
use crate::logline::LogView;
use crate::model::Client;
use crate::settings::{Settings, Theme};
use crate::workspace::{Geometry, PaneKind};
use crate::{
    App, Backend, LogSource, LoginEditor, Page, RunView, RunsTab, SETTINGS, StandIn, fleet,
    history, ignition, keymap, lsp, model, net, notify, profile, remote, report, settings, update,
};
use rune::{TextEdit, Tx, Waker};
use std::collections::HashMap;
use std::path::PathBuf;

fn repo_from_args() -> Option<PathBuf> {
    let args = rune::extra_args();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--repo" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix("--repo=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

pub(crate) fn init() -> App {
    let repo = repo_from_args().unwrap_or_else(model::default_repo);
    let settings = SETTINGS.get().cloned().unwrap_or_else(Settings::load);
    let (keymap, keys_refused) = keymap::Keymap::new(&settings.keys);
    let editor_command = TextEdit::new(&settings.editor_command);
    let drop_cost = TextEdit::new(format!("{}", settings.drop_max_cost));
    let drop_time = TextEdit::new(&settings.drop_max_time);
    let drafts_dir = TextEdit::new(
        settings
            .drafts_dir
            .as_deref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    );
    let (opened_tx, opened) = std::sync::mpsc::channel();
    let open_wake: std::sync::Arc<std::sync::Mutex<Option<Waker>>> = Default::default();
    let woken = open_wake.clone();
    rune_mac::files::on_open(move |paths| {
        let _ = opened_tx.send(paths);
        if let Some(wake) = woken.lock().ok().and_then(|w| w.clone()) {
            wake();
        }
    });
    let (name, chosen) = profile::pick(&settings);
    let target =
        profile::Target::new(&name, chosen, &settings.keys()).acting_as(settings.act_as(&name));
    let keeper = target.start(&repo, profile::supervise::supervisor);
    let net = net::Net::start();
    let remote = remote::Remote::new(target.clone(), &net, settings.drafts_root(), None);
    App {
        repo: repo.display().to_string(),
        remote,
        run: RunView::default(),
        local: Client::spawn(repo),
        local_seen: 0,
        lsp: lsp::Lsp::spawn(),
        lsp_seen: 0,
        checks: lsp::View::default(),
        lsp_sent: None,
        editor_error: None,
        backend: Backend::Controller,
        packs: Vec::new(),
        playbook_search: TextEdit::new(""),
        filters_open: false,
        selection: None,
        log_search: TextEdit::new(""),
        log_view: LogView::All,
        log_source: LogSource::Transcript,
        log_chatter: false,
        log_mode: None,
        keymap,
        keys_refused,
        palette: None,
        pane_focus: None,
        window_focused: true,
        notifier: None,
        rerun_wanted: None,
        clicks: None,
        recorders: HashMap::new(),
        provider_scratch: rune_kit::ComboboxState::default(),
        layout: settings.layout.clone(),
        shown: Vec::new(),
        pane_drag: None,
        resizing: None,
        rail_drag: None,
        geometry: Geometry::default(),
        ignition: ignition::Ignition::default(),
        afterglows: Default::default(),
        runs_unfolded: Default::default(),
        logo_clicks: 0,
        logo_screen: [0.0; 2],
        targets: HashMap::from([(name, (target, keeper))]),
        profile_form: None,
        keys_error: None,
        source_menu: false,
        panes_menu: false,
        tray: None,
        fleet: Vec::new(),
        fleet_client: fleet::Client::spawn(net.handle()),
        net,
        fleet_seen: 0,
        fleet_sent: Vec::new(),
        approvals_known: HashMap::new(),
        picks: None,
        opened: Some(opened),
        open_wake,
        history: history::History::default(),
        waker: None,
        release: update::Check::default(),
        scratch: TextEdit::new(""),
        #[cfg(not(target_arch = "wasm32"))]
        forge: Default::default(),
        #[cfg(not(target_arch = "wasm32"))]
        forge_offers: Default::default(),
        #[cfg(not(target_arch = "wasm32"))]
        promoting: None,
        #[cfg(not(target_arch = "wasm32"))]
        promote_form: Default::default(),
        settings,
        editor_command,
        drop_cost,
        drop_time,
        drop_error: None,
        drafts_dir,
        drafts_error: None,
        drop_failed: None,
        login_editor: LoginEditor::Unasked,
        opening: None,
        pack_md_source: false,
        run_file: None,
        files_md_source: false,
        files_html_source: false,
        html_preview_error: None,
        report_shown: report::shown::Scoped::default(),
        report_sent: None,
        local_report: None,
        runs_shown: report::shown::Scoped::default(),
        runs_tab: RunsTab::default(),
        view_plugin: std::env::var("CRUCIBLE_VIEW_PLUGIN")
            .ok()
            .and_then(|named| StandIn::read(&named)),
        knobs: rune_knobs::Knobs::default(),
        page: Page::Runs,
        flight: None,
        local_error: None,
        act_as_refused: None,
    }
}

pub(crate) fn setup(app: &mut App, waker: Waker) {
    app.waker = Some(waker.clone());
    let (clicked, clicks) = std::sync::mpsc::channel();
    #[cfg(target_os = "linux")]
    {
        instance::serve(clicked.clone(), waker.clone());
        if let Some(link) = instance::launch_link(rune::extra_args()) {
            let _ = clicked.send(notify::Clicked::Open(link));
        }
    }
    app.notifier = Some(notify::Notifier::start(clicked, waker.clone()));
    app.clicks = Some(clicks);
    let (picked, picks) = std::sync::mpsc::channel();
    let pick_waker = waker.clone();
    rune_mac::status::on_pick(move |id| {
        let _ = picked.send(id);
        pick_waker();
    });
    app.picks = Some(picks);
    if let Ok(mut wake) = app.open_wake.lock() {
        *wake = Some(waker.clone());
    }
    app.local.set_waker(waker.clone());
    app.remote.set_waker(&waker);
    app.fleet_client.set_waker(waker.clone());
    app.lsp.set_waker(waker);
    app.check_release();
}

/// Nothing here touches a file, a process or the network: the workers own all of that and wake the
/// loop once per finished snapshot.
pub(crate) fn poll(app: &mut App, clock: f64, tx: &mut Tx) -> bool {
    let mut changed = app.ignition.tick(clock);
    if app.afterglows.tick(clock) {
        tx.animated();
        changed = true;
    }
    if let Some((version, snapshot)) = app.local.take(app.local_seen) {
        app.local_seen = version;
        app.apply_local(snapshot);
        changed = true;
    }
    if app.heed_act_as_refusal() {
        changed = true;
    }
    if let Some((version, snapshot)) = app.remote.live_client.take(app.remote.live_seen) {
        app.remote.live_seen = version;
        app.apply_live(snapshot);
        changed = true;
    }
    if let Some((version, found)) = app.remote.finder_client.take(app.remote.finder_seen) {
        app.remote.finder_seen = version;
        app.apply_found(found);
        changed = true;
    }
    if let Some((version, view)) = app.remote.admin_client.take(app.remote.admin_seen) {
        app.remote.admin_seen = version;
        app.apply_admin(view);
        changed = true;
    }
    while let Some(clicked) = app
        .clicks
        .as_ref()
        .and_then(|clicks| clicks.try_recv().ok())
    {
        match clicked {
            notify::Clicked::Open(url) => {
                if let Some(link) = Link::parse(&url) {
                    app.follow(&link);
                    notify::bring_to_front();
                }
            }
            notify::Clicked::Answer(answer) => app.answer_from_note(answer),
            notify::Clicked::Run { link, action } => app.press_run_button(&link, action),
        }
        changed = true;
    }
    while let Some(paths) = app
        .opened
        .as_ref()
        .and_then(|opened| opened.try_recv().ok())
    {
        app.open_paths(paths);
        changed = true;
    }
    while let Some(id) = app.picks.as_ref().and_then(|picks| picks.try_recv().ok()) {
        app.picked(&id);
        changed = true;
    }
    if let Some((version, statuses)) = app.fleet_client.take(app.fleet_seen) {
        app.fleet_seen = version;
        app.fleet = statuses;
        changed = true;
    }
    if let Some(wait) = app.tick_reports(clock) {
        tx.wake_after(std::time::Duration::try_from_secs_f64(wait).unwrap_or_default());
    }
    changed |= app.rerun_when_read();
    app.sync_live();
    app.sync_report();
    app.sync_admin();
    app.sync_fleet();
    app.sync_tray();
    let place = app.place();
    app.history.saw(place);
    changed |= app.refresh_working();
    changed |= app.refresh_agent();
    changed |= app.finish_sign_in();
    #[cfg(not(target_arch = "wasm32"))]
    {
        changed |= app.follow_forge_accounts();
        changed |= app.follow_promotion();
    }
    changed |= app.finish_login_editor();
    changed |= app.finish_release_check();
    if let Some((version, view)) = app.lsp.take(app.lsp_seen) {
        app.lsp_seen = version;
        app.checks = view;
        changed = true;
    }
    app.sync_checks();
    for due in [app.ignition.due(clock), app.afterglows.due(clock)]
        .into_iter()
        .flatten()
    {
        tx.wake_at(due);
    }
    changed
}

/// Shows the pane `CRUCIBLE_TAB` names in the `CRUCIBLE_THEME` theme, for headless scenes,
/// without saving either.
pub(crate) fn scene_tab(app: &mut App) {
    let theme = std::env::var("CRUCIBLE_THEME").unwrap_or_default();
    if let Some(theme) = Theme::ALL
        .into_iter()
        .find(|t| t.label().eq_ignore_ascii_case(&theme))
    {
        app.settings.theme = theme;
    }
    let size = std::env::var("CRUCIBLE_TEXT").unwrap_or_default();
    if let Some(size) = settings::TextSize::ALL
        .into_iter()
        .find(|s| s.label().eq_ignore_ascii_case(&size))
    {
        app.settings.text_size = size;
    }
    app.run_file = std::env::var("CRUCIBLE_FILE").ok();
    let tab = std::env::var("CRUCIBLE_TAB").unwrap_or_default();
    if let Some(kind) = PaneKind::ALL
        .into_iter()
        .find(|kind| kind.label().eq_ignore_ascii_case(&tab))
    {
        app.settings.view_plugins |= kind == PaneKind::Report;
        match &mut app.layout {
            Some(layout) => layout.activate(kind),
            None => app.shown.push(kind),
        }
    }
}

/// A link the system opened with the app. It comes from outside, so anything that does not
/// parse is dropped.
#[cfg(not(feature = "hot"))]
pub(crate) fn open_link(app: &mut App, url: &str, _tx: &mut Tx) {
    if let Some(link) = Link::parse(url) {
        app.follow(&link);
        notify::bring_to_front();
    }
}
