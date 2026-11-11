//! The run workspace on screen: `crate::workspace`'s columns and groups in resizable splits,
//! each group a tab bar over its active pane. Tabs drag to another group's tab bar or middle
//! to join it, or to one of its edges to split it.

use rune::{Color, Cursor, Drag, El, Icon, Phase, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize};
use rune_theme_opencode::icons as oc;

use crate::ui::{
    Paint, Pane, agent, compare, data, empty_run, files, fx, graph, launch, log, pack, tasks,
    timeline, transcript, tube,
};
use crate::workspace::{At, Divider, Dock, Edge, Group, PaneDrag, PaneKind, hit};
use crate::{App, gfx};

const TAB_BAR: f32 = 36.0;
const GUTTER: f32 = 8.0;
const DRAG_START: f32 = 4.0;

pub(crate) fn workspace(pane: &Pane, app: &App, p: &Paint) -> El {
    let ws = app.workspace();
    let mut area = row()
        .key("workspace")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .on_layout_tx(|app: &mut App, rect, tx: &mut Tx| {
            app.geometry.area = rect;
            tx.live_only();
        });
    for (c, column) in ws.columns.iter().enumerate() {
        if c > 0 {
            area = area.child(divider(Divider::Columns(c - 1), app, p));
        }
        let mut stack = col()
            .key(format!("ws-col-{c}"))
            .w(0.0)
            .grow(column.share)
            .h_full()
            .min_w(0.0)
            .on_layout_tx(move |app: &mut App, rect, tx: &mut Tx| {
                app.geometry.columns.insert(c, rect);
                tx.live_only();
            });
        for (g, group) in column.groups.iter().enumerate() {
            if g > 0 {
                stack = stack.child(divider(Divider::Groups(c, g - 1), app, p));
            }
            stack = stack.child(group_view((c, g), group, pane, app, p));
        }
        area = area.child(stack);
    }
    area
}

/// The tools above the workspace: reopen a closed pane, or go back to the fitted layout.
pub(crate) fn tools(app: &App, p: &Paint) -> Option<El> {
    let layout = app.layout.as_ref()?;
    let mut bar = row().key("ws-tools").gap(4.0).center_y();
    for kind in layout
        .closed()
        .filter(|kind| *kind != PaneKind::Agent || app.agent_available())
    {
        bar = bar.child(
            Button::new(kind.label())
                .icon(&oc::PLUS)
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(move |app: &mut App| {
                    app.arrange(|layout| layout.activate(kind));
                    app.save_layout();
                })
                .build(p.k)
                .key(format!("reopen-{}", slug(kind))),
        );
    }
    Some(
        bar.child(
            Button::new("Reset layout")
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(|app: &mut App| app.reset_layout())
                .build(p.k)
                .key("reset-layout"),
        ),
    )
}

fn slug(kind: PaneKind) -> String {
    kind.label().to_ascii_lowercase()
}

pub(crate) fn glyph(kind: PaneKind) -> &'static Icon {
    match kind {
        PaneKind::Tasks => &oc::CHECKLIST,
        PaneKind::Timeline => &oc::STATUS,
        PaneKind::Graph => &oc::FILE_TREE,
        PaneKind::Data => &oc::CODE_LINES,
        PaneKind::Files => &oc::OUTLINE_WORKTREE,
        PaneKind::Transcript => &oc::BRAIN,
        PaneKind::Compare => &oc::OUTLINE_COPY,
        PaneKind::Pack => &oc::FOLDER,
        PaneKind::Launch => &oc::GLOBE,
        PaneKind::Log => &oc::CONSOLE,
        PaneKind::Agent => &oc::SPEECH_BUBBLE,
    }
}

