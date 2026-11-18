//! The sidebar: the brand, the pages, search and filters, and the playbooks or packs.

use crate::filter::{LandedFilter, OriginFilter, OwnerFilter};
use crate::ignition::Spark;
use crate::live::{LaunchStatus, LivePlaybook, LiveView};
use crate::model::{PackView, Verdict};
use crate::net::Down;
use crate::ui::account::account;
use crate::ui::{Paint, TITLE_BAND, Tone, fx, note, pulsing, rule};
use crate::{App, Backend, Selection, gfx};
use rune::{Color, El, Icon, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::shell::RowStatus;
use rune_kit::{
    Button, ButtonVariant, IconButton, IconButtonVariant, SegmentedControl, SidebarRow, Spinner,
    TextInput,
};
use rune_theme_opencode::icons as oc;

const SIDEBAR_PAD: f32 = 12.0;
const ROW: f32 = 40.0;
/// The width of a sidebar filter's label beside its control.
const FILTER_LABEL: f32 = 40.0;
const FILTER_TOGGLE: f32 = 28.0;

const BRAND: TextStyle = TextStyle::new(15.0, 640.0).lh(20.0);

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
pub(crate) fn tube(app: &App) -> Option<El> {
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

pub(crate) fn sidebar(app: &App, p: &Paint, layer: &mut Vec<El>) -> El {
    let source = match app.backend {
        Backend::Controller => match &*app.remote.live {
            LiveView::Ready { endpoint, .. } => {
                format!("{} \u{b7} {endpoint}", app.remote.target.name)
            }
            LiveView::Loading => app.remote.target.name.clone(),
            LiveView::Unavailable(_) => format!("{} \u{b7} not connected", app.remote.target.name),
        },
        Backend::Local => app.repo.clone(),
    };

    let brand = row()
        .key("brand")
        .center_y()
        .gap(6.0)
        .child(logo(app, p))
        .child(text("Playbooks").font(p.sized(BRAND)).color(p.c.text_base));

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
        .pb(10.0)
        .gap(8.0)
        .child(search(app, p));
    if let (
        Backend::Controller,
        LiveView::Ready {
            stale: Some(error), ..
        },
    ) = (app.backend, &*app.remote.live)
    {
        head = head.child(
            text(error.clone())
                .font(p.k.ty.caption())
                .color(p.c.warning.fg)
                .max_lines(2)
                .wrap(),
        );
    }
    if let Some(why) = &app.drop_failed {
        head = head.child(
            row()
                .key("drop-failed")
                .w_full()
                .gap(6.0)
                .child(
                    text(why.clone())
                        .font(p.k.ty.caption())
                        .color(p.fail)
                        .max_lines(3)
                        .wrap()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(
                    IconButton::new(&rune_kit::glyphs::CLOSE)
                        .variant(IconButtonVariant::GhostMuted)
                        .label("Dismiss")
                        .on_press(|app: &mut App| app.drop_failed = None)
                        .build(p.k)
                        .key("drop-failed-dismiss"),
                ),
        );
    }
    if app.backend == Backend::Controller && app.filters_open {
        head = head.child(filters(app, p));
    }

    col()
        .key("sidebar")
        .w(p.tune.sidebar)
        .h_full()
        .min_h(0.0)
        .fill(p.c.page)
        .child(band)
        .child(head)
        .child(rule("sidebar-head-rule", false, p))
        .child(pack_list(app, p))
        .children((app.backend == Backend::Controller).then(|| pages_nav(app, p)))
        .child(rule("sidebar-foot-rule", false, p))
        .children(app.act_as_refused.as_ref().map(|why| {
            text(why.clone())
                .key("act-as-refused")
                .role("status")
                .font(p.k.ty.small)
                .color(p.fail)
                .wrap()
                .px(12.0)
                .pt(8.0)
        }))
        .child(account(app, source, p, layer))
}

/// The controller's pages, under the playbooks. Setup pages are behind the gear.
fn pages_nav(app: &App, p: &Paint) -> El {
    let row_ = |key: &str, title: &str, glyph: &'static Icon, on: bool, badge: usize| {
        let mut entry = SidebarRow::new(title).icon(glyph).height(28.0).selected(on);
        if badge > 0 {
            entry = entry.badge(badge.to_string());
        }
        (entry, format!("nav-{key}"))
    };
    let group = |group: crate::Group, glyph: &'static Icon, badge: usize| {
        let on = app.page.group() == Some(group);
        let (entry, key) = row_(group.id(), group.title(), glyph, on, badge);
        entry
            .on_select(move |app: &mut App, _: &mut Tx| app.toggle_group(group))
            .build(p.k)
            .key(key)
    };
    let page = |page: crate::Page, glyph: &'static Icon| {
        let (entry, key) = row_(page.id(), page.title(), glyph, app.page == page, 0);
        entry
            .on_select(move |app: &mut App, _: &mut Tx| app.toggle_page(page))
            .build(p.k)
            .key(key)
    };
    col()
        .key("pages-nav")
        .w_full()
        .px(8.0)
        .pb(6.0)
        .gap(2.0)
        .child(rule("pages-nav-rule", false, p).mb(6.0))
        .child(group(
            crate::Group::Inbox,
            &oc::CIRCLE_CHECK,
            app.waiting() + app.approvals_waiting(),
        ))
        .child(page(crate::Page::Triggers, &oc::STATUS))
        .child(page(crate::Page::Blueprints, &oc::GRID_PLUS))
        .child(group(crate::Group::Activity, &oc::CHECKLIST, 0))
}

