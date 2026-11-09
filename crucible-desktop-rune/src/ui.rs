//! The window: live playbooks and local packs on the left, the selected run on the right.

mod compare;
mod data;
mod decisions;
mod desk;
mod dock;
mod graph;
mod launch;
mod launcher;
mod log;
mod pack;
mod palette;
mod runs;
mod schedules;
mod secrets;
mod shares;
mod tasks;
mod theme;
mod timeline;
mod transcript;

use rune::{Color, Cx, El, Icon, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::shell::RowStatus;
use rune_kit::{
    Badge, Button, ButtonVariant, IconButton, IconButtonVariant, Kit, Palette, SegmentedControl,
    SidebarRow, Spinner, StateColors, Switch, TextInput,
};
use rune_theme_opencode::icons as oc;

use crate::editor::Editor;
use crate::filter::{LandedFilter, OriginFilter, OwnerFilter};
use crate::live::{LiveDetail, LivePlaybook, LiveView};
use crate::logline::{LogLine, activity};
use crate::model::{PackView, Verdict};
use crate::pack::PackFiles;
use crate::session::{TaskState, TaskView};
use crate::settings::{Appearance, LogFont, Theme};
use crate::{App, Backend, MAX_TIME, Selection, gfx};
use crucible_contract::session::TaskStatus;

const SIDEBAR: f32 = 300.0;
/// The band across the top that holds the window buttons and drags the window.
const TITLE_BAND: f32 = 52.0;
/// The close button's top-left, centring the buttons in the title band.
#[cfg(not(feature = "hot"))]
pub const TRAFFIC_LIGHTS: (f32, f32) = (18.0, 19.0);
const SIDEBAR_PAD: f32 = 16.0;
const ROW: f32 = 46.0;

const TITLE: TextStyle = TextStyle::new(20.0, 640.0).lh(26.0).tracking(-0.2);
const BRAND: TextStyle = TextStyle::new(15.0, 640.0).lh(20.0);
const SECTION: TextStyle = TextStyle::new(13.0, 560.0).lh(16.0);
const VALUE: TextStyle = TextStyle::new(18.0, 600.0).lh(24.0).features("'tnum' 1");

struct Paint {
    k: &'static Kit,
    c: &'static Palette,
    dark: bool,
    accent: Color,
    pass: Color,
    fail: Color,
    live: Color,
    hair: f32,
    /// Points the window buttons take at the leading edge; 0 in full screen.
    inset: f32,
}

/// A status role with its own fill, ink and border: passing, needing attention, or failing.
#[derive(Clone, Copy)]
enum Tone {
    Success,
    Warning,
    Danger,
}

impl Paint {
    fn tone(&self, tone: Tone) -> &StateColors {
        match tone {
            Tone::Success => &self.c.success,
            Tone::Warning => &self.c.warning,
            Tone::Danger => &self.c.danger,
        }
    }
}

fn palette(app: &App, cx: &Cx<App>) -> Paint {
    let dark = app.settings.appearance.dark(cx.system_dark());
    let k = theme::kit(app.settings.theme, dark);
    let c = &k.c;
    Paint {
        k,
        c,
        dark,
        accent: c.text_accent,
        pass: c.success.fg,
        fail: c.danger.fg,
        live: c.warning.fg,
        hair: 1.0 / cx.env().scale,
        inset: cx.titlebar_inset(),
    }
}

fn mono(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight).mono()
}

/// An effect slot covering its parent, painted by `crate::gfx` as effect `kind`.
fn fx(kind: u32, data: [f32; 4], bleed: f32) -> El {
    rune::effect(kind)
        .effect_data(data)
        .effect_bleed(bleed)
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
}

/// A breathing ring behind `mark`, in `color`.
fn pulsing(mark: El, color: Color, size: f32) -> El {
    row()
        .size(size, size)
        .center()
        .radius(size / 2.0)
        .child(fx(gfx::PULSE, gfx::data(color, 0.6), 14.0).radius(size / 2.0))
        .child(mark)
}

/// The crucible, molten. A click sloshes it, and so does moving the window.
fn logo(app: &App, p: &Paint) -> El {
    if cfg!(feature = "hot") {
        return el()
            .size(18.0, 18.0)
            .radius(5.0)
            .fill(p.c.bg_accent)
            .center()
            .child(icon(&oc::BRANCH, 12.0).color(Color::WHITE));
    }
    el().key("logo")
        .size(30.0, 30.0)
        .role("button")
        .label("Crucible")
        .on_press(|app: &mut App| app.logo_clicks += 1)
        .on_screen_layout(|app: &mut App, [x, y, _, _]| app.logo_screen = [x, y])
        .child(
            rune::effect(gfx::MOLTEN)
                .effect_data([
                    app.logo_clicks as f32,
                    if live_work(app) { 1.0 } else { 0.0 },
                    app.logo_screen[0],
                    app.logo_screen[1],
                ])
                .absolute(0.0, 0.0)
                .w_full()
                .h_full(),
        )
}

