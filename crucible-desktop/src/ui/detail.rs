//! The selected pack or launch: its header, its figures and its panes.

use crate::live::{LiveDetail, LiveDraft, LivePlaybook, LiveView};
use crate::logline::activity;
use crate::model::{PackView, Verdict};
use crate::pack::PackFiles;
use crate::session::TaskState;
use crate::settings::LogFont;
use crate::ui::settings::settings_pane;
use crate::ui::sidebar::{draft_meta, verdict_color, verdict_icon};
use crate::ui::{
    Flight, Paint, Pane, SECTION, TITLE, TITLE_BAND, admin, approvals, banner, blueprints,
    decisions, dock, issues, keys, launcher, meter, providers, pulsing, report, runs, secrets,
    shares, tasks, teams, title_flight, triggers,
};
use crate::{App, Backend, MAX_TIME, Selection};
use crucible_contract::session::TaskStatus;
use rune::{Color, El, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, RollingText, Spinner,
};
use rune_theme_opencode::icons as oc;

const VALUE: TextStyle = TextStyle::new(15.0, 600.0).lh(20.0).features("'tnum' 1");

fn local_pane(pack: &PackView, log_font: LogFont) -> Pane<'_> {
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
        tasks: &pack.tasks,
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
        log: &pack.log,
        transcript: &pack.transcript,
        agent_events: &pack.agent_events,
        log_font,
        launchable: true,
        preview: Some(&pack.preview),
        previewing: false,
        run_tasks: &[],
        calls: &[],
        node_log: None,
        pack: Ok(&pack.pack),
        launch: None,
        run_id: None,
        files: Some(Err(
            "A local run keeps them in its pack's state/files folder.".to_string(),
        )),
        empty: (
            "No run recorded here yet.",
            "Run it with the button above, or from a shell with `crucible plan run`.".to_string(),
        ),
    }
}

/// `pinned` is a launch opened from the runs page, which the breadcrumb already names.
fn live_pane<'a>(
    launch: &LivePlaybook,
    detail: Option<&'a LiveDetail>,
    log_font: LogFont,
    pinned: bool,
) -> Pane<'a> {
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
        tasks: detail.map_or(&[], |detail| &detail.fold.tasks),
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
        log: detail.map_or(&[], |detail| &detail.log),
        transcript: detail.map_or(&[], |detail| &detail.fold.transcript),
        agent_events: detail.map_or(&[], |detail| &detail.fold.agent_events),
        log_font,
        launchable: false,
        preview: None,
        previewing: false,
        run_tasks: &[],
        calls: detail.map_or(&[], |detail| &detail.calls),
        node_log: detail.and_then(|detail| detail.node_log.as_ref()),
        empty: live_empty(launch, detail),
        pack: detail.map_or_else(
            || Err("Reading the pack\u{2026}".to_string()),
            |detail| detail.pack.as_ref().map_err(Clone::clone),
        ),
        launch: detail.and_then(|detail| detail.launch.as_ref()),
        run_id: detail.and_then(|detail| detail.run_id.clone()),
        files: detail.and_then(|detail| {
            detail
                .files
                .as_ref()
                .map(|files| files.as_ref().map(Vec::as_slice).map_err(Clone::clone))
        }),
    }
}

