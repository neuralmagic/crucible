//! The Teams page: every team the caller may see, and for one they own, its members, name and
//! audit trail.

use crux::dto::{MemberKind, Team, TeamMember, TeamRole};
use rune::{El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, SidebarRow, TextInput};

use crate::App;
use crate::admin::{Action, relative};
use crate::teams::{
    KINDS, ROLES, TeamForm, bodies, keeps_an_owner, kind_label, role_label, sorted_members,
    sorted_teams, with_member, with_role, without_member,
};
use crate::ui::desk::{
    card, field, frame, heading, landed, list, none, now, pills, reading, scrolled, stacked,
};
use crate::ui::{Paint, Tone, flag, tag};

const LIST: f32 = 280.0;

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.remote.admin.teams {
        Some(Ok(teams)) => {
            let mine = teams.iter().filter(|t| t.my_role.is_some()).count();
            format!("{} \u{b7} {mine} yours", teams.len())
        }
        _ => String::new(),
    };
    let create = Button::new("New team")
        .variant(ButtonVariant::Neutral)
        .disabled(app.remote.desk.new_team.is_some())
        .on_press(|app: &mut App| app.remote.desk.new_team = Some(TeamForm::default()))
        .build(p.k)
        .key("team-new");
    let page = frame("teams", "Teams", note, Some(create), app, p);
    let teams = match landed(&app.remote.admin.teams, "the teams", p) {
        Err(standin) => return page.child(*standin),
        Ok(teams) => teams,
    };
    let rows = sorted_teams(teams)
        .into_iter()
        .map(|team| {
            let slug = team.slug.clone();
            let role = team.my_role.map_or("", role_label);
            let count = team.members.len();
            let members = format!("{count} member{}", if count == 1 { "" } else { "s" });
            let subtitle = if role.is_empty() {
                members
            } else {
                format!("{role} \u{b7} {members}")
            };
            SidebarRow::new(team.display_name.clone())
                .subtitle(subtitle)
                .height(46.0)
                .selected(
                    app.remote.desk.new_team.is_none()
                        && app.remote.desk.team.as_deref() == Some(team.slug.as_str()),
                )
                .on_select(move |app: &mut App, _: &mut Tx| app.remote.desk.pick_team(&slug))
                .build(p.k)
                .key(format!("team-{}", team.slug))
        })
        .collect();
    let picked = app
        .remote
        .desk
        .team
        .as_ref()
        .and_then(|slug| teams.iter().find(|team| &team.slug == slug));
    let shown = match (&app.remote.desk.new_team, picked) {
        (Some(form), _) => new_team(form, app, p),
        (None, Some(team)) => detail(team, app, p),
        (None, None) if teams.is_empty() => none("teams-none", "No teams", p),
        (None, None) => none("team-unpicked", "Pick a team", p),
    };
    page.child(
        row()
            .key("teams-body")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .gap(16.0)
            .child(
                card("teams-list-card", list("teams-list", rows, p), p)
                    .w(LIST)
                    .h_full(),
            )
            .child(
                col()
                    .key("team-detail")
                    .grow(1.0)
                    .min_w(0.0)
                    .h_full()
                    .min_h(0.0)
                    .child(scrolled("team-scroll", shown, p)),
            ),
    )
}