/// A CRT tube over its parent, when the setting is on.
fn tube(app: &App) -> Option<El> {
    app.settings.crt.then(|| {
        fx(
            gfx::CRT,
            [if live_work(app) { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            0.0,
        )
        .effect_backdrop(true)
    })
}

/// Whether any listed run is still going, which keeps the brand tile moving.
fn live_work(app: &App) -> bool {
    match app.backend {
        Backend::Local => app
            .packs
            .iter()
            .any(|pack| pack.verdict == Verdict::Running),
        Backend::Controller => app
            .visible_playbooks()
            .iter()
            .any(|playbook| playbook.verdict() == Verdict::Running),
    }
}

/// A vertical or horizontal hairline in the border colour.
fn rule(key: &str, vertical: bool, p: &Paint) -> El {
    let line = el().key(key.to_string()).fill(p.c.border_base);
    if vertical {
        line.w(p.hair).h_full()
    } else {
        line.h(p.hair).w_full()
    }
}

/// A neutral tag: a task's kind, a pane's source.
fn tag(label: &str, p: &Paint) -> El {
    Badge::new(label).build(p.k)
}

/// A tag in a status role's colours, with an optional leading icon.
fn flag(label: &str, tone: Tone, glyph: Option<&'static Icon>, p: &Paint) -> El {
    let s = p.tone(tone);
    row()
        .role("status")
        .label(label.to_string())
        .h(20.0)
        .px(7.0)
        .gap(4.0)
        .center_y()
        .radius(p.k.radius.sm)
        .fill(s.bg)
        .border(p.hair, s.border)
        .children(glyph.map(|g| icon(g, 12.0).color(s.icon)))
        .child(
            text(label.to_string())
                .font(p.k.ty.small_medium)
                .color(s.fg),
        )
}

/// A thin bar split into coloured parts; each part is (fraction, colour).
fn meter(key: &str, parts: &[(f32, Color)], p: &Paint) -> El {
    let mut bar = row()
        .key(key.to_string())
        .grow(1.0)
        .h(4.0)
        .radius(2.0)
        .fill(p.c.layer_03)
        .clip_rounded(2.0);
    for (fraction, ink) in parts {
        if *fraction > 0.0 {
            bar = bar.child(el().w_frac(fraction.min(1.0)).h(4.0).fill(*ink));
        }
    }
    bar
}

pub fn view(app: &App, cx: &mut Cx<App>) -> El {
    let p = palette(app, cx);
    let window = row()
        .key("window")
        .w_full()
        .h_full()
        .fill(p.c.bg_base)
        .child(sidebar(app, &p))
        .child(rule("sidebar-edge", true, &p));
    let window = window
        .child(detail(app, &p))
        .children(dock::ghost(app, &p))
        .child(fx(gfx::GRAIN, gfx::data(Color::WHITE, 0.035), 0.0));
    let mut layer = Vec::new();
    launcher::sheet(app, &p, &mut layer);
    palette::palette(app, &p, &mut layer);
    rune_kit::root(window, layer)
}

/// One run, from whichever source, in the shape the detail pane draws.
struct Pane {
    title: String,
    subtitle: String,
    path: String,
    verdict: Verdict,
    spent: f64,
    cap: f64,
    passed: usize,
    failed: usize,
    tasks: Vec<TaskView>,
    time: String,
    time_note: Option<String>,
    meta: Option<String>,
    /// Why a failed run failed: the engine's stderr, an error or its shutdown reason.
    problem: Option<String>,
    /// Its session is streaming in as the engine writes it.
    streaming: bool,
    log: Vec<LogLine>,
    /// The agents' session transcript; empty for a run whose engine did not record one.
    transcript: Vec<LogLine>,
    /// The same, event by event, for reading a turn.
    agent_events: Vec<crate::session::AgentStep>,
    /// Why there is no task list, when there is none.
    empty: (&'static str, String),
    pack: Result<PackFiles, String>,
    /// The launch record, for the Launch tab; local packs have none.
    launch: Option<crux::dto::PlaybookLaunch>,
    log_font: LogFont,
    launchable: bool,
    /// The pack as it compiles now: a local pack's, or the run's draft working copy's.
    preview: Option<crate::preview::Preview>,
    /// `tasks` are the preview's, not the run's.
    previewing: bool,
    /// The run's tasks while the preview's are shown, for marking what the edit changed.
    run_tasks: Vec<TaskView>,
}

impl Pane {
    /// Show the compiled pack's tasks when they are picked, or when there is no run to show.
    fn previewed(mut self, source: Option<crate::GraphSource>) -> Pane {
        let Some(compiled) = self.preview.as_ref().map(|p| p.fold.tasks.clone()) else {
            return self;
        };
        let show = match source {
            Some(crate::GraphSource::Preview) => true,
            Some(crate::GraphSource::Run) => false,
            None => self.tasks.is_empty() && !compiled.is_empty(),
        };
        if show {
            self.run_tasks = std::mem::replace(&mut self.tasks, compiled);
            self.previewing = true;
        }
        self
    }

    /// What the agents did: the session transcript when the engine recorded one, otherwise the
    /// console's sampled progress lines.
    fn agent_lines(&self) -> &[LogLine] {
        if self.transcript.is_empty() {
            &self.log
        } else {
            &self.transcript
        }
    }
}

fn local_pane(pack: &PackView, log_font: LogFont) -> Pane {
    Pane {
        title: pack.name.clone(),
        subtitle: if pack.goal.is_empty() {
            "A local pack with no [agent] goal.".to_string()
        } else {
            pack.goal.clone()
        },
        path: pack.manifest.clone(),
        verdict: pack.verdict,
        spent: pack.spent,
        cap: pack.budget_usd,
        passed: pack.passed,
        failed: pack.failed,
        tasks: pack.tasks.clone(),
        time: pack
            .elapsed
            .clone()
            .or_else(|| (!pack.started_at.is_empty()).then(|| format!("at {}", pack.started_at)))
            .unwrap_or_else(|| "—".to_string()),
        time_note: Some(format!("{MAX_TIME} cap")),
        meta: None,
        streaming: false,
        problem: matches!(pack.verdict, Verdict::Invalid | Verdict::Error)
            .then(|| pack.detail.clone())
            .flatten(),
        log: pack.log.clone(),
        transcript: pack.transcript.clone(),
        agent_events: pack.agent_events.clone(),
        log_font,
        launchable: true,
        preview: Some(pack.preview.clone()),
        previewing: false,
        run_tasks: Vec::new(),
        pack: Ok(pack.pack.clone()),
        launch: None,
        empty: (
            "No run recorded here yet.",
            "Run it with the button above, or from a shell with `crucible plan run`.".to_string(),
        ),
    }
}

/// `pinned` is a launch opened from the runs page, which the breadcrumb already names.
fn live_pane(
    launch: &LivePlaybook,
    detail: Option<&LiveDetail>,
    log_font: LogFont,
    pinned: bool,
) -> Pane {
    let byline = format!(
        "{} · by {}{}",
        launch.status,
        launch.by,
        if launch.agent.is_empty() {
            String::new()
        } else {
            format!(" · {}", launch.agent)
        }
    );
    Pane {
        title: if pinned {
            launch.landed_at.clone()
        } else {
            launch.id.clone()
        },
        subtitle: launch.description.clone().unwrap_or(byline),
        path: launch.key.clone(),
        verdict: detail.map_or_else(|| launch.verdict(), |detail| detail.verdict),
        spent: detail.map_or_else(|| launch.cost.unwrap_or(0.0), |detail| detail.spent),
        cap: launch.cap,
        passed: detail.map_or(0, |detail| detail.passed),
        failed: detail.map_or(0, |detail| detail.failed),
        tasks: detail
            .map(|detail| detail.fold.tasks.clone())
            .unwrap_or_default(),
        time: format!("at {}", launch.created),
        time_note: Some(cap_note(&launch.max_time)),
        meta: Some(live_meta(&launch.origin, launch.runs, &launch.landed_at)),
        streaming: detail.is_some_and(|detail| detail.streaming),
        problem: detail.and_then(|detail| detail.error.clone()).or_else(|| {
            let verdict = detail.map_or_else(|| launch.verdict(), |detail| detail.verdict);
            matches!(verdict, Verdict::Invalid | Verdict::Error).then(|| {
                launch.why.clone().or_else(|| {
                    let shutdown = detail?.fold.shutdown.as_ref()?;
                    Some(format!("{}: {}", shutdown.outcome, shutdown.reason))
                })
            })?
        }),
        log: detail.map(|detail| detail.log.clone()).unwrap_or_default(),
        transcript: detail
            .map(|detail| detail.fold.transcript.clone())
            .unwrap_or_default(),
        agent_events: detail
            .map(|detail| detail.fold.agent_events.clone())
            .unwrap_or_default(),
        log_font,
        launchable: false,
        preview: None,
        previewing: false,
        run_tasks: Vec::new(),
        empty: live_empty(launch, detail),
        pack: detail.map_or_else(
            || Err("Reading the pack\u{2026}".to_string()),
            |detail| detail.pack.clone(),
        ),
        launch: detail.and_then(|detail| detail.launch.clone()),
    }
}

/// Why a live launch shows no tasks: still loading, unreadable, never dispatched, or no plan.
fn live_empty(launch: &LivePlaybook, detail: Option<&LiveDetail>) -> (&'static str, String) {
    let Some(detail) = detail else {
        return ("Reading the run\u{2026}", launch.key.clone());
    };
    if let Some(error) = &detail.error {
        return ("Could not read the run.", error.clone());
    }
    if launch.runs == 0 {
        let why = launch
            .why
            .clone()
            .unwrap_or_else(|| "The controller has not dispatched it.".to_string());
        return ("This launch never started a run.", why);
    }
    (
        "The run has no session yet.",
        "Its engine has not admitted a plan, or the session was never stored.".to_string(),
    )
}

fn sidebar(app: &App, p: &Paint) -> El {
    let source = match app.backend {
        Backend::Controller => match &app.live {
            LiveView::Ready { endpoint, .. } => endpoint.clone(),
            LiveView::Loading => "crux".to_string(),
            LiveView::Unavailable(_) => "crux · not connected".to_string(),
        },
        Backend::Local => app.repo.clone(),
    };

    let brand = row()
        .key("brand")
        .center_y()
        .gap(6.0)
        .child(logo(app, p))
        .child(text("Playbooks").font(BRAND).color(p.c.text_base));

    let band = row()
        .key("title-band")
        .w_full()
        .h(TITLE_BAND)
        .pl(if p.inset > 0.0 {
            p.inset + 10.0
        } else {
            SIDEBAR_PAD
        })
        .center_y()
        .window_drag()
        .child(brand);
    let mut head = col()
        .key("sidebar-head")
        .w_full()
        .px(SIDEBAR_PAD)
        .pb(14.0)
        .gap(12.0)
        .child(backend_switch(app, p))
        .child(search(app, p));
    if let (
        Backend::Controller,
        LiveView::Ready {
            stale: Some(error), ..
        },
    ) = (app.backend, &app.live)
    {
        head = head.child(
            text(error.clone())
                .font(p.k.ty.caption())
                .color(p.c.warning.fg)
                .max_lines(2)
                .wrap(),
        );
    }
    if app.backend == Backend::Controller {
        head = head.child(filters(app, p));
    }

    col()
        .key("sidebar")
        .w(SIDEBAR)
        .h_full()
        .min_h(0.0)
        .fill(p.c.page)
        .child(band)
        .child(head)
        .child(rule("sidebar-head-rule", false, p))
        .child(pack_list(app, p))
        .children((app.backend == Backend::Controller).then(|| pages_nav(app, p)))
        .child(rule("sidebar-foot-rule", false, p))
        .child(account(app, source, p))
}

/// The controller's management pages, under the playbooks.
fn pages_nav(app: &App, p: &Paint) -> El {
    let waiting = app.waiting();
    let entry = |page: crate::Page, glyph: &'static Icon, badge: Option<String>| {
        let mut entry = SidebarRow::new(page.title())
            .icon(glyph)
            .height(32.0)
            .selected(app.page == page)
            .on_select(move |app: &mut App, _: &mut Tx| app.toggle_page(page));
        if let Some(badge) = badge {
            entry = entry.badge(badge);
        }
        entry.build(p.k).key(format!("nav-{}", page.id()))
    };
    col()
        .key("pages-nav")
        .w_full()
        .px(8.0)
        .pb(6.0)
        .gap(2.0)
        .child(rule("pages-nav-rule", false, p).mb(6.0))
        .child(entry(
            crate::Page::Decisions,
            &oc::CHECKLIST,
            (waiting > 0).then(|| waiting.to_string()),
        ))
        .child(entry(crate::Page::Schedules, &oc::STATUS, None))
        .child(entry(crate::Page::Secrets, &rune_kit::glyphs::LOCK, None))
}

/// Who is signed in, bottom left, with the settings button beside it.
fn account(app: &App, source: String, p: &Paint) -> El {
    let (name, about) = match (app.backend, &app.live) {
        (Backend::Local, _) => ("Local packs".to_string(), source),
        (Backend::Controller, LiveView::Ready { identity, .. }) if !identity.user.is_empty() => {
            let role = if identity.role.is_empty() {
                String::new()
            } else {
                format!(" ({})", identity.role)
            };
            (
                identity.user.clone(),
                format!("{}{role}\n{source}", identity.user),
            )
        }
        (Backend::Controller, LiveView::Unavailable(_)) => ("Not connected".to_string(), source),
        (Backend::Controller, _) => ("Connecting\u{2026}".to_string(), source),
    };
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or("?".to_string(), |c| c.to_uppercase().to_string());
    row()
        .key("account")
        .w_full()
        .h(52.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .gap(10.0)
                .center_y()
                .help(about)
                .child(
                    row()
                        .size(26.0, 26.0)
                        .radius(13.0)
                        .fill(p.c.layer_03)
                        .border(p.hair, p.c.border_base)
                        .center()
                        .child(text(initial).font(p.k.ty.small_medium).color(p.c.text_base)),
                )
                .child(
                    text(name)
                        .font(p.k.ty.ui_medium)
                        .color(p.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                ),
        )
        .child(
            IconButton::new(&oc::SETTINGS_GEAR)
                .variant(if app.page == crate::Page::Settings {
                    IconButtonVariant::Neutral
                } else {
                    IconButtonVariant::GhostMuted
                })
                .label("Settings")
                .on_press(|app: &mut App| app.toggle_page(crate::Page::Settings))
                .build(p.k)
                .key("settings-open"),
        )
}

fn search(app: &App, p: &Paint) -> El {
    let placeholder = match app.backend {
        Backend::Controller => "Filter playbooks",
        Backend::Local => "Filter packs",
    };
    let mut input = TextInput::new(&app.playbook_search)
        .placeholder(placeholder)
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .width(Some(SIDEBAR - 2.0 * SIDEBAR_PAD))
        .on_edit(|app: &mut App| &mut app.playbook_search);
    if !app.playbook_search.text.is_empty() {
        input = input.clear_button();
    }
    input.build(p.k).key("playbook-search")
}

fn filters(app: &App, p: &Paint) -> El {
    col()
        .key("filters")
        .w_full()
        .gap(8.0)
        .child(filter_row(
            "Owner",
            &OwnerFilter::ALL.map(|choice| choice.label()),
            OwnerFilter::ALL
                .iter()
                .position(|choice| *choice == app.owner_filter),
            |app: &mut App, i: usize, _: &mut Tx| app.owner_filter = OwnerFilter::ALL[i],
            p,
        ))
        .child(filter_row(
            "Origin",
            &OriginFilter::ALL.map(|choice| choice.label()),
            OriginFilter::ALL
                .iter()
                .position(|choice| *choice == app.origin_filter),
            |app: &mut App, i: usize, _: &mut Tx| app.origin_filter = OriginFilter::ALL[i],
            p,
        ))
        .child(filter_row(
            "Landed",
            &LandedFilter::ALL.map(|choice| choice.label()),
            LandedFilter::ALL
                .iter()
                .position(|choice| *choice == app.landed_filter),
            |app: &mut App, i: usize, _: &mut Tx| app.landed_filter = LandedFilter::ALL[i],
            p,
        ))
}

fn filter_row(
    label: &str,
    choices: &[&str],
    selected: Option<usize>,
    pick: impl Fn(&mut App, usize, &mut Tx) + Clone + 'static,
    p: &Paint,
) -> El {
    col()
        .key(format!("filter-{}", slug(label)))
        .w_full()
        .gap(4.0)
        .child(text(label).font(p.k.ty.small).color(p.c.text_faint))
        .child(
            SegmentedControl::new(choices, selected)
                .label(label)
                .width(Some(SIDEBAR - 2.0 * SIDEBAR_PAD))
                .on_pick(pick)
                .build(p.k),
        )
}

fn slug(label: &str) -> String {
    label.to_ascii_lowercase().replace(' ', "-")
}

fn backend_switch(app: &App, p: &Paint) -> El {
    const BACKENDS: [Backend; 2] = [Backend::Controller, Backend::Local];
    SegmentedControl::new(
        &BACKENDS.map(|backend| backend.label()),
        BACKENDS.iter().position(|backend| *backend == app.backend),
    )
    .label("Source")
    .width(Some(SIDEBAR - 2.0 * SIDEBAR_PAD))
    .on_pick(|app: &mut App, i: usize, _: &mut Tx| app.switch(BACKENDS[i]))
    .build(p.k)
    .key("backend")
}

fn pack_list(app: &App, p: &Paint) -> El {
    let mut list = col()
        .key("packs")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(1.0)
        .px(8.0)
        .py(8.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(10.0);

    match app.backend {
        Backend::Controller => match &app.live {
            LiveView::Ready { now, .. } => {
                let playbooks = app.visible_playbooks();
                if playbooks.is_empty() {
                    list = list.child(note(
                        if app.filtering() {
                            "No playbooks match."
                        } else {
                            "No playbooks registered."
                        },
                        p,
                    ));
                } else {
                    list = list.children(live_rows(app, &playbooks, *now, p));
                }
            }
            LiveView::Loading => {
                list = list.child(
                    row()
                        .gap(8.0)
                        .px(8.0)
                        .py(8.0)
                        .center_y()
                        .child(Spinner::new().size(12.0).build(p.k))
                        .child(
                            text("Contacting the controller\u{2026}")
                                .font(p.k.ty.ui)
                                .color(p.c.text_muted),
                        ),
                )
            }
            LiveView::Unavailable(error) => list = list.child(unavailable(error, app, p)),
        },
        Backend::Local => {
            let packs: Vec<&PackView> = app.visible_packs().collect();
            if packs.is_empty() {
                list = list.child(note(
                    if app.filtering() {
                        "No packs match."
                    } else {
                        "No packs read."
                    },
                    p,
                ));
            }
            for pack in packs {
                list = list.child(pack_row(app, pack, p));
            }
        }
    }
    p.k.scroll_bar(list)
        .key("packs-pane")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
}

fn note(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.ui)
        .color(p.c.text_muted)
        .px(8.0)
        .py(8.0)
}

fn unavailable(error: &str, app: &App, p: &Paint) -> El {
    let s = p.tone(Tone::Danger);
    let action = if app.signing_in {
        row()
            .gap(8.0)
            .center_y()
            .child(Spinner::new().size(14.0).build(p.k))
            .child(
                text("Waiting for the browser\u{2026}")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            )
    } else {
        row().child(
            Button::new("Sign in with browser")
                .variant(ButtonVariant::Submit)
                .on_press(|app: &mut App| app.sign_in())
                .build(p.k)
                .key("sign-in"),
        )
    };
    col()
        .key("live-error")
        .w_full()
        .gap(8.0)
        .padding(10.0)
        .radius(p.k.radius.lg)
        .fill(s.bg)
        .border(p.hair, s.border)
        .child(
            row()
                .gap(6.0)
                .center_y()
                .child(icon(&oc::WARNING, 14.0).color(s.icon))
                .child(text("Not connected").font(p.k.ty.ui_medium).color(s.fg)),
        )
        .child(
            text(error.to_string())
                .font(mono(11.0, 400.0))
                .color(s.fg)
                .max_lines(4)
                .wrap(),
        )
        .child(
            TextInput::new(&app.sign_in_url)
                .placeholder("https://crucible.example.com")
                .width(Some(SIDEBAR - 2.0 * SIDEBAR_PAD - 20.0))
                .disabled(app.signing_in)
                .on_edit(|app: &mut App| &mut app.sign_in_url)
                .build(p.k)
                .key("sign-in-url"),
        )
        .child(action)
        .children(
            app.sign_in_error
                .as_ref()
                .map(|why| text(why.clone()).font(p.k.ty.small).color(s.fg).wrap()),
        )
}

fn cap_note(max_time: &str) -> String {
    if max_time.is_empty() {
        "no cap".to_string()
    } else {
        format!("{max_time} cap")
    }
}

fn live_meta(origin: &str, runs: i64, landed_at: &str) -> String {
    let origin = if origin.is_empty() { "launch" } else { origin };
    format!("{origin} · {} · landed {landed_at}", runs_note(runs))
}

fn runs_note(runs: i64) -> String {
    match runs {
        0 => "no runs".to_string(),
        1 => "1 run".to_string(),
        n => format!("{n} runs"),
    }
}

fn row_meta(playbook: &LivePlaybook) -> String {
    let origin = if playbook.origin.is_empty() {
        "launch"
    } else {
        playbook.origin.as_str()
    };
    format!("{} \u{b7} {origin}", playbook.status)
}

fn age(landed: i64, now: i64) -> String {
    let secs = now.saturating_sub(landed);
    match secs {
        s if s < 60 => "now".to_string(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// The status slot of a sidebar row: a spinner while running, a filled dot for a verdict, an
/// empty ring for a pack that never ran.
fn verdict_status(verdict: Verdict, p: &Paint) -> RowStatus {
    match verdict {
        Verdict::Running => RowStatus::Other(pulsing(
            Spinner::new().size(12.0).color(p.live).build(p.k),
            p.live,
            12.0,
        )),
        Verdict::NotRun | Verdict::Unfinished => {
            RowStatus::Other(el().size(7.0, 7.0).radius(3.5).border(1.0, p.c.text_faint))
        }
        verdict => RowStatus::Other(
            el().size(7.0, 7.0)
                .radius(3.5)
                .fill(verdict_color(verdict, p)),
        ),
    }
}

fn live_rows(app: &App, playbooks: &[&LivePlaybook], now: i64, p: &Paint) -> Vec<El> {
    let current = app.selected_playbook();
    playbooks
        .iter()
        .map(|playbook| {
            let id = playbook.id.clone();
            let mut item = SidebarRow::new(playbook.id.clone())
                .subtitle(row_meta(playbook))
                .height(ROW)
                .status(verdict_status(playbook.verdict(), p))
                .selected(current == Some(playbook.id.as_str()))
                .on_select(move |app: &mut App, _: &mut Tx| app.select(Selection::playbook(&id)));
            if playbook.landed > 0 {
                item = item.detail(age(playbook.landed, now));
            }
            item.build(p.k).key(format!("live-{}", playbook.id))
        })
        .collect()
}

/// The launches of the selected live playbook the filters keep, when there is more than one.
fn playbook_launches(app: &App) -> Option<(&str, Vec<&LivePlaybook>)> {
    let playbook = app.selected_playbook()?;
    let launches: Vec<&LivePlaybook> = app
        .live
        .kept(&app.launch_filter())
        .into_iter()
        .filter(|launch| launch.id == playbook)
        .collect();
    (launches.len() > 1).then_some((playbook, launches))
}

fn pack_row(app: &App, pack: &PackView, p: &Paint) -> El {
    let selected = app.selection == Some(Selection::Local(pack.name.clone()));
    let name = pack.name.clone();
    let subtitle = match pack.tasks.len() {
        0 => pack.verdict.label().to_string(),
        1 => format!("{} · 1 task", pack.verdict.label()),
        n => format!("{} · {n} tasks", pack.verdict.label()),
    };
    SidebarRow::new(pack.name.clone())
        .subtitle(subtitle)
        .height(ROW)
        .status(verdict_status(pack.verdict, p))
        .selected(selected)
        .on_select(move |app: &mut App, _: &mut Tx| app.select(Selection::Local(name.clone())))
        .build(p.k)
        .key(format!("pack-{}", pack.name))
}

fn verdict_color(verdict: Verdict, p: &Paint) -> Color {
    match verdict {
        Verdict::Valid => p.pass,
        Verdict::Invalid | Verdict::Error => p.fail,
        Verdict::Running => p.live,
        Verdict::Unfinished | Verdict::NotRun => p.c.text_faint,
    }
}

fn verdict_tone(verdict: Verdict) -> Option<(Tone, &'static Icon)> {
    match verdict {
        Verdict::Valid => Some((Tone::Success, &oc::CIRCLE_CHECK)),
        Verdict::Invalid | Verdict::Error => Some((Tone::Danger, &oc::CIRCLE_X)),
        Verdict::Running => Some((Tone::Warning, &oc::STATUS)),
        Verdict::Unfinished | Verdict::NotRun => None,
    }
}

fn detail(app: &App, p: &Paint) -> El {
    match app.page {
        crate::Page::Settings => return settings_pane(app, p),
        crate::Page::Schedules => return schedules::page(app, p),
        crate::Page::Secrets => return secrets::page(app, p),
        crate::Page::Decisions => return decisions::page(app, p),
        crate::Page::Runs => {}
    }
    if let (
        Some(Selection::Live { launch: None, .. }),
        Some((playbook, launches)),
        LiveView::Ready { now, .. },
    ) = (&app.selection, playbook_launches(app), &app.live)
    {
        return runs::runs_page(
            launcher::launch_button(app, playbook, p),
            shares::section(app, p),
            playbook,
            &launches,
            *now,
            p,
        );
    }
    let font = app.settings.log_font;
    let pane = match &app.selection {
        Some(Selection::Local(_)) => app.selected_pack().map(|pack| local_pane(pack, font)),
        Some(Selection::Live { launch, .. }) => {
            let pinned = launch.is_some() && playbook_launches(app).is_some();
            app.selected_live().map(|live| Pane {
                preview: app.working_preview.clone(),
                ..live_pane(live, app.selected_detail(), font, pinned)
            })
        }
        None => None,
    }
    .map(|pane| pane.previewed(app.graph_source));

    let Some(pane) = pane else {
        return col()
            .key("detail")
            .grow(1.0)
            .h_full()
            .center()
            .gap(8.0)
            .padding(28.0)
            .child(icon(&oc::CHECKLIST, 28.0).color(p.c.icon_faint))
            .child(
                text("No playbook selected")
                    .font(SECTION)
                    .color(p.c.text_base),
            )
            .child(
                text("Pick one from the sidebar to see its run.")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            );
    };

    let mut body = col()
        .key("detail")
        .grow(1.0)
        .h_full()
        .min_h(0.0)
        .min_w(0.0)
        .px(28.0)
        .pb(20.0)
        .gap(16.0)
        .child(header(&pane, app, p));

    if let Some(problem) = &pane.problem {
        body = body.child(banner(problem, p));
    }
    if let Some(error) = &app.error {
        body = body.child(banner(error, p));
    }
    if let Some(note) = &app.handoff_note {
        let (line, color) = match note {
            Ok(path) => (
                format!("Brief copied \u{b7} {}", path.display()),
                p.c.text_muted,
            ),
            Err(why) => (why.clone(), p.fail),
        };
        body = body.child(
            text(line)
                .font(p.k.ty.small)
                .color(color)
                .selectable(true)
                .selection_color(p.k.selection())
                .truncate()
                .key("handoff-note"),
        );
    }

    body.child(stats(&pane, p))
        .child(dock::workspace(&pane, app, p))
}

/// Copies the `crucible-playbooks://` link to the run on screen.
fn link_button(app: &App, p: &Paint) -> Option<El> {
    let url = app.link_here(None)?.url();
    Some(
        IconButton::new(&oc::COPY)
            .variant(IconButtonVariant::GhostMuted)
            .label("Copy link")
            .help(url.clone())
            .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(url.clone()))
            .build(p.k)
            .key("copy-link"),
    )
}

fn header(pane: &Pane, app: &App, p: &Paint) -> El {
    let about = match &pane.meta {
        Some(meta) => format!("{}\n{meta}", pane.path),
        None => pane.path.clone(),
    };
    col()
        .key("header")
        .w_full()
        .gap(0.0)
        .child(
            row()
                .w_full()
                .h(TITLE_BAND)
                .gap(10.0)
                .center_y()
                .window_drag()
                .children(breadcrumb(app, p))
                .child(
                    text(pane.title.clone())
                        .font(TITLE)
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection())
                        .help(about),
                )
                .children(pane.streaming.then(|| {
                    pulsing(el().size(7.0, 7.0).radius(3.5).fill(p.live), p.live, 12.0)
                        .help("Streaming")
                }))
                .child(el().grow(1.0))
                .children(dock::tools(app, p))
                .children(link_button(app, p))
                .children(
                    (!pane.launchable)
                        .then(|| launcher::fix_button(app, p))
                        .flatten(),
                )
                .children(pane.launchable.then(|| run_button(pane, p)))
                .children(
                    (!pane.launchable)
                        .then(|| launcher::rerun_button(app, p))
                        .flatten(),
                )
                .children(
                    (!pane.launchable)
                        .then(|| app.selected_playbook())
                        .flatten()
                        .and_then(|playbook| launcher::launch_button(app, playbook, p)),
                ),
        )
        .child(
            text(pane.subtitle.clone())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .max_lines(2)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        )
}

/// Back to the runs page, ahead of a launch picked there.
fn breadcrumb(app: &App, p: &Paint) -> Option<El> {
    let Some(Selection::Live {
        launch: Some(_), ..
    }) = &app.selection
    else {
        return None;
    };
    let (playbook, _) = playbook_launches(app)?;
    let back = Selection::playbook(playbook);
    Some(
        row()
            .gap(6.0)
            .center_y()
            .child(
                Button::new(playbook.to_string())
                    .variant(ButtonVariant::GhostMuted)
                    .on_press(move |app: &mut App| app.select(back.clone()))
                    .build(p.k)
                    .key("back-to-runs"),
            )
            .child(icon(&oc::CHEVRON_RIGHT, 14.0).color(p.c.icon_faint)),
    )
}

fn run_button(pane: &Pane, p: &Paint) -> El {
    let running = pane.verdict == Verdict::Running;
    let button = if running {
        Button::new("Running\u{2026}")
            .variant(ButtonVariant::Loading)
            .disabled(true)
    } else {
        Button::new("Run playbook")
            .variant(ButtonVariant::Submit)
            .on_press(|app: &mut App| app.launch())
    };
    button.build(p.k).key("run")
}

fn stats(pane: &Pane, p: &Paint) -> El {
    let ceiling = pane.cap.max(0.01);
    let fraction = (pane.spent / ceiling).clamp(0.0, 1.0) as f32;
    let spend_ink = if fraction >= 0.9 { p.fail } else { p.accent };
    row()
        .key("stats")
        .w_full()
        .gap(10.0)
        .child(status_stat(pane, p))
        .child(stat(
            "spend",
            "Spend",
            &format!("${:.4}", pane.spent),
            p.c.text_base,
            Some(meter("spend-track", &[(fraction, spend_ink)], p)),
            &format!("of ${ceiling:.2}"),
            p,
        ))
        .child(tasks_stat(pane, p))
        .child(activity_stat(pane, p))
        .child(
            stat("time", "Time", &pane.time, p.c.text_base, None, "", p)
                .help(pane.time_note.clone().unwrap_or_default()),
        )
}

fn stat_card(key: &str, p: &Paint) -> El {
    col()
        .key(format!("stat-{key}"))
        .grow(1.0)
        .min_w(0.0)
        .gap(4.0)
        .px(14.0)
        .py(12.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
}

fn stat_label(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.small_medium)
        .color(p.c.text_muted)
}

fn stat_sub(sub: &str, p: &Paint) -> El {
    text(sub.to_string())
        .font(p.k.ty.caption())
        .color(p.c.text_faint)
        .truncate()
}

fn status_stat(pane: &Pane, p: &Paint) -> El {
    let ink = verdict_color(pane.verdict, p);
    let glyph = verdict_tone(pane.verdict).map(|(_, glyph)| glyph);
    stat_card("status", p).child(stat_label("Status", p)).child(
        row()
            .gap(6.0)
            .center_y()
            .children(match pane.verdict {
                Verdict::Running => Some(Spinner::new().size(16.0).color(ink).build(p.k)),
                _ => glyph.map(|glyph| icon(glyph, 16.0).color(ink)),
            })
            .child(
                text(pane.verdict.label())
                    .font(VALUE)
                    .color(if glyph.is_some() { ink } else { p.c.text_muted })
                    .truncate(),
            ),
    )
}

fn stat(
    key: &str,
    label: &str,
    value: &str,
    ink: Color,
    bar: Option<El>,
    sub: &str,
    p: &Paint,
) -> El {
    let mut line = row().w_full().center_y().gap(8.0);
    if let Some(bar) = bar {
        line = line.child(bar);
    }
    stat_card(key, p)
        .child(stat_label(label, p))
        .child(text(value.to_string()).font(VALUE).color(ink).truncate())
        .child(line.child(stat_sub(sub, p)))
}

/// Passed over planned, with a bar split by where each task stands.
fn tasks_stat(pane: &Pane, p: &Paint) -> El {
    let total = pane.tasks.len().max(1) as f32;
    let mut counts = [0usize; 4];
    for task in &pane.tasks {
        let slot = match task.state() {
            TaskState::Settled(status) if status.passed() => 0,
            TaskState::Settled(TaskStatus::Skipped | TaskStatus::NotTaken) => 3,
            TaskState::Settled(_) => 1,
            TaskState::Running(_) => 2,
            TaskState::Pending => continue,
        };
        counts[slot] += 1;
    }
    let parts: Vec<(f32, Color)> = counts
        .iter()
        .zip([p.pass, p.fail, p.live, p.c.text_faint])
        .map(|(n, ink)| (*n as f32 / total, ink))
        .collect();
    let note = if counts[2] > 0 {
        format!("{} running", counts[2])
    } else if pane.failed > 0 {
        format!("{} failed", pane.failed)
    } else {
        String::new()
    };
    stat(
        "tasks",
        "Tasks",
        &format!("{} / {}", pane.passed, pane.tasks.len()),
        p.c.text_base,
        Some(meter("tasks-track", &parts, p)),
        &note,
        p,
    )
}

/// What the agents did across the whole run, from their console lines.
fn activity_stat(pane: &Pane, p: &Paint) -> El {
    let (tools, failed, tokens) = pane
        .tasks
        .iter()
        .map(|task| activity(pane.agent_lines(), &task.name))
        .fold((0, 0, 0), |(t, f, k), a| {
            (t + a.tools, f + a.failed_tools, k + a.tokens)
        });
    let sub = if tools == 0 {
        String::new()
    } else if failed > 0 {
        format!("{failed} failed \u{b7} {} tokens", tasks::count(tokens))
    } else {
        format!("{} tokens", tasks::count(tokens))
    };
    stat(
        "agents",
        "Tool calls",
        &tools.to_string(),
        if failed > 0 { p.fail } else { p.c.text_base },
        None,
        &sub,
        p,
    )
}

fn settings_pane(app: &App, p: &Paint) -> El {
    let mut rows = Vec::new();
    for (i, font) in LogFont::ALL.into_iter().enumerate() {
        if i > 0 {
            rows.push(rule(&format!("font-sep-{i}"), false, p));
        }
        rows.push(font_option(font, app, p));
    }

    col()
        .key("detail")
        .grow(1.0)
        .h_full()
        .min_h(0.0)
        .min_w(0.0)
        .px(40.0)
        .pt(32.0)
        .pb(32.0)
        .gap(24.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(
            row()
                .w_full()
                .center_y()
                .gap(10.0)
                .child(text("Settings").font(TITLE).color(p.c.text_base).grow(1.0))
                .child(
                    Button::new("Done")
                        .variant(ButtonVariant::Neutral)
                        .on_press(|app: &mut App| app.page = crate::Page::Runs)
                        .build(p.k)
                        .key("settings-done"),
                ),
        )
        .child(settings_section(
            "appearance",
            "Appearance",
            vec![
                setting_row(
                    "Theme",
                    SegmentedControl::new(
                        &Theme::ALL.map(|theme| theme.label()),
                        Theme::ALL.iter().position(|t| *t == app.settings.theme),
                    )
                    .label("Theme")
                    .width(Some(360.0))
                    .on_pick(|app: &mut App, i: usize, tx: &mut Tx| {
                        app.settings.set_theme(Theme::ALL[i]);
                        tx.color_motion(rune::Spring::new(0.35, 0.0));
                    })
                    .build(p.k)
                    .key("theme"),
                    p,
                ),
                rule("appearance-sep", false, p),
                setting_row(
                    "Mode",
                    SegmentedControl::new(
                        &Appearance::ALL.map(|a| a.label()),
                        Appearance::ALL
                            .iter()
                            .position(|a| *a == app.settings.appearance),
                    )
                    .label("Mode")
                    .width(Some(240.0))
                    .on_pick(|app: &mut App, i: usize, tx: &mut Tx| {
                        app.settings.set_appearance(Appearance::ALL[i]);
                        tx.color_motion(rune::Spring::new(0.35, 0.0));
                    })
                    .build(p.k)
                    .key("mode"),
                    p,
                ),
            ],
            p,
        ))
        .child(settings_section(
            "effects",
            "Effects",
            vec![setting_row(
                "CRT charts",
                Switch::new(app.settings.crt)
                    .label("CRT charts")
                    .hide_label()
                    .on_toggle(|app: &mut App| {
                        let on = !app.settings.crt;
                        app.settings.set_crt(on);
                    })
                    .build(p.k)
                    .key("crt-switch"),
                p,
            )],
            p,
        ))
        .child(settings_section(
            "checks",
            "Checks",
            vec![setting_row(
                "Check pack files",
                Switch::new(app.settings.checks)
                    .label("Check pack files")
                    .hide_label()
                    .on_toggle(|app: &mut App| {
                        let on = !app.settings.checks;
                        app.set_checks(on);
                    })
                    .build(p.k)
                    .key("checks-switch"),
                p,
            )],
            p,
        ))
        .child(settings_section(
            "notifications",
            "Notifications",
            vec![
                setting_row(
                    "When my runs finish",
                    Switch::new(app.settings.notify)
                        .label("When my runs finish")
                        .hide_label()
                        .on_toggle(|app: &mut App| {
                            let on = !app.settings.notify;
                            app.settings.set_notify(on);
                        })
                        .build(p.k)
                        .key("notify-switch"),
                    p,
                ),
                rule("notifications-sep", false, p),
                setting_row(
                    "When a decision waits on me",
                    Switch::new(app.settings.ask)
                        .label("When a decision waits on me")
                        .hide_label()
                        .on_toggle(|app: &mut App| {
                            let on = !app.settings.ask;
                            app.settings.set_ask(on);
                        })
                        .build(p.k)
                        .key("ask-switch"),
                    p,
                ),
            ],
            p,
        ))
        .child(settings_section(
            "packs",
            "Packs",
            vec![setting_row(
                "Editor",
                SegmentedControl::new(
                    &["Auto", "VS Code", "Cursor", "Zed"],
                    Some(
                        app.settings
                            .editor
                            .and_then(|e| Editor::ALL.iter().position(|x| *x == e))
                            .map_or(0, |i| i + 1),
                    ),
                )
                .label("Editor")
                .width(Some(320.0))
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                    app.settings
                        .set_editor(i.checked_sub(1).and_then(|i| Editor::ALL.get(i).copied()));
                })
                .build(p.k)
                .key("editor"),
                p,
            )],
            p,
        ))
        .child(settings_section("log-viewer", "Log font", rows, p))
        .child(settings_section("keys", "Keys", key_rows(app, p), p))
}

/// A row per command: its first key in a recorder, any others beside it, and a reset when the
/// config file binds it; the bindings the config file could not use come first.
fn key_rows(app: &App, p: &Paint) -> Vec<El> {
    let mut rows: Vec<El> = app
        .keys_refused
        .iter()
        .map(|refused| {
            text(format!(
                "{} {}: {}",
                refused.command, refused.key, refused.why
            ))
            .font(p.k.ty.small)
            .color(p.fail)
            .py(6.0)
        })
        .collect();
    for (i, command) in crate::keymap::Command::ALL.into_iter().enumerate() {
        if i > 0 {
            rows.push(rule(&format!("keys-sep-{i}"), false, p));
        }
        let state = app.recorders.get(&command).cloned().unwrap_or_else(|| {
            rune_kit::ShortcutRecorderState::new(app.keymap.keys(command).first().cloned())
        });
        let others: Vec<String> = app
            .keymap
            .keys(command)
            .iter()
            .skip(1)
            .map(|key| key.keycaps().join(""))
            .collect();
        let id = command.id();
        let recorder =
            rune_kit::ShortcutRecorder::new(format!("key-{id}"), &state, move |app: &mut App| {
                app.recorder(command)
            })
            .label(command.title())
            .allow_unmodified()
            .width(200.0)
            .on_change(move |app: &mut App, shortcut, _: &mut Tx| app.rebind(command, shortcut))
            .build(p.k);
        let mut control = row().gap(8.0).center_y();
        if !others.is_empty() {
            control = control.child(
                text(others.join("  "))
                    .font(mono(11.0, 400.0))
                    .color(p.c.text_faint),
            );
        }
        control = control.child(recorder);
        if app.settings.keys.contains_key(&id) {
            control = control.child(
                Button::new("Reset")
                    .variant(ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.reset_keys(command))
                    .build(p.k)
                    .key(format!("key-reset-{id}")),
            );
        }
        rows.push(setting_row(&command.title(), control, p));
    }
    rows
}

/// A titled card of rows, as a settings page lays them out.
fn settings_section(key: &str, title: &str, rows: Vec<El>, p: &Paint) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .max_w(720.0)
        .gap(10.0)
        .child(text(title.to_string()).font(SECTION).color(p.c.text_base))
        .child(
            col()
                .key(format!("{key}-card"))
                .w_full()
                .px(16.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .children(rows),
        )
}

fn setting_row(label: &str, control: El, p: &Paint) -> El {
    row()
        .w_full()
        .py(12.0)
        .gap(16.0)
        .center_y()
        .child(
            text(label.to_string())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .grow(1.0),
        )
        .child(control)
}

fn font_option(font: LogFont, app: &App, p: &Paint) -> El {
    let available = font != LogFont::Tx02 || app.settings.tx02.is_some();
    let selected = app.settings.log_font == font;
    let mut option = row()
        .key(format!(
            "font-{}",
            font.label().to_lowercase().replace(' ', "-")
        ))
        .role("radio")
        .label(font.label())
        .checked(selected)
        .w_full()
        .py(12.0)
        .gap(12.0)
        .center_y();
    if available {
        option = option
            .focusable(true)
            .on_press(move |app: &mut App| app.set_log_font(font));
    }

    let mut radio = el().size(16.0, 16.0).radius(8.0).center().border(
        1.0,
        if selected {
            p.c.bg_accent
        } else {
            p.c.border_strong
        },
    );
    if selected {
        radio = radio
            .fill(p.c.bg_accent)
            .child(el().size(6.0, 6.0).radius(3.0).fill(p.c.text_contrast));
    }

    let about = match (&app.settings.tx02, font, available) {
        (_, _, false) => "Not installed on this machine.".to_string(),
        (Some(path), LogFont::Tx02, true) => format!("{}\n{}", font.detail(), path.display()),
        _ => font.detail().to_string(),
    };
    option
        .help(about)
        .child(radio)
        .child(
            text(font.label())
                .font(p.k.ty.ui_medium)
                .color(if available {
                    p.c.text_base
                } else {
                    p.c.text_faint
                })
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            text("Aa 0O 1lI {}")
                .font(TextStyle::new(13.0, 400.0).family(font.family()))
                .color(if available {
                    p.c.text_muted
                } else {
                    p.c.text_faint
                }),
        )
}

fn empty_run(pane: &Pane, p: &Paint) -> El {
    col()
        .key("empty")
        .w_full()
        .padding(24.0)
        .gap(6.0)
        .items_center()
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(icon(&oc::CHECKLIST, 22.0).color(p.c.icon_faint))
        .child(
            text(pane.empty.0)
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .wrap(),
        )
        .child(
            text(pane.empty.1.clone())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .wrap(),
        )
}

fn banner(message: &str, p: &Paint) -> El {
    let s = p.tone(Tone::Danger);
    row()
        .key("banner")
        .w_full()
        .px(12.0)
        .py(10.0)
        .gap(8.0)
        .center_y()
        .radius(p.k.radius.lg)
        .fill(s.bg)
        .border(p.hair, s.border)
        .child(icon(&oc::WARNING, 14.0).color(s.icon))
        .child(
            text(message.to_string())
                .font(p.k.ty.ui)
                .color(s.fg)
                .grow(1.0)
                .wrap(),
        )
}