fn group_view(at: At, group: &Group, pane: &Pane, app: &App, p: &Paint) -> El {
    let closable = app.workspace().closed().count() + 1 < PaneKind::ALL.len();
    let mut bar = row()
        .key("tabs")
        .w_full()
        .h(TAB_BAR)
        .px(5.0)
        .gap(2.0)
        .center_y()
        .fill(p.c.layer_01);
    for kind in &group.tabs {
        bar = bar.child(tab(*kind, *kind == group.active, closable, app, p));
    }

    let mut frame = col()
        .key(format!("ws-g-{}-{}", at.0, at.1))
        .w_full()
        .h(0.0)
        .grow(group.share)
        .min_h(0.0)
        .radius(p.k.radius.lg)
        .fill(p.c.bg_base)
        .border(
            p.hair,
            if app.pane_focus.is_some() && app.focused_pane() == group.active {
                p.c.text_accent
            } else {
                p.c.border_muted
            },
        )
        .clip_rounded(p.k.radius.lg)
        .on_layout_tx(move |app: &mut App, rect, tx: &mut Tx| {
            app.geometry.groups.insert(at, rect);
            tx.live_only();
        })
        .child(bar)
        .child(el().h(p.hair).w_full().fill(p.c.border_muted))
        .child(
            col()
                .key(format!("pane-{}", slug(group.active)))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .padding(10.0)
                .child(content(group.active, pane, app, p)),
        );
    if let Some(preview) = drop_preview(at, group, app, p) {
        frame = frame.child(preview);
    }
    frame
}

fn content(kind: PaneKind, pane: &Pane, app: &App, p: &Paint) -> El {
    match kind {
        PaneKind::Tasks => tasks::task_list(
            app,
            pane.tasks,
            pane.agent_lines(),
            pane.verdict == crate::model::Verdict::Running,
            empty_run(pane, p),
            p,
        ),
        PaneKind::Timeline => col()
            .key("crt-timeline")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(timeline::timeline_view(pane, p))
            .children(tube(app)),
        PaneKind::Graph => col()
            .key("crt-graph")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(graph::graph_view(pane, app, p))
            .children(tube(app)),
        PaneKind::Data => data::data_view(pane, app, p),
        PaneKind::Files => files::files_view(pane, app, p),
        PaneKind::Transcript => transcript::transcript_view(pane, app, p),
        PaneKind::Compare => compare::compare_view(pane, app, p),
        PaneKind::Pack => pack::pack_view(
            pane.pack.as_deref().map_err(String::as_str),
            pane.log_font,
            app,
            p,
        ),
        PaneKind::Launch if pane.launchable => col()
            .key("launch-local")
            .w_full()
            .grow(1.0)
            .center()
            .gap(6.0)
            .child(icon(&oc::GLOBE, 22.0).color(p.c.icon_faint))
            .child(
                text("Local packs have no launch record.")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            ),
        PaneKind::Launch => launch::launch_view(pane.launch, p),
        PaneKind::Log => {
            log::log_pane(pane.transcript, pane.log, pane.tasks, pane.log_font, app, p)
        }
        PaneKind::Agent => agent::panel(pane.log_font, app, p),
    }
}