fn new_team(form: &TeamForm, app: &App, p: &Paint) -> El {
    let asked = form.ask();
    let problem = match &asked {
        Err(why) if !form.slug.text.trim().is_empty() => Some(why.clone()),
        _ => None,
    };
    let body = col()
        .key("team-form")
        .w_full()
        .gap(14.0)
        .child(text("New team").font(p.k.ty.ui_medium).color(p.c.text_base))
        .child(field(
            "Slug",
            TextInput::new(&form.slug)
                .placeholder("lowercase-slug")
                .width(Some(320.0))
                .on_edit(|app: &mut App| match &mut app.remote.desk.new_team {
                    Some(form) => &mut form.slug,
                    None => &mut app.scratch,
                })
                .build(p.k)
                .key("team-slug"),
            p,
        ))
        .child(field(
            "Name",
            TextInput::new(&form.name)
                .width(Some(320.0))
                .on_edit(|app: &mut App| match &mut app.remote.desk.new_team {
                    Some(form) => &mut form.name,
                    None => &mut app.scratch,
                })
                .build(p.k)
                .key("team-name"),
            p,
        ))
        .children(problem.map(|why| text(why).font(p.k.ty.small).color(p.fail)))
        .child(
            row()
                .gap(8.0)
                .child(
                    Button::new("Create")
                        .variant(ButtonVariant::Submit)
                        .disabled(asked.is_err() || app.remote.desk.busy())
                        .on_press(|app: &mut App| {
                            if let Some(Ok((slug, name))) =
                                app.remote.desk.new_team.as_ref().map(TeamForm::ask)
                            {
                                app.act(Action::CreateTeam { slug, name });
                            }
                        })
                        .build(p.k)
                        .key("team-create"),
                )
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|app: &mut App| app.remote.desk.new_team = None)
                        .build(p.k)
                        .key("team-create-cancel"),
                ),
        );
    card("team-form-card", body, p)
}

fn detail(team: &Team, app: &App, p: &Paint) -> El {
    let owner = team.my_role == Some(TeamRole::Owner);
    let mut body = col()
        .key(format!("team-shown-{}", team.slug))
        .w_full()
        .gap(16.0)
        .child(card("team-head-card", head(team, owner, app, p), p))
        .child(card("team-members-card", members(team, owner, app, p), p));
    if owner {
        body = body
            .child(card("team-add-card", add_member(team, app, p), p))
            .child(card("team-audit-card", audit(team, app, p), p));
    }
    body
}

fn head(team: &Team, owner: bool, app: &App, p: &Paint) -> El {
    let mut title = row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(team.display_name.clone())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base),
        )
        .child(
            text(team.slug.clone())
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_faint),
        );
    if let Some(role) = team.my_role {
        title = title.child(tag(role_label(role), p));
    }
    if !team.reachable {
        title = title.child(flag("reaches no user", Tone::Warning, None, p));
    }
    title = title.child(el().grow(1.0));
    let mut column = col().key("team-head").w_full().gap(12.0);
    if !owner {
        return column.child(title);
    }
    let slug = team.slug.clone();
    let name = team.display_name.clone();
    let armed = app.remote.desk.armed.as_deref() == Some(delete_key(&team.slug).as_str());
    title = title
        .child(
            Button::new("Rename")
                .variant(ButtonVariant::Ghost)
                .disabled(app.remote.desk.busy() || app.remote.desk.rename_team.is_some())
                .on_press(move |app: &mut App| {
                    app.remote.desk.rename_team = Some(rune::TextEdit::new(&name));
                })
                .build(p.k)
                .key("team-rename"),
        )
        .child(
            Button::new(if armed { "Confirm delete" } else { "Delete" })
                .variant(if armed {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Ghost
                })
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    let key = delete_key(&slug);
                    if app.remote.desk.armed.as_deref() == Some(key.as_str()) {
                        app.act(Action::DeleteTeam { slug: slug.clone() });
                    } else {
                        app.remote.desk.armed = Some(key);
                    }
                })
                .build(p.k)
                .key("team-delete"),
        );
    column = column.child(title);
    if let Some(next) = &app.remote.desk.rename_team {
        let slug = team.slug.clone();
        let ready = !next.text.trim().is_empty() && !app.remote.desk.busy();
        column = column.child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    TextInput::new(next)
                        .width(Some(320.0))
                        .on_edit(|app: &mut App| match &mut app.remote.desk.rename_team {
                            Some(next) => next,
                            None => &mut app.scratch,
                        })
                        .build(p.k)
                        .key("team-rename-name"),
                )
                .child(
                    Button::new("Save")
                        .variant(ButtonVariant::Submit)
                        .disabled(!ready)
                        .on_press(move |app: &mut App| {
                            if let Some(next) = &app.remote.desk.rename_team {
                                let name = next.text.trim().to_string();
                                app.act(Action::RenameTeam {
                                    slug: slug.clone(),
                                    name,
                                });
                            }
                        })
                        .build(p.k)
                        .key("team-rename-save"),
                )
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|app: &mut App| app.remote.desk.rename_team = None)
                        .build(p.k)
                        .key("team-rename-cancel"),
                ),
        );
    }
    column
}

