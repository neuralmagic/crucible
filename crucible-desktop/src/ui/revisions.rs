//! The History pane: a draft's saves as a changelog down a rail, and the diff of any two of
//! them beside it.
//!
//! ```text
//!  v4 → v6   3 files  +5 −2                      ‹  ›  [Unified|Split]  ⟳
//!  ┌ Bisect ─ 2 saves in this range ─────── [v4 → v5] [v5 → v6] ┐
//!  ┌ saves ───────────┐ ┌ diff ───────────────────────────────┐
//!  │ ○ v7             │ │ workflow.star                 +2 −1 │
//!  │ ● v6  ✕  to      │ │  3 - verify = command(…)            │
//!  │ │ v5             │ │  3 + verify = command(…)            │
//!  │ ◎ v4  ✓  from    │ │                                     │
//!  │ ○ v3             │ │                                     │
//!  └──────────────────┘ └─────────────────────────────────────┘
//! ```

use crux::dto::{DraftChangedFile, DraftHistoryEntry, FileChange};
use rune::{Color, El, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::diff::{DiffMode, DiffView};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, SegmentedControl, Spinner,
};
use rune_theme_opencode::icons as oc;

use crate::live::LiveView;
use crate::revisions::{self, Fire, Landed, Placement, Range, Read};
use crate::ui::{Paint, Tone, age, flag};
use crate::{App, Selection};

const RAIL: f32 = 236.0;
/// The rail's gutter, where the line through the saves is drawn.
const GUTTER: f32 = 20.0;
/// Changed files one save lists.
const LISTED: usize = 6;

pub(crate) fn history_view(app: &App, p: &Paint) -> El {
    let Some((draft, template)) = app.draft_target() else {
        return note(
            "This run has no draft.",
            "History follows a draft's saves. Draft this playbook from the Pack pane to start one.",
            p,
        );
    };
    let read = app
        .remote
        .revisions
        .as_ref()
        .filter(|read| read.wanted.draft == draft);
    let loading = app.remote.revisions_sent.as_ref() != read.map(|read| &read.wanted);
    let read = match read.map(|read| &read.result) {
        None => {
            return col()
                .key("history-reading")
                .w_full()
                .grow(1.0)
                .center()
                .child(Spinner::new().size(16.0).build(p.k));
        }
        Some(Err(_)) if template.is_some() => {
            return note(
                &format!("No draft {draft} yet."),
                "Draft this playbook from the Pack pane; each save of it lands here.",
                p,
            );
        }
        Some(Err(error)) => return note("Could not read the draft's history.", error, p),
        Some(Ok(read)) => read,
    };
    let ran = app
        .remote
        .revisions_sent
        .as_ref()
        .and_then(|wanted| wanted.ran);
    let now = match &*app.remote.live {
        LiveView::Ready { now, .. } => Some(*now),
        LiveView::Loading | LiveView::Unavailable(_) => None,
    };

    col()
        .key("history")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(8.0)
        .child(toolbar(read, loading, app, p))
        .children(bisect(read, p))
        .child(
            row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(10.0)
                .child(rail(&draft, read, ran, now, p))
                .child(diff(read, app, p)),
        )
}

/// The range on screen, what it changed, and the controls that move it.
fn toolbar(read: &Read, loading: bool, app: &App, p: &Paint) -> El {
    let (added, removed) = revisions::totals(&read.diff);
    let files = read.diff.files.len();
    let (older, newer) = revisions::neighbours(&read.history, read.range);
    let step = |glyph, says: &'static str, to: Option<Range>, key: &'static str| {
        IconButton::new(glyph)
            .variant(IconButtonVariant::GhostMuted)
            .label(says)
            .help(says)
            .disabled(to.is_none())
            .on_press(move |app: &mut App| {
                if let Some(to) = to {
                    app.pick_range(to);
                }
            })
            .build(p.k)
            .key(key)
    };
    let split = app.remote.revisions_diff.mode() == DiffMode::Split;
    row()
        .key("history-bar")
        .w_full()
        .h(28.0)
        .gap(8.0)
        .center_y()
        .child(
            text(read.range.label())
                .font(p.mono(13.0, 600.0))
                .color(p.c.text_base)
                .key("history-range"),
        )
        .child(
            text(match files {
                1 => "1 file".to_string(),
                n => format!("{n} files"),
            })
            .font(p.k.ty.small)
            .color(p.c.text_muted),
        )
        .children((files > 0).then(|| {
            text(format!("+{added}"))
                .font(p.mono(11.0, 500.0))
                .color(p.pass)
        }))
        .children((files > 0).then(|| {
            text(format!("\u{2212}{removed}"))
                .font(p.mono(11.0, 500.0))
                .color(p.fail)
        }))
        .children(loading.then(|| Spinner::new().size(13.0).build(p.k)))
        .child(el().grow(1.0))
        .child(step(&oc::ARROW_LEFT, "Older save", older, "history-older"))
        .child(step(&oc::ARROW_RIGHT, "Newer save", newer, "history-newer"))
        .child(
            SegmentedControl::new(&["Unified", "Split"], Some(usize::from(split)))
                .label("Diff layout")
                .fit()
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                    app.remote.revisions_diff.set_mode(if i == 1 {
                        DiffMode::Split
                    } else {
                        DiffMode::Unified
                    });
                })
                .build(p.k)
                .key("history-mode"),
        )
        .child(
            IconButton::new(&oc::REFRESH)
                .variant(IconButtonVariant::GhostMuted)
                .label("Read the saves again")
                .help("Read the saves again")
                .on_press(|app: &mut App| app.reread_history())
                .build(p.k)
                .key("history-reread"),
        )
}