fn search(app: &App, p: &Paint) -> El {
    let placeholder = match app.backend {
        Backend::Controller => "Filter playbooks",
        Backend::Local => "Filter packs",
    };
    let filterable = app.backend == Backend::Controller;
    let toggle = if filterable { FILTER_TOGGLE + 6.0 } else { 0.0 };
    let mut input = TextInput::new(&app.playbook_search)
        .placeholder(placeholder)
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .width(Some(p.tune.sidebar - 2.0 * SIDEBAR_PAD - toggle))
        .on_edit(|app: &mut App| &mut app.playbook_search);
    if !app.playbook_search.text.is_empty() {
        input = input.clear_button();
    }
    let filtered = app.owner_filter != OwnerFilter::All
        || app.origin_filter != OriginFilter::All
        || app.landed_filter != LandedFilter::Any;
    row()
        .w_full()
        .gap(6.0)
        .center_y()
        .child(input.build(p.k).key("playbook-search"))
        .children(filterable.then(|| {
            IconButton::new(&oc::OUTLINE_DOTS)
                .variant(if filtered {
                    IconButtonVariant::Contrast
                } else if app.filters_open {
                    IconButtonVariant::Neutral
                } else {
                    IconButtonVariant::GhostMuted
                })
                .label("Filters")
                .on_press(|app: &mut App| app.filters_open = !app.filters_open)
                .build(p.k)
                .key("filters-toggle")
        }))
}

fn filters(app: &App, p: &Paint) -> El {
    col()
        .key("filters")
        .w_full()
        .gap(6.0)
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
    row()
        .key(format!("filter-{}", slug(label)))
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            text(label)
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .w(FILTER_LABEL),
        )
        .child(
            SegmentedControl::new(choices, selected)
                .label(label)
                .width(Some(
                    p.tune.sidebar - 2.0 * SIDEBAR_PAD - FILTER_LABEL - 8.0,
                ))
                .on_pick(pick)
                .build(p.k),
        )
}