fn tab(kind: PaneKind, active: bool, closable: bool, app: &App, p: &Paint) -> El {
    let dragging = app
        .pane_drag
        .is_some_and(|drag| drag.kind == kind && drag.moved);
    let mut t = row()
        .key(format!("tab-{}", slug(kind)))
        .role("tab")
        .label(kind.label())
        .checked(active)
        .h(26.0)
        .pl(9.0)
        .pr(if closable { 3.0 } else { 9.0 })
        .gap(6.0)
        .center_y()
        .radius(p.k.radius.md)
        .fill(if active {
            p.c.layer_03
        } else {
            Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .hover_group()
        .opacity(if dragging { 0.4 } else { 1.0 })
        .cursor(if dragging {
            Cursor::Grabbing
        } else {
            Cursor::Default
        })
        .on_layout_tx(move |app: &mut App, rect, tx: &mut Tx| {
            app.geometry.tabs.insert(kind, rect);
            tx.live_only();
        })
        .on_drag(move |app: &mut App, d: &mut Drag, tx: &mut Tx| drag_tab(app, kind, d, tx))
        .child(icon(glyph(kind), 14.0).color(if active {
            p.c.icon_base
        } else {
            p.c.icon_muted
        }))
        .child(text(kind.label()).font(p.k.ty.ui_medium).color(if active {
            p.c.text_base
        } else {
            p.c.text_muted
        }));
    if closable {
        let close = row()
            .key("close")
            .role("button")
            .label(format!("Close {}", kind.label()))
            .size(18.0, 18.0)
            .center()
            .radius(p.k.radius.sm)
            .hover(p.c.hover)
            .on_press(move |app: &mut App| {
                app.arrange(|layout| layout.close(kind));
                app.save_layout();
            })
            .child(icon(&oc::XMARK_SMALL, 14.0).color(p.c.icon_faint));
        t = t.child(if active {
            close
        } else {
            close.group_opacity(0.0, 1.0)
        });
    }
    t
}

fn drag_tab(app: &mut App, kind: PaneKind, d: &mut Drag, tx: &mut Tx) {
    match d.phase {
        Phase::Down => {
            let [x, y, _, _] = app.geometry.tabs.get(&kind).copied().unwrap_or_default();
            let origin = [x + d.start_x, y + d.start_y];
            app.pane_drag = Some(PaneDrag {
                kind,
                origin,
                pointer: origin,
                moved: false,
                dock: None,
            });
        }
        Phase::Move => {
            let ws = app.workspace();
            let groups: Vec<(At, [f32; 4])> = app
                .geometry
                .groups
                .iter()
                .filter(|(at, _)| ws.group(**at).is_some())
                .map(|(at, rect)| (*at, *rect))
                .collect();
            let Some(drag) = app.pane_drag.as_mut() else {
                return;
            };
            drag.pointer = [drag.origin[0] + d.dx, drag.origin[1] + d.dy];
            drag.moved |= d.dx.hypot(d.dy) > DRAG_START;
            if drag.moved {
                tx.instant();
                drag.dock = hit(&ws, &groups, TAB_BAR, drag.pointer);
            }
        }
        Phase::Up => {
            let Some(drag) = app.pane_drag.take() else {
                return;
            };
            if d.cancelled() {
                return;
            }
            match (drag.moved, drag.dock) {
                (false, _) => app.show_pane(kind),
                (true, Some(dock)) => {
                    app.arrange(|layout| layout.dock(kind, dock));
                    app.save_layout();
                }
                (true, None) => {}
            }
        }
    }
}

/// Where a dragged tab would land, drawn over the group it would land in.
fn drop_preview(at: At, group: &Group, app: &App, p: &Paint) -> Option<El> {
    let drag = app.pane_drag.filter(|drag| drag.moved)?;
    let (anchor, edge) = match drag.dock? {
        Dock::Into(anchor) => (anchor, None),
        Dock::Split(anchor, edge) => (anchor, Some(edge)),
    };
    if !group.tabs.contains(&anchor) {
        return None;
    }
    let [_, _, w, h] = app.geometry.groups.get(&at).copied()?;
    let body = h - TAB_BAR;
    let [x, y, w, h] = match edge {
        None => [0.0, 0.0, w, h],
        Some(Edge::Left) => [0.0, TAB_BAR, w / 2.0, body],
        Some(Edge::Right) => [w / 2.0, TAB_BAR, w / 2.0, body],
        Some(Edge::Top) => [0.0, TAB_BAR, w, body / 2.0],
        Some(Edge::Bottom) => [0.0, TAB_BAR + body / 2.0, w, body / 2.0],
    };
    let inset = 4.0;
    Some(
        el().key("drop-preview")
            .absolute(x + inset, y + inset)
            .size((w - 2.0 * inset).max(0.0), (h - 2.0 * inset).max(0.0))
            .radius(p.k.radius.md)
            .fill(p.c.bg_accent.alpha(0.12))
            .border(1.5, p.c.bg_accent.alpha(0.7))
            .child(
                fx(
                    gfx::GLOW,
                    gfx::data(p.c.bg_accent, p.tune.glow),
                    p.tune.glow_bleed,
                )
                .radius(p.k.radius.md),
            ),
    )
}

/// The dragged tab, following the pointer.
pub(crate) fn ghost(app: &App, p: &Paint) -> Option<El> {
    let drag = app.pane_drag.filter(|drag| drag.moved)?;
    let chip = row()
        .key("tab-ghost")
        .absolute(drag.pointer[0] + 10.0, drag.pointer[1] + 8.0)
        .h(28.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(p.k.radius.md)
        .fill(p.c.layer_03)
        .border(p.hair, p.c.border_base)
        .child(icon(glyph(drag.kind), 14.0).color(p.c.icon_base))
        .child(
            text(drag.kind.label())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base),
        );
    Some(p.k.elevate(chip, &p.k.elevation.floating))
}

fn divider(split: Divider, app: &App, p: &Paint) -> El {
    let across = matches!(split, Divider::Columns(_));
    let held = app.resizing.is_some_and(|(held, _)| held == split);
    let grip = el().key("grip").radius(1.5).fill(if held {
        p.c.bg_accent
    } else {
        p.c.border_strong
    });
    let grip = if across {
        grip.size(3.0, 32.0)
    } else {
        grip.size(32.0, 3.0)
    };
    let handle = row()
        .key(match split {
            Divider::Columns(i) => format!("split-col-{i}"),
            Divider::Groups(c, g) => format!("split-group-{c}-{g}"),
        })
        .role("separator")
        .label("Resize")
        .center()
        .hover_group()
        .cursor(if across {
            Cursor::ResizeColumn
        } else {
            Cursor::ResizeRow
        })
        .on_drag(move |app: &mut App, d: &mut Drag, tx: &mut Tx| resize(app, split, d, tx))
        .on_double_press(move |app: &mut App| {
            app.arrange(|layout| match split {
                Divider::Columns(i) => {
                    let total = layout.columns[i].share + layout.columns[i + 1].share;
                    layout.resize_columns(i, (total / 2.0, total / 2.0), 0.0);
                }
                Divider::Groups(c, g) => {
                    let groups = &layout.columns[c].groups;
                    let total = groups[g].share + groups[g + 1].share;
                    layout.resize_groups(c, g, (total / 2.0, total / 2.0), 0.0);
                }
            });
            app.save_layout();
        })
        .child(if held {
            grip
        } else {
            grip.group_opacity(0.0, 1.0)
        });
    if across {
        handle.w(GUTTER).h_full()
    } else {
        handle.h(GUTTER).w_full()
    }
}

fn resize(app: &mut App, split: Divider, d: &mut Drag, tx: &mut Tx) {
    match d.phase {
        Phase::Down => {
            let ws = app.workspace();
            let shares = match split {
                Divider::Columns(i) => (ws.columns[i].share, ws.columns[i + 1].share),
                Divider::Groups(c, g) => {
                    let groups = &ws.columns[c].groups;
                    (groups[g].share, groups[g + 1].share)
                }
            };
            app.resizing = Some((split, shares));
        }
        Phase::Move => {
            let Some((_, from)) = app.resizing else {
                return;
            };
            tx.instant();
            let ws = app.workspace();
            let (length, travel) = match split {
                Divider::Columns(_) => {
                    let gutters = (ws.columns.len().saturating_sub(1)) as f32 * GUTTER;
                    (app.geometry.area[2] - gutters, d.dx)
                }
                Divider::Groups(c, _) => {
                    let height = app.geometry.columns.get(&c).map_or(0.0, |rect| rect[3]);
                    let gutters = (ws.columns[c].groups.len().saturating_sub(1)) as f32 * GUTTER;
                    (height - gutters, d.dy)
                }
            };
            if length <= 0.0 {
                return;
            }
            let delta = travel / length;
            app.arrange(|layout| match split {
                Divider::Columns(i) => layout.resize_columns(i, from, delta),
                Divider::Groups(c, g) => layout.resize_groups(c, g, from, delta),
            });
        }
        Phase::Up => {
            if let (true, Some((_, from))) = (d.cancelled(), app.resizing) {
                app.arrange(|layout| match split {
                    Divider::Columns(i) => layout.resize_columns(i, from, 0.0),
                    Divider::Groups(c, g) => layout.resize_groups(c, g, from, 0.0),
                });
            }
            app.resizing = None;
            app.save_layout();
        }
    }
}
