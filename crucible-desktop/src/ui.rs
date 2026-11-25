//! The window: live playbooks and local packs on the left, the selected run on the right.

mod about;
mod account;
mod activity;
mod admin;
mod agent;
mod approvals;
mod blueprints;
mod compare;
mod data;
mod decisions;
mod desk;
mod detail;
mod dock;
mod files;
mod finder;
mod graph;
mod graph_call;
mod graph_find;
mod issues;
mod keys;
mod launch;
mod launcher;
mod log;
mod pack;
mod palette;
mod policy;
mod profiles;
mod providers;
mod rail;
mod report;
mod revisions;
mod runs;
mod schedules;
mod secrets;
mod settings;
mod shares;
mod sidebar;
mod slider;
mod tasks;
mod teams;
mod theme;
mod timeline;
mod transcript;
mod triggers;
mod webhook_form;

use rune::{Color, Cx, El, Icon, TextStyle, Tx, el, icon, row, text};

use rune_kit::{Badge, IconButton, IconButtonVariant, Kit, Palette, StateColors};
use rune_theme_opencode::icons as oc;

use crate::logline::LogLine;
use crate::model::Verdict;
use crate::pack::PackFiles;
use crate::session::TaskView;
use crate::settings::LogFont;
use crate::ui::detail::detail;
use crate::ui::sidebar::sidebar;
use crate::{App, gfx};

/// The band across the top that holds the window buttons and drags the window.
const TITLE_BAND: f32 = 52.0;
#[cfg(not(target_arch = "wasm32"))]
/// The close button's top-left, centring the buttons in the title band.
#[cfg(not(feature = "hot"))]
pub const TRAFFIC_LIGHTS: (f32, f32) = (18.0, 19.0);
const TITLE: TextStyle = TextStyle::new(20.0, 640.0).lh(26.0).tracking(-0.2);
const SECTION: TextStyle = TextStyle::new(13.0, 560.0).lh(16.0);
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
    tune: crate::tune::Tune,
    /// What app-set text sizes are multiplied by.
    text: f32,
}

impl Paint {
    /// `style` at the text size the user picked.
    fn sized(&self, style: TextStyle) -> TextStyle {
        theme::scaled(style, self.text)
    }