fn delete_key(slug: &str) -> String {
    format!("team-delete:{slug}")
}

fn member_key(member: &TeamMember) -> String {
    format!("{}:{}", kind_label(member.kind), member.member)
}

fn members(team: &Team, owner: bool, app: &App, p: &Paint) -> El {
    let column = col()
        .key("team-members")
        .w_full()
        .gap(6.0)
        .child(heading("Members", p));
    if team.members.is_empty() {
        return column.child(none("team-members-none", "None", p));
    }
    let rows = sorted_members(&team.members)
        .into_iter()
        .map(|member| member_row(team, member, owner, app, p))
        .collect();
    column.child(stacked("team-members-list", rows, p))
}

fn member_row(team: &Team, member: &TeamMember, owner: bool, app: &App, p: &Paint) -> El {
    let key = member_key(member);
    let mut line = row()
        .key(format!("member-{key}"))
        .w_full()
        .py(8.0)
        .gap(10.0)
        .center_y()
        .child(
            text(member.member.clone())
                .font(p.mono(12.0, 520.0))
                .color(p.c.text_base)
                .truncate(),
        )
        .child(tag(kind_label(member.kind), p));
    if member.kind == MemberKind::User && !member.signed_in {
        line = line.child(
            text("never signed in")
                .font(p.k.ty.small)
                .color(p.c.text_faint),
        );
    }
    line = line.child(el().grow(1.0));
    if !owner {
        return line.child(
            text(role_label(member.role))
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        );
    }
    let held = bodies(&team.members);
    let (kind, name) = (member.kind, member.member.clone());
    let roles: Vec<(String, String)> = ROLES
        .iter()
        .filter(|role| {
            **role == member.role || keeps_an_owner(&with_role(held.clone(), kind, &name, **role))
        })
        .map(|role| (role_label(*role).to_string(), role_label(*role).to_string()))
        .collect();
    let (slug, list, who) = (team.slug.clone(), held.clone(), name.clone());
    line = line.child(pills(
        &format!("member-role-{key}"),
        &roles,
        Some(role_label(member.role)),
        move |app: &mut App, picked: &str| {
            let Some(role) = ROLES.iter().find(|role| role_label(**role) == picked) else {
                return;
            };
            if app.remote.desk.busy() {
                return;
            }
            app.act(Action::PutMembers {
                slug: slug.clone(),
                members: with_role(list.clone(), kind, &who, *role),
            });
        },
        p,
    ));
    let rest = without_member(held, kind, &name);
    let armed = app.remote.desk.armed.as_deref() == Some(key.as_str());
    let (slug, armed_key) = (team.slug.clone(), key.clone());
    line.child(
        Button::new(if armed { "Confirm remove" } else { "Remove" })
            .variant(if armed {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Ghost
            })
            .disabled(app.remote.desk.busy() || !keeps_an_owner(&rest))
            .on_press(move |app: &mut App| {
                if app.remote.desk.armed.as_deref() == Some(armed_key.as_str()) {
                    app.act(Action::PutMembers {
                        slug: slug.clone(),
                        members: rest.clone(),
                    });
                } else {
                    app.remote.desk.armed = Some(armed_key.clone());
                }
            })
            .build(p.k)
            .key(format!("member-remove-{key}")),
    )
}

