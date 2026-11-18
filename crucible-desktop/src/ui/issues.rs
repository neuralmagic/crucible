//! The Issues page: the issues the controller works for itself, one in full, and parking it.

use crux::dto::{Issue, IssueDetail};
use rune::{El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, SidebarRow, TextInput};

use crate::App;
use crate::admin::{Action, relative};
use crate::issues::{STATUSES, newest, source};
use crate::ui::desk::{
    card, fact, field, frame, heading, landed, mono, none, now, pills, reading, split, stacked,
};
use crate::ui::{Paint, tag};

const LIST: f32 = 380.0;
const ALL: &str = "all";

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let status = &app.remote.desk.issue_status;
    let listed = app
        .remote
        .admin
        .issues
        .as_ref()
        .filter(|(of, _)| of == status)
        .map(|(_, issues)| issues);
    let note = match listed {
        Some(Some(Ok(issues))) if issues.len() > crate::issues::SHOWN => {
            format!("newest {} of {}", crate::issues::SHOWN, issues.len())
        }
        Some(Some(Ok(issues))) => issues.len().to_string(),
        _ => String::new(),
    };
    let options: Vec<(String, String)> = std::iter::once(ALL)
        .chain(STATUSES)
        .map(|status| (status.to_string(), status.replace('-', " ")))
        .collect();
    let page = frame("issues", "Issues", note, None, app, p).child(pills(
        "issues-status",
        &options,
        Some(status.as_deref().unwrap_or(ALL)),
        |app: &mut App, picked: &str| {
            app.remote.desk.issue_status = (picked != ALL).then(|| picked.to_string());
        },
        p,
    ));
    let issues = match listed {
        None => return page.child(reading("the issues", p)),
        Some(issues) => match landed(issues, "the issues", p) {
            Err(standin) => return page.child(*standin),
            Ok(issues) => issues,
        },
    };
    if issues.is_empty() {
        return page.child(none("issues-none", "No issues", p));
    }
    let now = now();
    let (shown, _) = newest(issues);
    let picked = app.remote.desk.issue.as_deref();
    let rows = shown
        .iter()
        .map(|issue| {
            let key = issue.key.clone();
            SidebarRow::new(issue.title.clone().unwrap_or_else(|| source(issue)))
                .subtitle(format!(
                    "{} \u{b7} {} \u{b7} {}",
                    issue.status,
                    source(issue),
                    relative(&issue.updated_at, now)
                ))
                .height(46.0)
                .selected(picked == Some(issue.key.as_str()))
                .on_select(move |app: &mut App, _: &mut Tx| {
                    app.remote.desk.armed = None;
                    app.remote.desk.issue = Some(key.clone());
                })
                .build(p.k)
                .key(format!("issue-{}", issue.key))
        })
        .collect();
    let shown = match (picked, &app.remote.admin.issue) {
        (None, _) => none("issue-none", "Pick an issue", p),
        (Some(key), Some((of, detail))) if of == key => match landed(detail, "the issue", p) {
            Ok(detail) => self::detail(detail, app, p),
            Err(standin) => *standin,
        },
        (Some(_), _) => reading("the issue", p),
    };
    page.child(split("issues", rows, LIST, shown, p))
}

fn detail(detail: &IssueDetail, app: &App, p: &Paint) -> El {
    let issue = &detail.issue;
    let mut head = row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(issue.title.clone().unwrap_or_else(|| source(issue)))
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .wrap(),
        )
        .child(tag(&issue.status, p));
    if let Some(tier) = &issue.tier {
        head = head.child(tag(tier, p));
    }
    let links: Vec<&str> = [&issue.evidence_url, &issue.pr_url]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut facts = col()
        .key("issue-facts")
        .w_full()
        .gap(12.0)
        .child(head)
        .child(fact("Key", "issue-key", issue.key.clone(), p));
    if source(issue) != issue.key {
        facts = facts.child(fact("Source", "issue-source", source(issue), p));
    }
    if !links.is_empty() {
        facts = facts.child(fact("Links", "issue-links", links.join("\n"), p));
    }
    if let Some(reason) = &issue.parked_reason {
        let by = issue.parked_by.as_deref().unwrap_or("unknown");
        facts = facts.child(fact(
            &format!("Parked by {by}"),
            "issue-parked",
            reason.render(),
            p,
        ));
    }
    let mut body = col()
        .key("issue-detail")
        .w_full()
        .gap(16.0)
        .child(card("issue-facts-card", facts, p))
        .child(card("issue-park-card", park(issue, app, p), p));
    if let Some(text_) = detail.body.as_ref().filter(|body| !body.trim().is_empty()) {
        body = body.child(card(
            "issue-body-card",
            col()
                .w_full()
                .gap(8.0)
                .child(heading("Body", p))
                .child(mono("issue-body", text_.clone(), p)),
            p,
        ));
    }
    if !detail.events.is_empty() {
        let now = now();
        let rows = detail
            .events
            .iter()
            .rev()
            .enumerate()
            .map(|(n, event)| {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|reason| reason.render())
                    .unwrap_or_default();
                col()
                    .key(format!("issue-event-{n}"))
                    .w_full()
                    .py(8.0)
                    .gap(4.0)
                    .child(
                        row()
                            .w_full()
                            .gap(10.0)
                            .child(
                                text(format!("{} \u{2192} {}", event.from, event.to))
                                    .font(p.k.ty.ui)
                                    .color(p.c.text_base),
                            )
                            .child(el().grow(1.0))
                            .child(
                                text(
                                    [
                                        event.actor.clone().unwrap_or_default(),
                                        relative(&event.ts, now),
                                    ]
                                    .into_iter()
                                    .filter(|part| !part.is_empty())
                                    .collect::<Vec<_>>()
                                    .join(" \u{b7} "),
                                )
                                .font(p.k.ty.small)
                                .color(p.c.text_muted),
                            ),
                    )
                    .children(
                        (!reason.is_empty())
                            .then(|| text(reason).font(p.k.ty.small).color(p.c.text_muted).wrap()),
                    )
            })
            .collect();
        body = body.child(card(
            "issue-events-card",
            col()
                .w_full()
                .gap(6.0)
                .child(heading("History", p))
                .child(stacked("issue-events", rows, p)),
            p,
        ));
    }
    body
}

fn park(issue: &Issue, app: &App, p: &Paint) -> El {
    let key = issue.key.clone();
    if issue.status == "parked" {
        return row().key("issue-park").child(
            Button::new("Unpark")
                .variant(ButtonVariant::Neutral)
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| app.act(Action::UnparkIssue { key: key.clone() }))
                .build(p.k)
                .key("issue-unpark"),
        );
    }
    let ready = !app.remote.desk.why.text.trim().is_empty() && !app.remote.desk.busy();
    row()
        .key("issue-park")
        .w_full()
        .gap(10.0)
        .items_end()
        .child(field(
            "Reason",
            TextInput::new(&app.remote.desk.why)
                .width(Some(360.0))
                .on_edit(|app: &mut App| &mut app.remote.desk.why)
                .build(p.k)
                .key("issue-park-reason"),
            p,
        ))
        .child(
            Button::new("Park")
                .variant(ButtonVariant::Neutral)
                .disabled(!ready)
                .on_press(move |app: &mut App| {
                    let reason = app.remote.desk.why.text.trim().to_string();
                    app.act(Action::ParkIssue {
                        key: key.clone(),
                        reason,
                    });
                })
                .build(p.k)
                .key("issue-park-go"),
        )
}