    /// A monospaced style at `size` points, scaled to the picked text size.
    fn mono(&self, size: f32, weight: f32) -> TextStyle {
        self.sized(TextStyle::new(size, weight).mono())
    }
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
    let k = theme::kit(app.settings.theme, dark, app.settings.text_size);
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
        tune: crate::tune::Tune::read(&app.knobs),
        text: app.settings.text_size.factor(),
    }
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
fn pulsing(p: &Paint, mark: El, color: Color, size: f32) -> El {
    row()
        .size(size, size)
        .center()
        .radius(size / 2.0)
        .child(
            fx(
                gfx::PULSE,
                gfx::data(color, p.tune.pulse),
                p.tune.pulse_bleed,
            )
            .radius(size / 2.0),
        )
        .child(mark)
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

/// What a run view flies out of on the runs page: the matched id each one carries.
#[derive(Clone, Copy)]
pub(crate) enum Flight {
    Row,
    Dot,
    Cell,
}

impl Flight {
    pub(crate) fn id(self, key: &str) -> String {
        let from = match self {
            Flight::Row => "row",
            Flight::Dot => "dot",
            Flight::Cell => "cell",
        };
        format!("run-{key}-{from}")
    }
}

/// The matched id a playbook's name carries wherever it is a heading or a way back to it.
pub(crate) fn title_flight(playbook: &str) -> String {
    format!("playbook-{playbook}")
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
    cx.zoom(app.settings.zoom);
    let p = palette(app, cx);
    let mut layer = Vec::new();
    let window = row()
        .key("window")
        .w_full()
        .h_full()
        .fill(p.c.bg_base)
        .child(sidebar(app, &p, &mut layer))
        .child(rule("sidebar-edge", true, &p));
    let window = window
        .child(detail(app, &p, &mut layer))
        .children(dock::ghost(app, &p))
        .child(fx(gfx::GRAIN, gfx::data(Color::WHITE, p.tune.grain), 0.0));
    let window = rune_knobs::side(
        window,
        &app.knobs,
        crate::tune::GROUPS,
        if p.dark {
            rune_knobs::Style::dark()
        } else {
            rune_knobs::Style::light()
        },
        |app: &mut App| &mut app.knobs,
    );
    launcher::sheet(app, &p, &mut layer);
    palette::palette(app, &p, &mut layer);
    finder::finder(app, &p, &mut layer);
    let window = window.children(
        (!layer.is_empty()).then(|| el().key(crate::html_preview::OBSCURED).w(0.0).h(0.0)),
    );
    rune_kit::root(window, layer)
}

/// One run, from whichever source, in the shape the detail pane draws.
struct Pane<'a> {
    title: String,
    subtitle: String,
    path: String,
    verdict: Verdict,
    spent: f64,
    cap: f64,
    passed: usize,
    failed: usize,
    tasks: &'a [TaskView],
    time: String,
    time_note: Option<String>,
    meta: Option<String>,
    /// Why a failed run failed: the engine's stderr, an error or its shutdown reason.
    problem: Option<String>,
    /// Its session is streaming in as the engine writes it.
    streaming: bool,
    log: &'a [LogLine],
    /// The agents' session transcript; empty for a run whose engine did not record one.
    transcript: &'a [LogLine],
    /// The same, event by event, for reading a turn.
    agent_events: &'a [crate::session::AgentStep],
    /// Why there is no task list, when there is none.
    empty: (&'static str, String),
    pack: Result<&'a PackFiles, String>,
    /// The launch record, for the Launch tab; local packs have none.
    launch: Option<&'a crux::dto::PlaybookLaunch>,
    /// The run shown, and the files its tasks captured; `None` until listed.
    run_id: Option<String>,
    files: Option<Result<&'a [crate::captured::Captured], String>>,
    log_font: LogFont,
    launchable: bool,
    /// The pack as it compiles now: a local pack's, or the run's draft working copy's.
    preview: Option<&'a crate::preview::Preview>,
    /// `tasks` are the preview's, not the run's.
    previewing: bool,
    /// The run's tasks while the preview's are shown, for marking what the edit changed.
    run_tasks: &'a [TaskView],
    /// The calls the run dispatched, with their targets' progress.
    calls: &'a [crate::live::calls::LiveCall],
}

impl<'a> Pane<'a> {
    /// Show the compiled pack's tasks when they are picked, or when there is no run to show.
    fn previewed(mut self, source: Option<crate::GraphSource>) -> Pane<'a> {
        let Some(compiled) = self.preview.map(|p| p.fold.tasks.as_slice()) else {
            return self;
        };
        if crate::GraphSource::previews(source, !self.tasks.is_empty(), !compiled.is_empty()) {
            self.run_tasks = std::mem::replace(&mut self.tasks, compiled);
            self.previewing = true;
        }
        self
    }

    /// What the agents did: the session transcript when the engine recorded one, otherwise the
    /// console's sampled progress lines.
    fn agent_lines(&self) -> &'a [LogLine] {
        if self.transcript.is_empty() {
            self.log
        } else {
            self.transcript
        }
    }
}

fn note(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.ui)
        .color(p.c.text_muted)
        .px(8.0)
        .py(8.0)
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
                .min_w(0.0)
                .shrink()
                .wrap_anywhere()
                .max_lines(4)
                .help(message.to_string()),
        )
        .child({
            let full = message.to_string();
            IconButton::new(&oc::COPY)
                .variant(IconButtonVariant::GhostMuted)
                .label("Copy")
                .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(full.clone()))
                .build(p.k)
        })
}