fn add_member(team: &Team, app: &App, p: &Paint) -> El {
    let form = &app.remote.desk.add_member;
    let kinds: Vec<(String, String)> = KINDS
        .iter()
        .map(|kind| (kind_label(*kind).to_string(), kind_label(*kind).to_string()))
        .collect();
    let roles: Vec<(String, String)> = ROLES
        .iter()
        .map(|role| (role_label(*role).to_string(), role_label(*role).to_string()))
        .collect();
    let (slug, held) = (team.slug.clone(), bodies(&team.members));
    col()
        .key("team-add")
        .w_full()
        .gap(12.0)
        .child(heading("Add member", p))
        .child(pills(
            "member-kind",
            &kinds,
            Some(kind_label(form.kind)),
            |app: &mut App, picked: &str| {
                if let Some(kind) = KINDS.iter().find(|kind| kind_label(**kind) == picked) {
                    app.remote.desk.add_member.kind = *kind;
                }
            },
            p,
        ))
        .child(
            TextInput::new(&form.member)
                .placeholder(match form.kind {
                    MemberKind::User => "login",
                    MemberKind::Group => "group path",
                    MemberKind::Team => "team slug",
                    MemberKind::Rule => "email-domain:example.com",
                })
                .width(Some(320.0))
                .on_edit(|app: &mut App| &mut app.remote.desk.add_member.member)
                .build(p.k)
                .key("member-name"),
        )
        .children(crate::ui::desk::suggestions(
            "member-name-suggest",
            &form.member.text,
            crate::principals::Want::Member(form.kind),
            |app: &mut App, picked: &str| {
                app.remote.desk.add_member.member = rune::TextEdit::new(picked);
            },
            app,
            p,
        ))
        .child(pills(
            "member-add-role",
            &roles,
            Some(role_label(form.role)),
            |app: &mut App, picked: &str| {
                if let Some(role) = ROLES.iter().find(|role| role_label(**role) == picked) {
                    app.remote.desk.add_member.role = *role;
                }
            },
            p,
        ))
        .child(
            row().child(
                Button::new("Add")
                    .variant(ButtonVariant::Submit)
                    .disabled(form.ask().is_none() || app.remote.desk.busy())
                    .on_press(move |app: &mut App| {
                        if let Some(added) = app.remote.desk.add_member.ask() {
                            app.act(Action::PutMembers {
                                slug: slug.clone(),
                                members: with_member(held.clone(), added),
                            });
                        }
                    })
                    .build(p.k)
                    .key("member-add"),
            ),
        )
}

fn audit(team: &Team, app: &App, p: &Paint) -> El {
    let column = col()
        .key("team-audit")
        .w_full()
        .gap(6.0)
        .child(heading("Audit", p));
    let trail = match &app.remote.admin.team_audit {
        Some((slug, loaded)) if slug == &team.slug => match landed(loaded, "the audit trail", p) {
            Ok(trail) => trail,
            Err(standin) => return column.child(*standin),
        },
        _ => return column.child(reading("the audit trail", p)),
    };
    if trail.is_empty() {
        return column.child(none("team-audit-none", "Nothing yet", p));
    }
    let now = now();
    let rows = trail
        .iter()
        .map(|entry| {
            row()
                .key(format!("team-audit-{}", entry.id))
                .w_full()
                .py(6.0)
                .gap(12.0)
                .child(text(entry.action.clone()).font(p.mono(12.0, 520.0)).color(
                    if entry.decision == "deny" {
                        p.fail
                    } else {
                        p.c.text_base
                    },
                ))
                .child(
                    text(entry.actor.clone())
                        .font(p.mono(12.0, 420.0))
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    text(relative(&entry.at, now))
                        .font(p.mono(12.0, 420.0))
                        .color(p.c.text_muted),
                )
        })
        .collect();
    column.child(stacked("team-audit-list", rows, p))
}