fn slug(label: &str) -> String {
    label.to_ascii_lowercase().replace(' ', "-")
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
        Backend::Controller => match &*app.remote.live {
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

fn unavailable(error: &Down, app: &App, p: &Paint) -> El {
    if let crate::profile::Readiness::Starting(what) = app.remote.target.readiness() {
        return row()
            .key("live-starting")
            .w_full()
            .gap(8.0)
            .padding(10.0)
            .center_y()
            .child(Spinner::new().size(14.0).build(p.k))
            .child(
                text(format!("{what}\u{2026}"))
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted)
                    .wrap(),
            );
    }
    let s = p.tone(Tone::Danger);
    let refused = matches!(error, Down::Refused(_));
    let action = if app.remote.signing_in {
        row()
            .gap(8.0)
            .center_y()
            .child(Spinner::new().size(14.0).build(p.k))
            .child(
                text("Waiting for the browser\u{2026}")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            )
            .child(el().grow(1.0))
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(|app: &mut App| app.cancel_sign_in())
                    .build(p.k)
                    .key("sign-in-cancel"),
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
            text(error.why().to_string())
                .font(p.mono(11.0, 400.0))
                .color(s.fg)
                .max_lines(4)
                .wrap(),
        )
        .child(
            text(format!(
                "{} \u{b7} {}",
                app.remote.target.name,
                app.remote.target.url()
            ))
            .font(p.mono(11.0, 400.0))
            .color(p.c.text_muted)
            .truncate(),
        )
        .children(refused.then_some(action))
        .children(refused.then(|| {
            TextInput::new(&app.remote.key_paste)
                .placeholder("or paste a crk_ key")
                .secure()
                .width(Some(p.tune.sidebar - 2.0 * SIDEBAR_PAD - 20.0))
                .on_edit(|app: &mut App| &mut app.remote.key_paste)
                .on_submit(|app: &mut App, _: &mut Tx| app.save_pasted_key())
                .build(p.k)
                .key("sign-in-key")
        }))
        .children(
            app.remote
                .sign_in_error
                .as_ref()
                .map(|why| text(why.clone()).font(p.k.ty.small).color(s.fg).wrap()),
        )
}

fn row_meta(playbook: &LivePlaybook) -> String {
    let origin = if playbook.origin.is_empty() {
        "launch"
    } else {
        playbook.origin.as_str()
    };
    match playbook.status {
        LaunchStatus::Done => origin.to_string(),
        _ => format!("{} \u{b7} {origin}", playbook.status),
    }
}

pub(crate) fn age(landed: i64, now: i64) -> String {
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
            p,
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

/// The flare around a run a launch just started: an ember ring and glow that spring out from its
/// status mark and fade.
fn ignited(mark: El, spark: Spark, ember: Color) -> El {
    let spent = spark == Spark::Spent;
    let burn = rune::Spring::new(crate::ignition::BURN as f32, 0.0);
    let flare = |key: &str, grown: f32, lit: f32| {
        let side = if spent { IGNITION * grown } else { IGNITION };
        let at = (IGNITION - side) / 2.0;
        el().key(key)
            .absolute(at, at)
            .size(side, side)
            .radius(side / 2.0)
            .opacity(if spent { 0.0 } else { lit })
            .motion(burn)
            .opacity_motion(burn)
            .motion_intent(rune::MotionIntent::PULSE)
            .a11y_hidden()
    };
    row()
        .size(IGNITION, IGNITION)
        .center()
        .child(flare("ignition-glow", 2.2, 0.45).fill(ember))
        .child(flare("ignition-ring", 3.4, 0.9).border(1.5, ember))
        .child(mark)
}

/// The side of the status slot the flare starts from.
const IGNITION: f32 = 12.0;

fn live_rows(app: &App, playbooks: &[&LivePlaybook], now: i64, p: &Paint) -> Vec<El> {
    let current = app.selected_playbook();
    playbooks
        .iter()
        .map(|playbook| {
            let id = playbook.id.clone();
            let status = match (
                verdict_status(playbook.verdict(), p),
                app.ignition.spark(&playbook.id),
            ) {
                (RowStatus::Other(mark), Some(spark)) => {
                    RowStatus::Other(ignited(mark, spark, p.live))
                }
                (status, _) => status,
            };
            let mut item = SidebarRow::new(playbook.id.clone())
                .subtitle(row_meta(playbook))
                .height(ROW * p.text)
                .status(status)
                .selected(current == Some(playbook.id.as_str()))
                .on_select(move |app: &mut App, _: &mut Tx| app.select(Selection::playbook(&id)));
            if playbook.landed > 0 {
                item = item.detail(age(playbook.landed, now));
            }
            item.build(p.k).key(format!("live-{}", playbook.id))
        })
        .collect()
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
        .height(ROW * p.text)
        .status(verdict_status(pack.verdict, p))
        .selected(selected)
        .on_select(move |app: &mut App, _: &mut Tx| app.select(Selection::Local(name.clone())))
        .build(p.k)
        .key(format!("pack-{}", pack.name))
}

pub(crate) fn verdict_color(verdict: Verdict, p: &Paint) -> Color {
    match verdict {
        Verdict::Valid => p.pass,
        Verdict::Invalid | Verdict::Error => p.fail,
        Verdict::Running => p.live,
        Verdict::Cancelled => p.c.text_muted,
        Verdict::Unfinished | Verdict::NotRun => p.c.text_faint,
    }
}

pub(crate) fn verdict_icon(verdict: Verdict) -> Option<&'static Icon> {
    match verdict {
        Verdict::Valid => Some(&oc::CIRCLE_CHECK),
        Verdict::Invalid | Verdict::Error => Some(&oc::CIRCLE_X),
        Verdict::Cancelled => Some(&oc::CIRCLE_BAN_SIGN),
        Verdict::Running => Some(&oc::STATUS),
        Verdict::Unfinished | Verdict::NotRun => None,
    }
}

#[cfg(test)]
mod tests {
    use rune::{Color, Cx, El, Env, Runtime, el};
    use rune_kit::shell::{RowStatus, SidebarRow};

    use crate::ignition::Spark;
    use crate::ui::sidebar::{IGNITION, ignited};

    fn row(spark: &Spark, _: &mut Cx<Spark>) -> El {
        let mark = el().size(7.0, 7.0).radius(3.5).fill(Color::WHITE);
        SidebarRow::new("triage")
            .status(RowStatus::Other(ignited(mark, *spark, Color::WHITE)))
            .build(&rune_kit::Kit::neutral(true))
            .key("row")
    }

    #[test]
    fn a_spent_flare_spreads_past_its_status_mark_and_fades() {
        let mut rt = Runtime::new(Spark::Lit, row, (320.0, 80.0), Env::new(true, 1.0));
        rt.frame(0.0);
        let find = |rt: &Runtime<Spark>, key: &str| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("no {key}"))
        };
        let lit = find(&rt, "ignition-ring");
        assert_eq!(lit.rect[2], IGNITION);
        assert!(lit.opacity > 0.8);

        rt.state = Spark::Spent;
        rt.invalidate();
        for _ in 0..240 {
            rt.frame(1.0 / 60.0);
        }
        let spent = find(&rt, "ignition-ring");
        assert!(spent.rect[2] > IGNITION * 3.0, "grown: {:?}", spent.rect);
        assert!(
            spent.visible[2] > IGNITION * 2.0,
            "not clipped to the slot: {:?}",
            spent.visible
        );
        assert!(spent.opacity < 0.05, "faded: {}", spent.opacity);
    }
}
