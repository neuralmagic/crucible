//! The Admin page: the controller's settings and their overrides, the autopilot flag, who has
//! access, and the repos discovery sweeps.

use crux::dto::{Autopilot, Knob, WatchedRepo};
use rune::{El, col, row, text};
use rune_kit::{Button, ButtonVariant, TextInput};

use crate::App;
use crate::admin::{Action, relative};
use crate::overrides::{parse, shown, with, without};
use crate::ui::desk::{
    card, confirm, fact, field, frame, heading, landed, none, now, scrolled, stacked,
};
use crate::ui::{Paint, tag};

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let admin = &app.remote.admin;
    let reason = card(
        "admin-why-card",
        field(
            "Reason for a change",
            TextInput::new(&app.remote.desk.why)
                .width(Some(480.0))
                .on_edit(|app: &mut App| &mut app.remote.desk.why)
                .build(p.k)
                .key("admin-why"),
            p,
        ),
        p,
    );
    let section = |key: &str, title: &str, body: El| {
        card(
            key,
            col().w_full().gap(6.0).child(heading(title, p)).child(body),
            p,
        )
    };
    let settings = match landed(&admin.config, "the settings", p) {
        Ok(config) => stacked(
            "admin-knobs",
            config
                .knobs
                .iter()
                .map(|knob| knob_row(knob, &config.knobs, app, p))
                .collect(),
            p,
        ),
        Err(standin) => *standin,
    };
    let autopilot = match landed(&admin.autopilot, "the autopilot flag", p) {
        Ok(flag) => autopilot_row(flag, app, p),
        Err(standin) => *standin,
    };
    let access = match landed(&admin.access, "the access lists", p) {
        Ok(access) => {
            let listed = |names: &[String]| {
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            };
            col()
                .w_full()
                .gap(10.0)
                .child(fact("Admins", "admin-admins", listed(&access.admins), p))
                .child(fact(
                    "Operators",
                    "admin-operators",
                    listed(&access.operators),
                    p,
                ))
                .child(fact(
                    "Repo orgs",
                    "admin-orgs",
                    listed(&access.allowed_orgs),
                    p,
                ))
        }
        Err(standin) => *standin,
    };
    let repos = match landed(&admin.repos, "the repos", p) {
        Ok(repos) => repos_list(repos, app, p),
        Err(standin) => *standin,
    };
    frame("admin", "Admin", String::new(), None, app, p).child(scrolled(
        "admin-scroll",
        col()
            .key("admin-body")
            .w_full()
            .gap(16.0)
            .child(reason)
            .child(section("admin-autopilot-card", "Autopilot", autopilot))
            .child(section("admin-settings-card", "Settings", settings))
            .child(section("admin-repos-card", "Repos", repos))
            .child(section("admin-access-card", "Access", access)),
        p,
    ))
}

fn why(app: &App) -> String {
    app.remote.desk.why.text.trim().to_string()
}

fn knob_row(knob: &Knob, knobs: &[Knob], app: &App, p: &Paint) -> El {
    let editing = app
        .remote
        .desk
        .knob
        .as_ref()
        .filter(|(name, _)| name == &knob.name);
    let overridden = knob.source == "override";
    let mut line = row()
        .key(format!("knob-{}", knob.name))
        .w_full()
        .py(8.0)
        .gap(10.0)
        .center_y()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(knob.name.clone())
                                .font(p.mono(12.0, 520.0))
                                .color(p.c.text_base),
                        )
                        .child(tag(&knob.source, p)),
                )
                .child(
                    text(knob.description.clone())
                        .font(p.k.ty.small)
                        .color(p.c.text_muted)
                        .truncate()
                        .help(knob.description.clone()),
                ),
        );
    let Some((_, typed)) = editing else {
        line = line.child(
            text(shown(&knob.value))
                .font(p.mono(12.0, 480.0))
                .color(p.c.text_base),
        );
        if !knob.overridable {
            return line;
        }
        let (name, value) = (knob.name.clone(), shown(&knob.value));
        line = line.child(
            Button::new("Override")
                .variant(ButtonVariant::Ghost)
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    app.remote.desk.knob = Some((name.clone(), rune::TextEdit::new(&value)));
                })
                .build(p.k)
                .key(format!("knob-edit-{}", knob.name)),
        );
        if overridden {
            let (rest, why) = (without(knobs, &knob.name), why(app));
            line = line.child(
                Button::new("Reset")
                    .variant(ButtonVariant::Ghost)
                    .disabled(why.is_empty() || app.remote.desk.busy())
                    .on_press(move |app: &mut App| {
                        app.act(Action::SetOverrides {
                            overrides: rest.clone(),
                            justification: why.clone(),
                        })
                    })
                    .build(p.k)
                    .key(format!("knob-reset-{}", knob.name)),
            );
        }
        return line;
    };
    let read = parse(&knob.value, &typed.text);
    let next = with(knobs, &knob.name, &typed.text);
    let why = why(app);
    let ready = next.is_ok() && !why.is_empty() && !app.remote.desk.busy();
    line.child(
        TextInput::new(typed)
            .width(Some(180.0))
            .invalid(read.is_err())
            .on_edit(|app: &mut App| match &mut app.remote.desk.knob {
                Some((_, typed)) => typed,
                None => &mut app.scratch,
            })
            .build(p.k)
            .key(format!("knob-value-{}", knob.name))
            .help(read.err().unwrap_or_default()),
    )
    .child(
        Button::new("Save")
            .variant(ButtonVariant::Submit)
            .disabled(!ready)
            .on_press(move |app: &mut App| {
                if let Ok(overrides) = &next {
                    app.act(Action::SetOverrides {
                        overrides: overrides.clone(),
                        justification: why.clone(),
                    });
                }
            })
            .build(p.k)
            .key(format!("knob-save-{}", knob.name)),
    )
    .child(
        Button::new("Cancel")
            .variant(ButtonVariant::Ghost)
            .on_press(|app: &mut App| app.remote.desk.knob = None)
            .build(p.k)
            .key(format!("knob-cancel-{}", knob.name)),
    )
}