fn verdict_word(fires: &[Fire], version: i64) -> Option<&'static str> {
    revisions::verdict(fires, version).map(|passed| if passed { "passed" } else { "failed" })
}

/// The two halves of a range that covers more than one save. When the middle save was
/// test-fired, the half its verdict points at is the one offered first.
fn bisect(read: &Read, p: &Paint) -> Option<El> {
    let (older, newer) = revisions::halves(&read.history, read.range)?;
    let covered = revisions::saves_in(&read.history, read.range).len();
    let middle = older.to;
    let middle_passed = revisions::verdict(&read.fires, middle);
    let ends = match (
        read.range
            .from
            .and_then(|from| verdict_word(&read.fires, from).map(|w| (from, w))),
        verdict_word(&read.fires, read.range.to),
    ) {
        (Some((from, was)), Some(is)) => {
            format!("v{from} {was}, v{} {is}.", read.range.to)
        }
        _ => String::new(),
    };
    let hint = match middle_passed {
        Some(true) => format!("v{middle} passed, so the change is in the newer half."),
        Some(false) => format!("v{middle} failed, so the change is in the older half."),
        None => format!("v{middle} splits them. Read a half, then narrow to it."),
    };
    let half = |range: Range, label: &str, likely: bool, key: &'static str| {
        Button::new(format!("{label} \u{b7} {}", range.label()))
            .variant(if likely {
                ButtonVariant::Submit
            } else {
                ButtonVariant::Neutral
            })
            .control_size(ControlSize::Small)
            .on_press(move |app: &mut App| app.pick_range(range))
            .build(p.k)
            .key(key)
    };
    Some(
        row()
            .key("history-bisect")
            .w_full()
            .min_h(36.0)
            .px(10.0)
            .py(6.0)
            .gap(8.0)
            .center_y()
            .radius(p.k.radius.md)
            .fill(p.accent.alpha(0.09))
            .border(p.hair, p.accent.alpha(0.32))
            .child(icon(&oc::FORK, 14.0).color(p.accent))
            .child(
                text("Bisect".to_string())
                    .font(p.k.ty.small_medium)
                    .color(p.c.text_base),
            )
            .child(
                text(
                    format!("{covered} saves in this range. {ends} {hint}")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                )
                .font(p.k.ty.small)
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .wrap(),
            )
            .child(half(
                older,
                "Older half",
                middle_passed == Some(false),
                "history-older-half",
            ))
            .child(half(
                newer,
                "Newer half",
                middle_passed == Some(true),
                "history-newer-half",
            )),
    )
}

