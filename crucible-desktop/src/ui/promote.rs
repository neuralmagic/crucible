//! The draft's Promote row: the pull or merge request it opened and where it stands, and the form
//! that opens or updates one with a signed-in forge account.

use rune::{El, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, TextInput};

use crate::App;
use crate::ui::Paint;

pub(crate) fn section(app: &App, p: &Paint) -> Option<El> {
    let promoting = app.promoting.as_ref()?;
    let accounts = app.signed_in_accounts();
    let mut block = col().key("promote").w_full().gap(8.0);

    if let Some(shown) = &promoting.shown {
        let url = shown.url.clone();
        block = block.child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    Button::new(shown.label())
                        .variant(ButtonVariant::Ghost)
                        .control_size(rune_kit::ControlSize::Small)
                        .on_press(move |app: &mut App| {
                            if let Err(why) = crate::editor::open_url(&url)
                                && let Some(promoting) = &mut app.promoting
                            {
                                promoting.error = Some(why);
                            }
                        })
                        .build(p.k)
                        .key("promote-open"),
                )
                .children((!shown.note.is_empty()).then(|| {
                    text(shown.note.clone())
                        .font(p.k.ty.small)
                        .color(p.c.text_muted)
                        .truncate()
                })),
        );
    }

    if accounts.is_empty() {
        return Some(
            block.child(
                text("Sign in to GitHub or GitLab in Settings to promote")
                    .font(p.k.ty.small)
                    .color(p.c.text_muted),
            ),
        );
    }

    let mut picker = row().gap(4.0).center_y();
    for account in accounts {
        let picked = promoting.account.as_ref() == Some(&account);
        let key = account.store_key();
        let chosen = account.clone();
        picker = picker.child(
            Button::new(account.name())
                .variant(match picked {
                    true => ButtonVariant::Neutral,
                    false => ButtonVariant::Ghost,
                })
                .control_size(rune_kit::ControlSize::Small)
                .on_press(move |app: &mut App| app.promote_with(chosen.clone()))
                .build(p.k)
                .key(format!("promote-account-{key}")),
        );
    }
    let ready =
        promoting.account.is_some() && app.promote_form.destination().is_some() && !promoting.busy;
    let label = match &promoting.shown {
        Some(shown) if shown.followed() => "Promote again",
        _ => "Promote",
    };
    block = block.child(
        row()
            .gap(8.0)
            .center_y()
            .child(picker)
            .child(
                TextInput::new(&app.promote_form.project)
                    .placeholder("owner/repo")
                    .width(Some(200.0))
                    .on_edit(|app: &mut App| &mut app.promote_form.project)
                    .build(p.k)
                    .key("promote-project"),
            )
            .child(
                TextInput::new(&app.promote_form.path)
                    .placeholder("path")
                    .width(Some(160.0))
                    .on_edit(|app: &mut App| &mut app.promote_form.path)
                    .build(p.k)
                    .key("promote-path"),
            )
            .child(
                TextInput::new(&app.promote_form.base)
                    .placeholder("base branch")
                    .width(Some(120.0))
                    .on_edit(|app: &mut App| &mut app.promote_form.base)
                    .on_submit(|app: &mut App, _: &mut Tx| app.promote_draft())
                    .build(p.k)
                    .key("promote-base"),
            )
            .child(
                Button::new(label)
                    .variant(ButtonVariant::Submit)
                    .control_size(rune_kit::ControlSize::Small)
                    .disabled(!ready)
                    .on_press(|app: &mut App| app.promote_draft())
                    .build(p.k)
                    .key("promote-send"),
            )
            .children(
                promoting
                    .busy
                    .then(|| rune_kit::Spinner::new().size(12.0).build(p.k)),
            ),
    );
    if let Some(why) = &promoting.error {
        block = block.child(
            text(why.clone())
                .font(p.k.ty.small)
                .color(p.fail)
                .selectable(true)
                .key("promote-error"),
        );
    }
    Some(block)
}
