//! The Approvals page: pack imports to register or discard, scope packs waiting on their approval
//! PR, and kept candidate PRs to review.

use rune::{El, TextEdit, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, TextInput};

use crate::App;
use crate::admin::{Action, relative};
use crate::ui::desk::{card, frame, landed, none, now, scrolled, stacked};
use crate::ui::{Paint, Tone, flag, tag};

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.remote.admin.approvals {
        Some(Ok(queue)) => format!(
            "{} imports \u{b7} {} scopes \u{b7} {} kept PRs",
            queue.pending_imports.len(),
            queue.awaiting_approval.len(),
            queue.kept_prs.len()
        ),
        _ => String::new(),
    };
    let page = frame("approvals", "Approvals", note, None, app, p);
    let queue = match landed(&app.remote.admin.approvals, "the approval queue", p) {
        Err(standin) => return page.child(*standin),
        Ok(queue) => queue,
    };
    if queue.pending_imports.is_empty()
        && queue.awaiting_approval.is_empty()
        && queue.kept_prs.is_empty()
    {
        return page.child(none("approvals-none", "Nothing waits on approval", p));
    }
    let now = now();
    let mut sections = col().w_full().gap(18.0);
    if !queue.pending_imports.is_empty() {
        let rows = queue
            .pending_imports
            .iter()
            .map(|import| import_row(import, app, now, p))
            .collect();
        sections = sections.child(section(
            "imports",
            "Pack imports",
            stacked("imports-list", rows, p),
            p,
        ));
    }
    if !queue.awaiting_approval.is_empty() {
        let rows = queue
            .awaiting_approval
            .iter()
            .map(|scope| {
                let mut title = row().gap(8.0).center_y().child(
                    text(scope.key.clone())
                        .font(p.k.ty.ui_medium)
                        .color(p.c.text_base),
                );
                if scope.stale {
                    title = title.child(flag("stale", Tone::Warning, None, p));
                }
                link_row(
                    &format!("scope-{}", scope.scope_id),
                    title,
                    format!("scope {}", scope.scope_id),
                    &scope.approval_pr,
                    p,
                )
            })
            .collect();
        sections = sections.child(section(
            "scopes",
            "Scope packs",
            stacked("scopes-list", rows, p),
            p,
        ));
    }
    if !queue.kept_prs.is_empty() {
        let rows = queue
            .kept_prs
            .iter()
            .enumerate()
            .map(|(i, pr)| {
                link_row(
                    &format!("kept-{i}"),
                    row().child(
                        text(pr_label(&pr.pr_url).unwrap_or_else(|| pr.pr_url.clone()))
                            .font(p.k.ty.ui_medium)
                            .color(p.c.text_base),
                    ),
                    pr.issue.clone(),
                    &pr.pr_url,
                    p,
                )
            })
            .collect();
        sections = sections.child(section(
            "kept",
            "Kept PRs",
            stacked("kept-list", rows, p),
            p,
        ));
    }
    page.child(scrolled("approvals-scroll", sections, p))
}

fn section(key: &str, title: &str, body: El, p: &Paint) -> El {
    col()
        .key(format!("{key}-section"))
        .w_full()
        .gap(8.0)
        .child(
            text(title.to_string())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_muted),
        )
        .child(card(&format!("{key}-card"), body, p))
}

/// A row that opens `url` in the browser.
fn link_row(key: &str, title: El, detail: String, url: &str, p: &Paint) -> El {
    let url = url.to_string();
    row()
        .key(key.to_string())
        .w_full()
        .py(10.0)
        .gap(12.0)
        .center_y()
        .child(
            col().grow(1.0).min_w(0.0).gap(3.0).child(title).child(
                text(detail)
                    .font(p.mono(11.0, 400.0))
                    .color(p.c.text_muted)
                    .truncate(),
            ),
        )
        .child(
            Button::new("Open PR")
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .disabled(url.is_empty())
                .on_press(move |app: &mut App| {
                    app.editor_error = crate::editor::open_url(&url).err();
                })
                .build(p.k)
                .key(format!("{key}-open")),
        )
}

/// `owner/repo#7` for a pull request's URL on any forge that puts it at `/owner/repo/pull/7`.
fn pr_label(url: &str) -> Option<String> {
    let (_, path) = url.split_once("://")?.1.split_once('/')?;
    let mut parts = path.trim_end_matches('/').split('/');
    let (owner, repo, "pull", number) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?)
    else {
        return None;
    };
    let numbered = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    (numbered && parts.next().is_none() && !owner.is_empty() && !repo.is_empty())
        .then(|| format!("{owner}/{repo}#{number}"))
}