/// Every save, newest first, down a line that is drawn solid through the range on screen.
fn rail(draft: &str, read: &Read, ran: Option<i64>, now: Option<i64>, p: &Paint) -> El {
    let mut list = col()
        .key("history-saves")
        .w(RAIL)
        .h_full()
        .min_h(0.0)
        .padding(6.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(
            row()
                .w_full()
                .px(6.0)
                .pt(4.0)
                .pb(6.0)
                .gap(6.0)
                .center_y()
                .child(
                    text(draft.to_string())
                        .font(p.mono(10.5, 600.0))
                        .color(p.c.text_faint)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate()
                        .help_when_truncated(draft.to_string()),
                )
                .child(
                    text(match read.history.len() {
                        1 => "1 save".to_string(),
                        n => format!("{n} saves"),
                    })
                    .font(p.mono(10.0, 400.0))
                    .color(p.c.text_faint),
                ),
        );
    let last = read.history.len().saturating_sub(1);
    for (i, entry) in read.history.iter().enumerate() {
        let landed = read.landed.get(i).cloned().unwrap_or_default();
        list = list.child(save_row(
            entry,
            &landed,
            read,
            Ends {
                first: i == 0,
                last: i == last,
            },
            ran == Some(entry.version),
            now,
            p,
        ));
    }
    list
}

/// Whether a save is the newest or the oldest, where the line through the saves stops.
#[derive(Clone, Copy)]
struct Ends {
    first: bool,
    last: bool,
}

fn save_row(
    entry: &DraftHistoryEntry,
    landed: &Landed,
    read: &Read,
    ends: Ends,
    ran: bool,
    now: Option<i64>,
    p: &Paint,
) -> El {
    let version = entry.version;
    let range = read.range;
    let place = revisions::placement(range, version);
    let picked = matches!(place, Placement::From | Placement::To);
    let show = revisions::single(&read.history, version);
    let who = entry.created_by.as_deref().unwrap_or("unknown");
    let when = match (landed.at, now) {
        (Some(at), Some(now)) => age(at, now),
        _ => String::new(),
    };

    let mut head = row().w_full().gap(6.0).center_y().h(20.0).child(
        text(format!("v{version}")).font(p.mono(12.0, 600.0)).color(
            if place == Placement::Outside {
                p.c.text_muted
            } else {
                p.c.text_base
            },
        ),
    );
    if let Some(fire) = read.fires.iter().find(|fire| fire.version == version) {
        head = head.children(fired(fire, &read.fires, p));
    }
    if entry.schema_digest.is_none() {
        head = head.child(
            flag("broken", Tone::Danger, None, p).help(match entry.diagnostics {
                1 => "This save did not compile: 1 diagnostic".to_string(),
                n => format!("This save did not compile: {n} diagnostics"),
            }),
        );
    }
    if ran {
        head = head.child(
            text("this run".to_string())
                .font(p.mono(9.5, 500.0))
                .color(p.accent),
        );
    }
    head = head
        .child(el().grow(1.0))
        .child(end(
            "from",
            place == Placement::From,
            Range {
                from: Some(version),
                to: range.to,
            },
            version,
            p,
        ))
        .child(end(
            "to",
            place == Placement::To,
            match range.from {
                Some(from) => Range {
                    from: Some(from),
                    to: version,
                },
                None => show,
            },
            version,
            p,
        ));

    let body = col()
        .grow(1.0)
        .min_w(0.0)
        .pb(8.0)
        .gap(2.0)
        .child(head)
        .child(
            text(if when.is_empty() {
                who.to_string()
            } else {
                format!("{who} \u{b7} {when}")
            })
            .font(p.k.ty.small)
            .color(p.c.text_faint)
            .truncate()
            .help(landed.label.clone()),
        )
        .child(changes(entry.changes.as_deref(), p));

    row()
        .key(format!("history-save-{version}"))
        .role("button")
        .label(format!("Show what v{version} changed"))
        .w_full()
        .pr(6.0)
        .items_stretch()
        .radius(p.k.radius.md)
        .fill(if picked {
            p.c.pressed
        } else {
            Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .hover_group()
        .focusable(true)
        .on_press(move |app: &mut App| app.pick_range(show))
        .child(gutter(range, version, place, ends, p))
        .child(body)
}

/// A save's stretch of the line: a segment up to the save above, its node, and a segment down
/// to the save below, each solid where it runs inside the range.
fn gutter(range: Range, version: i64, place: Placement, ends: Ends, p: &Paint) -> El {
    let (low, high) = match range.from {
        Some(from) => (from.min(range.to), from.max(range.to)),
        None => (range.to, range.to),
    };
    let quiet = p.c.border_base;
    let segment = |inside: bool, hidden: bool| {
        el().w(if inside { 2.0 } else { 1.0 }).fill(if hidden {
            Color::TRANSPARENT
        } else if inside {
            p.accent
        } else {
            quiet
        })
    };
    let node = match place {
        Placement::To => el()
            .size(11.0, 11.0)
            .radius(5.5)
            .fill(p.accent)
            .border(2.0, p.c.layer_01),
        Placement::From => el()
            .size(11.0, 11.0)
            .radius(5.5)
            .fill(p.c.layer_01)
            .border(2.0, p.accent),
        Placement::Inside => el().size(7.0, 7.0).radius(3.5).fill(p.accent),
        Placement::Outside => el()
            .size(7.0, 7.0)
            .radius(3.5)
            .fill(p.c.layer_01)
            .border(1.0, p.c.text_faint),
    };
    col()
        .w(GUTTER)
        .items_center()
        .child(segment(version >= low && version < high, ends.first).h(5.0))
        .child(col().h(11.0).center().child(node))
        .child(segment(version > low && version <= high, ends.last).grow(1.0))
}

/// How a save's newest test-fire went, opening that launch on a click, and how many there were.
fn fired(fire: &Fire, fires: &[Fire], p: &Paint) -> Option<El> {
    let passed = revisions::verdict(fires, fire.version)?;
    let count = fires.iter().filter(|f| f.version == fire.version).count();
    let says = format!(
        "Test-fire {} \u{b7} open it{}",
        if passed { "passed" } else { "failed" },
        if count > 1 {
            format!(" ({count} test-fires)")
        } else {
            String::new()
        }
    );
    let key = fire.key.clone();
    Some(
        row()
            .key(format!("history-fire-{}", fire.version))
            .role("button")
            .label(says.clone())
            .help(says)
            .size(18.0, 18.0)
            .center()
            .radius(9.0)
            .hover(p.c.hover)
            .on_press(move |app: &mut App| {
                if let Some(launch) = app.remote.live.launch(&key).cloned() {
                    app.select(Selection::launch(&launch));
                }
            })
            .child(
                icon(
                    if passed {
                        &oc::CIRCLE_CHECK
                    } else {
                        &oc::CIRCLE_X
                    },
                    14.0,
                )
                .color(if passed { p.pass } else { p.fail }),
            ),
    )
}

/// One end of the range, set to this save on a press. An end the range is not at shows only
/// while its save is under the pointer.
fn end(label: &'static str, on: bool, range: Range, version: i64, p: &Paint) -> El {
    let chip = row()
        .key(format!("history-{label}-{version}"))
        .role("button")
        .label(format!("Compare {label} v{version}"))
        .help(format!("Compare {label} v{version}"))
        .h(16.0)
        .px(5.0)
        .center_y()
        .radius(p.k.radius.sm)
        .fill(if on {
            p.c.bg_accent
        } else {
            Color::TRANSPARENT
        })
        .border(p.hair, if on { p.accent } else { p.c.border_muted })
        .hover(p.c.hover)
        .on_press(move |app: &mut App| app.pick_range(range))
        .child(
            text(label.to_string())
                .font(p.mono(9.5, 500.0))
                .color(if on { p.c.white } else { p.c.text_faint }),
        );
    if on {
        chip
    } else {
        chip.group_opacity(0.0, 1.0)
    }
}

fn changes(changes: Option<&[DraftChangedFile]>, p: &Paint) -> El {
    let faint = |says: &str| {
        text(says.to_string())
            .font(p.mono(10.5, 400.0))
            .color(p.c.text_faint)
    };
    let Some(changes) = changes else {
        return faint("changes unknown");
    };
    if changes.is_empty() {
        return faint("no file changes");
    }
    let mut list = col().w_full().gap(1.0).pt(2.0);
    for file in changes.iter().take(LISTED) {
        let (letter, ink) = match file.change {
            FileChange::Added => ("A", p.pass),
            FileChange::Removed => ("D", p.fail),
            FileChange::Modified => ("M", p.live),
        };
        list = list.child(
            row()
                .w_full()
                .gap(6.0)
                .child(
                    text(letter.to_string())
                        .font(p.mono(10.5, 600.0))
                        .color(ink)
                        .w(9.0),
                )
                .child(
                    text(file.path.clone())
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate_head()
                        .help_when_truncated(file.path.clone()),
                ),
        );
    }
    if changes.len() > LISTED {
        list = list.child(faint(&format!("+{} more", changes.len() - LISTED)));
    }
    list
}

fn diff(read: &Read, app: &App, p: &Paint) -> El {
    if read.diff.files.is_empty() {
        return col().grow(1.0).min_w(0.0).child(note(
            "No file changes.",
            &match read.range.from {
                Some(from) => format!("v{from} and v{} hold the same files.", read.range.to),
                None => format!("v{} holds no files.", read.range.to),
            },
            p,
        ));
    }
    col()
        .key("history-diff-frame")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .clip_rounded(p.k.radius.lg)
        .child(
            DiffView::new(
                |app: &App| &app.remote.revisions_diff,
                |app: &mut App| &mut app.remote.revisions_diff,
            )
            .key("history-diff")
            .build(app, p.k),
        )
}

fn note(headline: &str, detail: &str, p: &Paint) -> El {
    let mut card = col()
        .key("history-note")
        .w_full()
        .padding(24.0)
        .gap(6.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(
            text(headline.to_string())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .wrap(),
        );
    if !detail.is_empty() {
        card = card.child(
            text(detail.to_string())
                .font(p.mono(11.0, 400.0))
                .color(p.c.text_muted)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        );
    }
    card
}