fn autopilot_row(flag: &Autopilot, app: &App, p: &Paint) -> El {
    let mut said = vec![if flag.enabled { "on" } else { "off" }.to_string()];
    if let Some(by) = &flag.changed_by {
        said.push(format!("by {by}"));
    }
    if let Some(at) = &flag.changed_at {
        said.push(relative(at, now()));
    }
    if let Some(reason) = flag.reason.as_ref().filter(|r| !r.is_empty()) {
        said.push(reason.clone());
    }
    let (enabled, why) = (flag.enabled, why(app));
    let reason = why.clone();
    row()
        .key("admin-autopilot")
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(said.join(" \u{b7} "))
                .font(p.k.ty.ui)
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
        .child(
            Button::new(if enabled { "Turn off" } else { "Turn on" })
                .variant(ButtonVariant::Neutral)
                .disabled(why.is_empty() || app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    app.act(Action::SetAutopilot {
                        enabled: !enabled,
                        reason: reason.clone(),
                    })
                })
                .build(p.k)
                .key("admin-autopilot-toggle"),
        )
        .child(
            Button::new("Reconcile now")
                .variant(ButtonVariant::Ghost)
                .disabled(app.remote.desk.busy())
                .on_press(|app: &mut App| app.act(Action::Reconcile))
                .build(p.k)
                .key("admin-reconcile"),
        )
}

fn repos_list(repos: &[WatchedRepo], app: &App, p: &Paint) -> El {
    let now = now();
    let mut sorted: Vec<&WatchedRepo> = repos.iter().collect();
    sorted.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.repo.cmp(&b.repo)));
    let rows: Vec<El> = sorted
        .into_iter()
        .map(|repo| repo_row(repo, now, app, p))
        .collect();
    let listed = if rows.is_empty() {
        none("admin-repos-none", "None", p)
    } else {
        stacked("admin-repos", rows, p)
    };
    let typed = app.remote.desk.new_repo.text.trim().to_string();
    let why = why(app);
    let ready = typed.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty() && !name.is_empty() && !typed.contains(char::is_whitespace)
    }) && !why.is_empty()
        && !app.remote.desk.busy();
    col().w_full().gap(10.0).child(listed).child(
        row()
            .gap(8.0)
            .center_y()
            .child(
                TextInput::new(&app.remote.desk.new_repo)
                    .placeholder("owner/name")
                    .width(Some(320.0))
                    .on_edit(|app: &mut App| &mut app.remote.desk.new_repo)
                    .build(p.k)
                    .key("admin-repo-name"),
            )
            .child(
                Button::new("Watch")
                    .variant(ButtonVariant::Submit)
                    .disabled(!ready)
                    .on_press(move |app: &mut App| {
                        app.act(Action::AddRepo {
                            repo: typed.clone(),
                            justification: why.clone(),
                        })
                    })
                    .build(p.k)
                    .key("admin-repo-add"),
            ),
    )
}

fn repo_row(repo: &WatchedRepo, now: i64, app: &App, p: &Paint) -> El {
    let mut counts = vec![format!("{} total", repo.total)];
    for (n, what) in [
        (repo.running, "running"),
        (repo.pr_open, "PRs open"),
        (repo.parked, "parked"),
        (repo.done, "done"),
    ] {
        if n > 0 {
            counts.push(format!("{n} {what}"));
        }
    }
    if let Some(at) = &repo.watermark {
        counts.push(format!("swept {}", relative(at, now)));
    }
    let mut line = row()
        .key(format!("repo-{}", repo.repo))
        .w_full()
        .py(8.0)
        .gap(10.0)
        .center_y()
        .child(
            text(repo.repo.clone())
                .font(p.mono(12.0, 520.0))
                .color(p.c.text_base),
        )
        .children(repo.paused.then(|| tag("paused", p)))
        .child(
            text(counts.join(" \u{b7} "))
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if !repo.watched || !repo.repo.contains('/') {
        return line;
    }
    let (name, paused) = (repo.repo.clone(), repo.paused);
    line = line.child(
        Button::new(if paused { "Resume" } else { "Pause" })
            .variant(ButtonVariant::Ghost)
            .disabled(app.remote.desk.busy())
            .on_press(move |app: &mut App| {
                app.act(Action::PauseRepo {
                    repo: name.clone(),
                    paused: !paused,
                })
            })
            .build(p.k)
            .key(format!("repo-pause-{}", repo.repo)),
    );
    line.child(confirm(
        format!("repo-unwatch-{}", repo.repo),
        &format!("repo-unwatch:{}", repo.repo),
        ("Unwatch", "Confirm unwatch"),
        true,
        Action::UnwatchRepo {
            repo: repo.repo.clone(),
        },
        app,
        p,
    ))
}