/// The registry id an import suggests: its pack folder's name.
pub(crate) fn suggested_id(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn import_row(import: &crux::dto::PendingImport, app: &App, now: i64, p: &Paint) -> El {
    let id = import.id.clone();
    let pack = if import.path.is_empty() {
        import.repo.clone()
    } else {
        format!("{}/{}", import.repo, import.path)
    };
    let mut title = row().gap(8.0).center_y().child(
        text(pack)
            .font(p.k.ty.ui_medium)
            .color(p.c.text_base)
            .truncate(),
    );
    if !import.compiles {
        title = title.child(flag("does not compile", Tone::Danger, None, p));
    } else if import.diagnostics > 0 {
        let n = import.diagnostics;
        title = title.child(flag(
            &format!("{n} diagnostic{}", if n == 1 { "" } else { "s" }),
            Tone::Warning,
            None,
            p,
        ));
    }
    if let Some(draft) = &import.draft_id {
        title = title.child(tag(&format!("draft {draft}"), p));
    }
    let mut about = vec![import.rev.chars().take(12).collect::<String>()];
    if let Some(by) = &import.proposed_by {
        about.push(format!("by {by}"));
    }
    about.push(relative(&import.created_at, now));

    let armed_key = format!("discard:{id}");
    let armed = app.remote.desk.armed.as_deref() == Some(armed_key.as_str());
    let registering = app
        .remote
        .desk
        .register
        .as_ref()
        .is_some_and(|(open, ..)| *open == id);
    let mut actions = row().gap(6.0).center_y();
    if !registering {
        let (open_id, path) = (id.clone(), import.path.clone());
        actions = actions.child(
            Button::new("Register")
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .disabled(app.remote.desk.busy() || !import.compiles)
                .on_press(move |app: &mut App| {
                    app.remote.desk.register = Some((
                        open_id.clone(),
                        TextEdit::new(suggested_id(&path)),
                        TextEdit::new(""),
                    ));
                })
                .build(p.k)
                .key(format!("import-register-{id}")),
        );
    }
    let discard = id.clone();
    actions = actions.child(
        Button::new(if armed { "Confirm discard" } else { "Discard" })
            .variant(if armed {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Ghost
            })
            .control_size(ControlSize::Small)
            .disabled(app.remote.desk.busy())
            .on_press(move |app: &mut App| {
                let key = format!("discard:{discard}");
                if app.remote.desk.armed.as_deref() == Some(key.as_str()) {
                    app.act(Action::DiscardImport {
                        import: discard.clone(),
                    });
                } else {
                    app.remote.desk.armed = Some(key);
                }
            })
            .build(p.k)
            .key(format!("import-discard-{id}")),
    );

    let mut body = col().w_full().py(10.0).gap(8.0).child(
        row()
            .w_full()
            .gap(12.0)
            .center_y()
            .child(
                col().grow(1.0).min_w(0.0).gap(3.0).child(title).child(
                    text(about.join(" \u{b7} "))
                        .font(p.mono(11.0, 400.0))
                        .color(p.c.text_muted)
                        .truncate(),
                ),
            )
            .child(actions),
    );
    if registering && let Some((_, registry_id, description)) = &app.remote.desk.register {
        body = body.child(register_form(&id, registry_id, description, app, p));
    }
    body.key(format!("import-{id}"))
}

fn register_form(
    import: &str,
    registry_id: &TextEdit,
    description: &TextEdit,
    app: &App,
    p: &Paint,
) -> El {
    let ready = !registry_id.text.trim().is_empty() && !description.text.trim().is_empty();
    let import = import.to_string();
    let submit = move |app: &mut App| {
        let Some((_, id, description)) = &app.remote.desk.register else {
            return;
        };
        let (id, description) = (
            id.text.trim().to_string(),
            description.text.trim().to_string(),
        );
        if id.is_empty() || description.is_empty() {
            return;
        }
        app.remote.desk.register = None;
        app.act(Action::RegisterImport {
            import: import.clone(),
            id,
            description,
        });
    };
    let on_submit = submit.clone();
    row()
        .key("import-register-form")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            TextInput::new(registry_id)
                .placeholder("registry id")
                .width(Some(180.0))
                .on_edit(|app: &mut App| match &mut app.remote.desk.register {
                    Some((_, id, _)) => id,
                    None => &mut app.scratch,
                })
                .build(p.k)
                .key("import-register-id"),
        )
        .child(
            TextInput::new(description)
                .placeholder("What it does")
                .on_edit(|app: &mut App| match &mut app.remote.desk.register {
                    Some((_, _, description)) => description,
                    None => &mut app.scratch,
                })
                .on_submit(move |app: &mut App, _: &mut Tx| on_submit(app))
                .build(p.k)
                .key("import-register-description")
                .grow(1.0),
        )
        .child(el())
        .child(
            Button::new("Cancel")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|app: &mut App| app.remote.desk.register = None)
                .build(p.k)
                .key("import-register-cancel"),
        )
        .child(
            Button::new("Register")
                .variant(ButtonVariant::Submit)
                .control_size(ControlSize::Small)
                .disabled(!ready || app.remote.desk.busy())
                .on_press(submit)
                .build(p.k)
                .key("import-register-save"),
        )
}

#[cfg(test)]
mod tests {
    use crate::ui::approvals::{pr_label, suggested_id};

    #[test]
    fn a_pull_request_is_named_by_its_repository_and_number() {
        let label = |url| pr_label(url).unwrap();
        assert_eq!(
            label("https://github.com/acme/relay/pull/2"),
            "acme/relay#2"
        );
        assert_eq!(
            label("https://git.example.com/acme/relay/pull/41/"),
            "acme/relay#41"
        );
        for url in [
            "",
            "acme/relay/pull/2",
            "https://github.com/acme/relay",
            "https://github.com/acme/relay/issues/2",
            "https://github.com/acme/relay/pull/",
            "https://github.com/acme/relay/pull/two",
            "https://github.com/acme/relay/pull/2/files",
            "https://github.com//relay/pull/2",
        ] {
            assert_eq!(pr_label(url), None, "{url}");
        }
    }

    #[test]
    fn an_import_suggests_its_folder_as_the_registry_id() {
        assert_eq!(suggested_id("domains/backport"), "backport");
        assert_eq!(suggested_id("domains/backport/"), "backport");
        assert_eq!(suggested_id(""), "");
    }
}