/// A draft no launch ran: its newest save, and nothing of a run.
fn draft_pane<'a>(
    draft: &LiveDraft,
    pack: Option<&'a Result<PackFiles, String>>,
    log_font: LogFont,
) -> Pane<'a> {
    Pane {
        title: draft.id.clone(),
        subtitle: draft
            .description
            .clone()
            .unwrap_or_else(|| format!("{} \u{b7} by {}", draft_meta(draft), draft.by)),
        path: format!("draft {} v{}", draft.id, draft.version),
        verdict: Verdict::NotRun,
        spent: 0.0,
        cap: 0.0,
        passed: 0,
        failed: 0,
        tasks: &[],
        time: "\u{2014}".to_string(),
        time_note: None,
        meta: Some(draft.owner.clone()),
        streaming: false,
        problem: None,
        log: &[],
        transcript: &[],
        agent_events: &[],
        log_font,
        launchable: false,
        preview: None,
        previewing: false,
        run_tasks: &[],
        calls: &[],
        node_log: None,
        empty: ("No launches listed.", String::new()),
        pack: match pack {
            Some(pack) => pack.as_ref().map_err(Clone::clone),
            None if draft.version > 0 => Err("Reading the pack\u{2026}".to_string()),
            None => Err("Nothing saved.".to_string()),
        },
        launch: None,
        run_id: None,
        files: Some(Ok(&[])),
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

pub(crate) fn runs_note(runs: i64) -> String {
    match runs {
        0 => "no runs".to_string(),
        1 => "1 run".to_string(),
        n => format!("{n} runs"),
    }
}

pub(crate) fn detail(app: &App, p: &Paint, layer: &mut Vec<El>) -> El {
    match app.page {
        crate::Page::Settings => return settings_pane(app, p),
        crate::Page::Triggers => return triggers::page(app, p),
        crate::Page::Secrets => return secrets::page(app, p),
        crate::Page::Decisions => return decisions::page(app, p),
        crate::Page::Approvals => return approvals::page(app, p),
        crate::Page::Teams => return teams::page(app, p),
        crate::Page::Providers => return providers::page(app, p),
        crate::Page::Blueprints => return blueprints::page(app, p),
        crate::Page::Activity => return crate::ui::activity::page(app, p),
        crate::Page::Issues => return issues::page(app, p),
        crate::Page::Admin => return admin::page(app, p),
        crate::Page::Keys => return keys::page(app, p),
        crate::Page::Runs => {}
    }
    if let (Some((playbook, launches)), LiveView::Ready { .. }) =
        (app.runs_page(), &*app.remote.live)
    {
        let view = app.runs_view().map(|built| {
            let drawn = (app.runs_tab == crate::RunsTab::Report)
                .then(|| report::runs_report_view(built, app, p, layer));
            (app.runs_tab, drawn)
        });
        return runs::runs_page(
            launcher::launch_button(app, playbook, p),
            shares::section(app, p),
            playbook,
            &launches,
            app,
            view,
            p,
        );
    }
    let font = app.settings.log_font;
    let pane = match &app.selection {
        Some(Selection::Local(_)) => app.selected_pack().map(|pack| local_pane(pack, font)),
        Some(Selection::Live { launch, .. }) => {
            let pinned = launch.is_some() && app.playbook_launches().is_some();
            app.selected_live()
                .map(|live| live_pane(live, app.selected_detail(), font, pinned))
                .or_else(|| {
                    app.selected_draft()
                        .map(|draft| draft_pane(draft, app.selected_draft_pack(), font))
                })
                .map(|pane| Pane {
                    preview: app.remote.working_preview.as_ref(),
                    ..pane
                })
        }
        None => None,
    }
    .map(|pane| pane.previewed(app.remote.graph_source));

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
                    .font(p.sized(SECTION))
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
        .child(header(&pane, app, p, layer));

    if let Some(problem) = &pane.problem {
        body = body.child(banner(problem, p));
    }
    if let (Backend::Local, Some(error)) = (app.backend, &app.local_error) {
        body = body.child(banner(error, p));
    }
    if let Some(note) = cancel_note(app, p) {
        body = body.child(note);
    }
    if let Some(note) = &app.run.handoff_note {
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

    let stats = match app.selected_live() {
        Some(launch) => {
            let landing = app
                .flight
                .clone()
                .filter(|id| id.starts_with(&format!("run-{}-", launch.key)))
                .unwrap_or_else(|| Flight::Row.id(&launch.key));
            stats(&pane, p).matched(landing)
        }
        None => stats(&pane, p),
    };
    body.children(app.selected_draft().is_none().then_some(stats))
        .child(dock::workspace(&pane, app, p, layer))
}

/// Stops the launch on screen while it runs, after a confirming second press.
fn cancel_button(app: &App, p: &Paint) -> Option<El> {
    let launch = app
        .selected_live()
        .filter(|l| l.verdict() == crate::model::Verdict::Running)?;
    let key = launch.key.clone();
    let armed_key = format!("cancel:{key}");
    let armed = app.remote.desk.armed.as_deref() == Some(armed_key.as_str());
    Some(
        Button::new(if armed { "Confirm cancel" } else { "Cancel" })
            .variant(if armed {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Ghost
            })
            .disabled(app.remote.desk.busy())
            .on_press(move |app: &mut App| {
                if app.remote.desk.armed.as_deref() == Some(armed_key.as_str()) {
                    app.act(crate::admin::Action::CancelLaunch { key: key.clone() });
                } else {
                    app.remote.desk.armed = Some(armed_key.clone());
                }
            })
            .build(p.k)
            .key("cancel-launch"),
    )
}

/// What the last cancel came to, under the run header.
fn cancel_note(app: &App, p: &Paint) -> Option<El> {
    let acted = app.remote.admin.acted.as_ref()?;
    if acted.did != crate::admin::Did::CancelLaunch {
        return None;
    }
    let (line, color) = match (&acted.result, app.remote.desk.busy()) {
        (_, true) => ("Cancelling\u{2026}".to_string(), p.c.text_muted),
        (Ok(_), false) => (
            "Cancel queued: the launch is parked and its pods are being deleted".to_string(),
            p.c.text_muted,
        ),
        (Err(why), false) => (why.clone(), p.fail),
    };
    Some(
        text(line)
            .font(p.k.ty.small)
            .color(color)
            .truncate()
            .key("cancel-note"),
    )
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

fn header(pane: &Pane, app: &App, p: &Paint, layer: &mut Vec<El>) -> El {
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
                        .font(p.sized(TITLE))
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection())
                        .help(about),
                )
                .children(pane.streaming.then(|| {
                    pulsing(
                        p,
                        el().size(7.0, 7.0).radius(3.5).fill(p.live),
                        p.live,
                        12.0,
                    )
                    .help("Streaming")
                }))
                .child(el().grow(1.0))
                .child(dock::tools(app, p, layer))
                .children(link_button(app, p))
                .children(cancel_button(app, p))
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
        .children(called_by(app, p))
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

fn called_by(app: &App, p: &Paint) -> Option<El> {
    let by = app.selected_live()?.called_by.clone()?;
    let caller = app.remote.live.launch(&by.launch).map_or_else(
        || by.launch.clone(),
        |launch| format!("{} {}", launch.id, launch.landed_at),
    );
    let task = match &by.instance {
        Some(key) => format!("{}[{key}]", by.task),
        None => by.task.clone(),
    };
    let (whole, at) = (by.launch.clone(), by.launch.clone());
    let open_task = by.task.clone();
    Some(
        row()
            .key("called-by")
            .gap(4.0)
            .center_y()
            .child(text("called by").font(p.k.ty.small).color(p.c.text_faint))
            .child(
                Button::new(caller)
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |app: &mut App| app.open_launch(&whole, None))
                    .build(p.k)
                    .key("called-by-run"),
            )
            .child(icon(&oc::CHEVRON_RIGHT, 12.0).color(p.c.icon_faint))
            .child(
                Button::new(task)
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |app: &mut App| app.open_launch(&at, Some(&open_task)))
                    .build(p.k)
                    .key("called-by-task"),
            ),
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
    let (playbook, _) = app.playbook_launches()?;
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
                    .key("back-to-runs")
                    .matched(title_flight(playbook)),
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
        .px(12.0)
        .py(8.0)
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
    let glyph = verdict_icon(pane.verdict);
    stat_card("status", p).child(
        row()
            .w_full()
            .gap(6.0)
            .center_y()
            .child(stat_label("Status", p).grow(1.0))
            .children(match pane.verdict {
                Verdict::Running => Some(Spinner::new().size(14.0).color(ink).build(p.k)),
                _ => glyph.map(|glyph| icon(glyph, 14.0).color(ink)),
            })
            .child(
                text(pane.verdict.label())
                    .font(p.sized(VALUE))
                    .color(if glyph.is_some() { ink } else { p.c.text_muted })
                    .truncate(),
            ),
    )
}

pub(crate) fn stat(
    key: &str,
    label: &str,
    value: &str,
    ink: Color,
    bar: Option<El>,
    sub: &str,
    p: &Paint,
) -> El {
    let detail = (bar.is_some() || !sub.is_empty()).then(|| {
        row()
            .w_full()
            .center_y()
            .gap(8.0)
            .children(bar)
            .children((!sub.is_empty()).then(|| stat_sub(sub, p)))
    });
    stat_card(key, p)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(stat_label(label, p).grow(1.0).min_w(0.0))
                .child(
                    RollingText::new(format!("stat-{key}-value"), value)
                        .font(p.sized(VALUE))
                        .color(ink)
                        .build(p.k),
                ),
        )
        .children(detail)
}

/// Passed over planned, with a bar split by where each task stands.
fn tasks_stat(pane: &Pane, p: &Paint) -> El {
    let total = pane.tasks.len().max(1) as f32;
    let mut counts = [0usize; 4];
    for task in pane.tasks {
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

pub(crate) fn empty_run(pane: &Pane, p: &Paint) -> El {
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
