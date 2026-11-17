//! The report view: every lane's `verdicts.json` as one row per ticket, by disposition.

use std::collections::{BTreeMap, BTreeSet};

use crucible_view::{
    Bar, Cell, Column, ColumnKind, Context2d, Control, FlowLink, FlowNode, LineCap, Node, PlotKind,
    Request, Row, Scale, Segment, Series, Stat, Tab, TextAlign, TextBaseline, Tone, View, Width,
    file_link, task_link,
};
use serde::Deserialize;

crucible_view::plugin!(inputs = ["verdicts.json"], view = report);

const TICKETS: &str = "https://issues.example.com/browse/";

/// The verdicts are read as of this moment, in Unix seconds: 1 Nov 2026, UTC.
const AS_OF: f64 = 1_793_491_200.0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Affected,
    NeedsReview,
    SbomInaccurate,
    AlreadyFixed,
    NotShipped,
    NotAffected,
}

impl Category {
    const ALL: [Category; 6] = [
        Category::Affected,
        Category::NeedsReview,
        Category::SbomInaccurate,
        Category::AlreadyFixed,
        Category::NotShipped,
        Category::NotAffected,
    ];

    fn of(lane: &str, verdict: &str, close: &str, rationale: &str) -> Category {
        let scan_absent =
            rationale.contains("final-artifact package scan for") && rationale.contains("absent");
        match (close, verdict) {
            ("not_shipped", _) => Category::NotShipped,
            _ if lane == "absent" || scan_absent => Category::SbomInaccurate,
            ("already_fixed", _) => Category::AlreadyFixed,
            (_, "affected") => Category::Affected,
            (_, "not_affected") => Category::NotAffected,
            _ => Category::NeedsReview,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Category::Affected => "affected",
            Category::NeedsReview => "needs_review",
            Category::SbomInaccurate => "sbom_inaccurate",
            Category::AlreadyFixed => "already_fixed",
            Category::NotShipped => "not_shipped",
            Category::NotAffected => "not_affected",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Category::Affected => Tone::Bad,
            Category::NeedsReview => Tone::Warn,
            Category::SbomInaccurate => Tone::Info,
            Category::AlreadyFixed | Category::NotAffected => Tone::Good,
            Category::NotShipped => Tone::Muted,
        }
    }
}

#[derive(Deserialize)]
struct Record {
    ticket_key: String,
    #[serde(default)]
    jira_status: String,
    #[serde(default)]
    age_days: f64,
    #[serde(default)]
    verdict_property: Verdict,
}

#[derive(Default, Deserialize)]
struct Verdict {
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    cve_ids: Vec<String>,
    #[serde(default)]
    package: Package,
    #[serde(default)]
    release: Named,
    #[serde(default)]
    artifact: Artifact,
    #[serde(default)]
    proposed_actions: Actions,
    #[serde(default)]
    rationale: String,
}

#[derive(Default, Deserialize)]
struct Package {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Default, Deserialize)]
struct Named {
    #[serde(default)]
    name: String,
}

#[derive(Default, Deserialize)]
struct Artifact {
    #[serde(default)]
    locator: String,
}

#[derive(Default, Deserialize)]
struct Actions {
    #[serde(default)]
    lane: String,
    #[serde(default)]
    cvss: Option<Cvss>,
}

#[derive(Deserialize)]
struct Cvss {
    score: f64,
}

struct Ticket {
    key: String,
    status: String,
    age_days: f64,
    category: Category,
    verdict: Verdict,
    source: String,
    /// The address of the captured file the ticket came from.
    file: String,
}

/// The rest of the line after `label` in `text`.
fn noted<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(label))
        .map_or("", str::trim)
}

fn report(request: Request) -> Result<View, String> {
    let tasks: BTreeSet<&str> = request.files.iter().map(|f| f.task.as_str()).collect();
    let task = request
        .select("task")
        .filter(|task| tasks.contains(task))
        .unwrap_or("all");
    let mut options = vec![("all", "All")];
    options.extend(tasks.iter().map(|task| (*task, *task)));
    let mut tickets: BTreeMap<String, Ticket> = BTreeMap::new();
    for file in request
        .files
        .iter()
        .filter(|file| task == "all" || file.task == task)
    {
        let records: Vec<Record> = serde_json::from_str(&file.text)
            .map_err(|e| format!("{} from {}: {e}", file.path, file.task))?;
        for record in records {
            let verdict = record.verdict_property;
            let category = Category::of(
                &verdict.proposed_actions.lane,
                &verdict.verdict,
                noted(&verdict.rationale, "Close recommendation:"),
                &verdict.rationale,
            );
            tickets.insert(
                record.ticket_key.clone(),
                Ticket {
                    key: record.ticket_key,
                    status: record.jira_status,
                    age_days: record.age_days,
                    category,
                    verdict,
                    source: file.task.clone(),
                    file: file_link(file),
                },
            );
        }
    }
    let mut tickets: Vec<Ticket> = tickets.into_values().collect();
    tickets.sort_by(|a, b| (a.category, &a.key).cmp(&(b.category, &b.key)));

    let count = |category: Category| tickets.iter().filter(|t| t.category == category).count();
    let closable = tickets.len() - count(Category::Affected) - count(Category::NeedsReview);
    let stats = Node::Stats {
        items: vec![
            stat("Triaged", tickets.len(), Tone::Neutral),
            stat("Affected", count(Category::Affected), Tone::Bad),
            stat("Needs review", count(Category::NeedsReview), Tone::Warn),
            stat("Closable", closable, Tone::Good),
        ],
    };

    let mut tabs = vec![
        Tab {
            label: "Summary".into(),
            count: None,
            body: Node::Stack {
                children: vec![
                    by_release(&tickets),
                    Node::Columns {
                        children: vec![
                            meter("Closable", closable, tickets.len(), Tone::Good),
                            meter(
                                "Affected",
                                count(Category::Affected),
                                tickets.len(),
                                Tone::Bad,
                            ),
                            meter(
                                "Needs review",
                                count(Category::NeedsReview),
                                tickets.len(),
                                Tone::Warn,
                            ),
                        ],
                    },
                    pivot(&tickets),
                    Node::Section {
                        id: "categorised".into(),
                        title: "How a ticket is categorised".into(),
                        open: false,
                        body: Box::new(categories()),
                    },
                ],
            },
        },
        Tab {
            label: "Charts".into(),
            count: None,
            body: Node::Stack {
                children: vec![
                    Node::Columns {
                        children: vec![across_releases(&tickets), age_by_score(&tickets)],
                    },
                    how_decided(&tickets),
                    Node::Columns {
                        children: vec![days_open(&tickets), by_category_and_release(&tickets)],
                    },
                ],
            },
        },
        Tab {
            label: "Canvas".into(),
            count: None,
            body: drawing(&tickets),
        },
        Tab {
            label: "All".into(),
            count: Some(tickets.len() as u64),
            body: table("all", tickets.iter()),
        },
    ];
    for category in Category::ALL {
        tabs.push(Tab {
            label: category.label().into(),
            count: Some(count(category) as u64),
            body: table(
                category.label(),
                tickets.iter().filter(|t| t.category == category),
            ),
        });
    }
    Ok(View {
        title: "Triage report".into(),
        body: Node::Stack {
            children: vec![
                stats,
                Node::Tabs {
                    id: "category".into(),
                    tabs,
                },
            ],
        },
        controls: vec![Control::select("task", "Lane", &options, task)],
    })
}

fn stat(label: &str, value: usize, tone: Tone) -> Stat {
    Stat {
        label: label.into(),
        value: value.to_string(),
        tone,
    }
}

fn meter(label: &str, value: usize, of: usize, tone: Tone) -> Node {
    Node::Meter {
        label: label.into(),
        value: value as f64,
        max: of.max(1) as f64,
        tone,
    }
}

fn by_release(tickets: &[Ticket]) -> Node {
    Node::Bars {
        bars: releases(tickets)
            .into_iter()
            .map(|release| Bar {
                label: release.into(),
                segments: Category::ALL
                    .into_iter()
                    .map(|category| Segment {
                        label: category.label().into(),
                        value: tickets
                            .iter()
                            .filter(|t| t.verdict.release.name == release)
                            .filter(|t| t.category == category)
                            .count() as f64,
                        tone: category.tone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn releases(tickets: &[Ticket]) -> Vec<&str> {
    tickets
        .iter()
        .map(|t| t.verdict.release.name.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Each category's count per release, a release a category on the x axis.
fn across_releases(tickets: &[Ticket]) -> Node {
    let releases = releases(tickets);
    Node::Plot {
        kind: PlotKind::Line,
        x: "release".into(),
        y: "tickets".into(),
        series: Category::ALL
            .into_iter()
            .map(|category| Series {
                label: category.label().into(),
                tone: category.tone(),
                points: releases
                    .iter()
                    .enumerate()
                    .map(|(i, release)| {
                        let count = tickets
                            .iter()
                            .filter(|t| t.verdict.release.name == *release)
                            .filter(|t| t.category == category)
                            .count();
                        [i as f64, count as f64]
                    })
                    .collect(),
            })
            .collect(),
        x_scale: Scale::Linear,
        y_scale: Scale::Linear,
        categories: releases.iter().map(|release| release.to_string()).collect(),
    }
}

/// How long the tickets have been open.
fn days_open(tickets: &[Ticket]) -> Node {
    Node::Histogram {
        x: "days open".into(),
        values: tickets.iter().map(|t| t.age_days).collect(),
        bins: Some(8),
        tone: Tone::Neutral,
    }
}

/// Tickets per category and release.
fn by_category_and_release(tickets: &[Ticket]) -> Node {
    let releases = releases(tickets);
    Node::Heatmap {
        x: releases.iter().map(|release| release.to_string()).collect(),
        y: Category::ALL
            .iter()
            .map(|category| category.label().to_string())
            .collect(),
        values: Category::ALL
            .into_iter()
            .map(|category| {
                releases
                    .iter()
                    .map(|release| {
                        tickets
                            .iter()
                            .filter(|t| t.category == category)
                            .filter(|t| t.verdict.release.name == *release)
                            .count() as f64
                    })
                    .collect()
            })
            .collect(),
        tone: Tone::Info,
    }
}

/// Tickets flowing from the lane that triaged them, through what the decision rested on, to
/// their category.
fn how_decided(tickets: &[Ticket]) -> Node {
    let mut nodes: Vec<FlowNode> = Vec::new();
    let mut links: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut stage = |id: String, label: &str, tone: Tone| {
        if !nodes.iter().any(|node| node.id == id) {
            nodes.push(FlowNode {
                id: id.clone(),
                label: label.into(),
                tone,
            });
        }
        id
    };
    let mut flows = Vec::new();
    for ticket in tickets {
        let basis = match ticket.verdict.proposed_actions.lane.as_str() {
            "" => "unknown",
            basis => basis,
        };
        flows.push((
            stage(
                format!("lane:{}", ticket.source),
                &ticket.source,
                Tone::Muted,
            ),
            stage(format!("basis:{basis}"), basis, Tone::Neutral),
            ticket.category,
        ));
    }
    for category in Category::ALL {
        if flows.iter().any(|(.., reached)| *reached == category) {
            stage(
                format!("category:{}", category.label()),
                category.label(),
                category.tone(),
            );
        }
    }
    for (lane, basis, category) in flows {
        *links.entry((lane, basis.clone())).or_default() += 1;
        *links
            .entry((basis, format!("category:{}", category.label())))
            .or_default() += 1;
    }
    Node::Flow {
        nodes,
        links: links
            .into_iter()
            .map(|((from, to), count)| FlowLink {
                from,
                to,
                value: count as f64,
                tone: None,
            })
            .collect(),
    }
}

/// Each scored ticket's days open against its CVSS score.
fn age_by_score(tickets: &[Ticket]) -> Node {
    Node::Plot {
        kind: PlotKind::Scatter,
        x: "CVSS score".into(),
        y: "days open".into(),
        series: Category::ALL
            .into_iter()
            .map(|category| Series {
                label: category.label().into(),
                tone: category.tone(),
                points: tickets
                    .iter()
                    .filter(|t| t.category == category)
                    .filter_map(|t| {
                        let cvss = t.verdict.proposed_actions.cvss.as_ref()?;
                        Some([cvss.score, t.age_days])
                    })
                    .collect(),
            })
            .collect(),
        x_scale: Scale::Linear,
        y_scale: Scale::Linear,
        categories: vec![],
    }
}

/// A heat map of tickets by category and release beside the closable share as a ring, drawn
/// with canvas commands.
fn drawing(tickets: &[Ticket]) -> Node {
    use std::f32::consts::{FRAC_PI_2, TAU};

    let releases = releases(tickets);
    let count = |category: Category, release: &str| {
        tickets
            .iter()
            .filter(|t| t.category == category && t.verdict.release.name == release)
            .count()
    };
    let most = Category::ALL
        .into_iter()
        .flat_map(|category| releases.iter().map(move |release| (category, *release)))
        .map(|(category, release)| count(category, release))
        .max()
        .unwrap_or(0)
        .max(1);
    let (left, top, wide, tall) = (130.0, 28.0, 64.0, 26.0);
    let grid = left + wide * releases.len() as f32;
    let mut ctx = Context2d::new(grid + 190.0, top + tall * Category::ALL.len() as f32 + 12.0);

    ctx.set_font("11px monospace");
    ctx.set_text_align(TextAlign::Center);
    ctx.set_fill_style(Tone::Muted);
    for (column, release) in releases.iter().enumerate() {
        let x = left + wide * (column as f32 + 0.5);
        ctx.fill_text_with_max_width(release, x, 16.0, wide - 4.0);
    }
    for (row, category) in Category::ALL.into_iter().enumerate() {
        let y = top + tall * row as f32;
        ctx.set_text_align(TextAlign::Right);
        ctx.set_fill_style(category.tone());
        ctx.fill_text_with_max_width(category.label(), left - 10.0, y + 17.0, left - 14.0);
        ctx.set_text_align(TextAlign::Center);
        ctx.set_text_baseline(TextBaseline::Middle);
        for (column, release) in releases.iter().enumerate() {
            let n = count(category, release);
            let x = left + wide * column as f32;
            ctx.set_fill_style(category.tone());
            ctx.set_global_alpha(0.12 + 0.88 * n as f32 / most as f32);
            ctx.fill_rect(x + 1.0, y + 1.0, wide - 2.0, tall - 2.0);
            ctx.set_global_alpha(1.0);
            ctx.set_fill_style(Tone::Neutral);
            ctx.fill_text(&n.to_string(), x + wide / 2.0, y + tall / 2.0);
            ctx.hint(
                x,
                y,
                wide,
                tall,
                &format!("{} on {release}: {n}", category.label()),
            );
        }
        ctx.set_text_baseline(TextBaseline::Alphabetic);
    }

    let open = tickets
        .iter()
        .filter(|t| matches!(t.category, Category::Affected | Category::NeedsReview))
        .count();
    let share = if tickets.is_empty() {
        0.0
    } else {
        (tickets.len() - open) as f32 / tickets.len() as f32
    };
    let (cx, cy, radius) = (grid + 100.0, top + tall * 3.0, 56.0);
    ctx.set_line_width(12.0);
    ctx.set_stroke_style(Tone::Muted);
    ctx.set_global_alpha(0.3);
    ctx.begin_path();
    ctx.arc(cx, cy, radius, 0.0, TAU);
    ctx.stroke();
    ctx.set_line_width(1.5);
    ctx.save();
    ctx.translate(cx, cy);
    for _ in 0..20 {
        ctx.rotate(TAU / 20.0);
        ctx.begin_path();
        ctx.move_to(radius + 9.0, 0.0);
        ctx.line_to(radius + 13.0, 0.0);
        ctx.stroke();
    }
    ctx.restore();
    ctx.set_line_width(12.0);
    ctx.set_global_alpha(1.0);
    ctx.set_stroke_style(Tone::Good);
    ctx.set_line_cap(LineCap::Round);
    ctx.begin_path();
    ctx.arc(cx, cy, radius, -FRAC_PI_2, -FRAC_PI_2 + TAU * share);
    ctx.stroke();
    ctx.set_fill_style(Tone::Neutral);
    ctx.set_font("bold 20px monospace");
    ctx.fill_text(&format!("{:.0}%", share * 100.0), cx, cy + 4.0);
    ctx.set_fill_style(Tone::Muted);
    ctx.set_font("11px sans-serif");
    ctx.fill_text("closable", cx, cy + 22.0);
    ctx.hint(
        cx - radius,
        cy - radius,
        radius * 2.0,
        radius * 2.0,
        &format!("{} of {} closable", tickets.len() - open, tickets.len()),
    );
    ctx.finish()
}

fn pivot(tickets: &[Ticket]) -> Node {
    let statuses: BTreeSet<&str> = tickets.iter().map(|t| t.status.as_str()).collect();
    let tally = |category: Option<Category>, status: Option<&str>| {
        tickets
            .iter()
            .filter(|t| category.is_none_or(|c| t.category == c))
            .filter(|t| status.is_none_or(|s| t.status == s))
            .count() as f64
    };
    let line = |label: &str, tone: Tone, category: Option<Category>| {
        let mut cells = vec![
            Cell::text(label).tone(tone),
            Cell::count(tally(category, None)),
        ];
        cells.extend(
            statuses
                .iter()
                .map(|status| Cell::count(tally(category, Some(status)))),
        );
        cells
    };
    let mut columns = vec!["category".to_string(), "total".to_string()];
    columns.extend(statuses.iter().map(|status| status.to_string()));
    let mut rows: Vec<Vec<Cell>> = Category::ALL
        .into_iter()
        .map(|category| line(category.label(), category.tone(), Some(category)))
        .collect();
    rows.push(line("total", Tone::Neutral, None));
    Node::Grid { columns, rows }
}

/// How a ticket gets its category, as a diagram.
fn categories() -> Node {
    Node::Mermaid {
        source: "flowchart LR
    ticket([Ticket]) --> shipped{Shipped?}
    shipped -- no --> not_shipped
    shipped -- yes --> present{In the final image?}
    present -- no --> sbom_inaccurate
    present -- yes --> fixed{Already fixed?}
    fixed -- yes --> already_fixed
    fixed -- no --> verdict{Verdict}
    verdict -- affected --> affected
    verdict -- not affected --> not_affected
    verdict -- neither --> needs_review"
            .into(),
    }
}

/// What a selected row shows under the table: the whole rationale and where it came from.
fn detail(ticket: &Ticket) -> String {
    let v = &ticket.verdict;
    format!(
        "### {key}: {category}\n\n{rationale}\n\n\
         | Field | Value |\n|---|---|\n\
         | Package | `{package} {version}` |\n\
         | Image | `{image}` |\n\
         | CVE | {cve} |\n\
         | Confidence | {confidence} |\n\n\
         Triaged by [{task}]({task_at}), from [its verdicts]({file}). \
         [Open the ticket]({TICKETS}{key}).\n",
        key = ticket.key,
        category = ticket.category.label(),
        rationale = v.rationale.replace('\n', "\n\n"),
        package = v.package.name,
        version = v.package.version,
        image = v.artifact.locator,
        cve = v.cve_ids.join(", "),
        confidence = v.confidence,
        task = ticket.source,
        task_at = task_link(&ticket.source),
        file = ticket.file,
    )
}

/// The tone of a CVSS score: bad from 9, warn from 7.
fn severity(score: f64) -> Tone {
    if score >= 9.0 {
        Tone::Bad
    } else if score >= 7.0 {
        Tone::Warn
    } else {
        Tone::Neutral
    }
}

fn table<'a>(id: &str, tickets: impl Iterator<Item = &'a Ticket>) -> Node {
    let column = |id: &str, title: &str, kind: ColumnKind, width: Width| Column {
        id: id.into(),
        title: title.into(),
        kind,
        width,
    };
    Node::Table {
        id: id.into(),
        columns: vec![
            column("ticket", "Ticket", ColumnKind::Mono, Width::Narrow),
            column("status", "Status", ColumnKind::Text, Width::Narrow),
            column("category", "Category", ColumnKind::Badge, Width::Medium),
            column("confidence", "Confidence", ColumnKind::Text, Width::Narrow),
            column("cvss", "CVSS", ColumnKind::Number, Width::Narrow),
            column("open", "Open", ColumnKind::Number, Width::Narrow),
            column("release", "Release", ColumnKind::Mono, Width::Narrow),
            column("cve", "CVE", ColumnKind::Mono, Width::Medium),
            column("package", "Package", ColumnKind::Text, Width::Wide),
            column("version", "Version", ColumnKind::Mono, Width::Narrow),
            column("image", "Image", ColumnKind::Mono, Width::Wide),
            column("lane", "Lane", ColumnKind::Text, Width::Narrow),
            column("opened", "Opened", ColumnKind::Mono, Width::Narrow),
            column("why", "Why", ColumnKind::Text, Width::Fill),
        ],
        rows: tickets
            .map(|ticket| {
                let v = &ticket.verdict;
                let why = match noted(&v.rationale, "Why review:") {
                    "" => v.rationale.lines().next().unwrap_or_default(),
                    why => why,
                };
                Row {
                    id: ticket.key.clone(),
                    cells: vec![
                        Cell::text(&ticket.key).link(format!("{TICKETS}{}", ticket.key)),
                        Cell::text(&ticket.status),
                        Cell::text(ticket.category.label()).tone(ticket.category.tone()),
                        Cell::text(&v.confidence),
                        match &v.proposed_actions.cvss {
                            Some(cvss) => Cell::text(format!("{:.1}", cvss.score))
                                .tone(severity(cvss.score))
                                .bar((cvss.score / 10.0).clamp(0.0, 1.0)),
                            None => Cell::text(""),
                        },
                        Cell::text(format!("{} d", ticket.age_days)).value(ticket.age_days),
                        Cell::text(&v.release.name),
                        Cell::text(v.cve_ids.join(", ")),
                        Cell::text(&v.package.name),
                        Cell::text(&v.package.version),
                        Cell::text(&v.artifact.locator),
                        Cell::text(&ticket.source).link(task_link(&ticket.source)),
                        Cell::date(AS_OF - ticket.age_days * 86_400.0),
                        Cell::text(why),
                    ],
                    detail: Some(detail(ticket)),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crucible_view::{File, Limits, Node, Param, PlotKind, Request, View};

    use crate::report;

    fn file(lane: &str) -> File {
        let inbox = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../inbox");
        File {
            path: "verdicts.json".into(),
            task: format!("triage-{lane}"),
            text: std::fs::read_to_string(inbox.join(format!("{lane}.json"))).unwrap(),
        }
    }

    fn both(params: &[(&str, Param)]) -> View {
        let view = report(Request {
            files: vec![file("go"), file("python")],
            params: params
                .iter()
                .map(|(id, value)| (id.to_string(), value.clone()))
                .collect(),
        })
        .unwrap();
        view.check(&Limits::DEFAULT).unwrap();
        view
    }

    #[test]
    fn the_report_merges_both_lanes() {
        let view = both(&[]);
        assert_eq!(view.title, "Triage report");
        let Node::Stack { children } = &view.body else {
            panic!("the report is a stack");
        };
        let [Node::Stats { items }, Node::Tabs { tabs, .. }] = children.as_slice() else {
            panic!("the report is its stats over its tabs");
        };
        assert_eq!(
            (items[0].label.as_str(), items[0].value.as_str()),
            ("Triaged", "64")
        );
        let all = tabs.iter().find(|tab| tab.label == "All").unwrap();
        assert_eq!(all.count, Some(64));
        let by_category: u64 = tabs.iter().skip(4).filter_map(|tab| tab.count).sum();
        assert_eq!(by_category, 64);
        let Node::Stack { children: summary } = &tabs[0].body else {
            panic!("Summary is a stack");
        };
        let Node::Bars { bars } = &summary[0] else {
            panic!("Summary starts with its bars");
        };
        let labels: Vec<&str> = bars.iter().map(|bar| bar.label.as_str()).collect();
        assert_eq!(labels, ["3.3", "3.4", "3.5"]);
        let charted: f64 = bars
            .iter()
            .flat_map(|bar| &bar.segments)
            .map(|segment| segment.value)
            .sum();
        assert_eq!(charted, 64.0);
        let Node::Stack { children: charts } = &tabs[1].body else {
            panic!("Charts is a stack");
        };
        let [
            Node::Columns { children: side },
            Node::Flow { nodes, links },
            Node::Columns { children: under },
        ] = charts.as_slice()
        else {
            panic!("Charts is two plots side by side over a flow over two more charts");
        };
        let [
            Node::Plot {
                kind: PlotKind::Line,
                series: lines,
                categories,
                x_scale: crucible_view::Scale::Linear,
                ..
            },
            Node::Plot {
                kind: PlotKind::Scatter,
                series: marks,
                ..
            },
        ] = side.as_slice()
        else {
            panic!("the plots are a line plot and a scatter plot");
        };
        assert_eq!(categories, &["3.3", "3.4", "3.5"]);
        assert!(
            lines
                .iter()
                .flat_map(|series| &series.points)
                .all(|point| [0.0, 1.0, 2.0].contains(&point[0])),
            "x indexes a release"
        );
        let [
            Node::Histogram {
                values: ages,
                bins: Some(8),
                ..
            },
            Node::Heatmap {
                x: heat_x,
                y: heat_y,
                values: heat,
                ..
            },
        ] = under.as_slice()
        else {
            panic!("under the flow are a histogram and a heatmap");
        };
        assert_eq!(ages.len(), 64);
        assert_eq!(heat_x, categories);
        assert_eq!(heat_y.len(), 6);
        assert_eq!(heat.iter().flatten().sum::<f64>(), 64.0);
        assert!(
            nodes
                .iter()
                .filter(|node| node.id.starts_with("basis:"))
                .all(|node| node.tone == crucible_view::Tone::Neutral),
            "the bases take the palette's colours"
        );
        let columns = crucible_view::flow_columns(nodes, links).unwrap();
        let reaching = |column: usize| -> f64 {
            links
                .iter()
                .filter(|link| {
                    nodes
                        .iter()
                        .zip(&columns)
                        .any(|(node, at)| node.id == link.to && *at == column)
                })
                .map(|link| link.value)
                .sum()
        };
        assert_eq!(columns.iter().max(), Some(&2));
        assert_eq!((reaching(1), reaching(2)), (64.0, 64.0));
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node.id.starts_with("lane:"))
                .count(),
            2
        );
        assert!(lines.iter().all(|series| series.points.len() == 3));
        let lined: f64 = lines.iter().flat_map(|s| &s.points).map(|p| p[1]).sum();
        assert_eq!(lined, 64.0);
        let marked: usize = marks.iter().map(|series| series.points.len()).sum();
        assert_eq!(marked, 64);
        let Node::Canvas {
            width, commands, ..
        } = &tabs[2].body
        else {
            panic!("Canvas is a canvas");
        };
        assert_eq!(*width, 130.0 + 64.0 * 3.0 + 190.0);
        let filled = commands
            .iter()
            .filter(|command| matches!(command, crucible_view::Draw::FillRect(_)))
            .count();
        assert_eq!(filled, 18, "a cell per category and release");
        let cells: usize = commands
            .iter()
            .filter_map(|command| match command {
                crucible_view::Draw::FillText { text, .. } => text.parse::<usize>().ok(),
                _ => None,
            })
            .sum();
        assert_eq!(cells, 64);
        assert!(commands.iter().any(|command| matches!(
            command,
            crucible_view::Draw::FillText { text, .. } if text == "70%"
        )));
        let hints: Vec<&str> = commands
            .iter()
            .filter_map(|command| match command {
                crucible_view::Draw::Hint { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hints.len(), 19, "a hint per cell and one on the ring");
        assert!(hints.contains(&"45 of 64 closable"), "{hints:?}");
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, crucible_view::Draw::Rotate(_)))
                .count(),
            20,
            "a tick each twentieth of the ring"
        );
        assert!(commands.iter().any(|command| matches!(
            command,
            crucible_view::Draw::FillText { max_width: Some(width), .. } if *width == 116.0
        )));
        let Node::Table { rows, columns, .. } = &all.body else {
            panic!("All is a table");
        };
        assert_eq!(rows.len(), 64);
        assert_eq!(columns[0].id, "ticket");
        let row = rows.iter().find(|row| row.id == "DEMO-101").unwrap();
        assert_eq!(
            row.cells[0].href.as_deref(),
            Some("https://issues.example.com/browse/DEMO-101")
        );
        assert_eq!(
            (row.cells[11].text.as_str(), row.cells[11].href.as_deref()),
            ("triage-go", Some("run:task/triage-go"))
        );
        assert_eq!(
            (columns[4].id.as_str(), columns[4].kind, columns[5].kind),
            (
                "cvss",
                crucible_view::ColumnKind::Number,
                crucible_view::ColumnKind::Number
            )
        );
        assert_eq!(
            (row.cells[4].text.as_str(), row.cells[4].value),
            ("6.1", None)
        );
        assert_eq!(
            (row.cells[5].text.as_str(), row.cells[5].value),
            ("57 d", Some(57.0))
        );
        let detail = row.detail.as_deref().unwrap();
        assert!(
            detail.starts_with("### DEMO-101: sbom_inaccurate\n"),
            "{detail}"
        );
        for part in [
            "The final-artifact package scan for the image reported absent.",
            "| Field | Value |\n|---|---|\n| Package | `google.golang.org/grpc 1.64.0` |",
            "[triage-go](run:task/triage-go)",
            "[its verdicts](run:file/triage-go/verdicts.json)",
        ] {
            assert!(detail.contains(part), "{part} in {detail}");
        }
        let [
            _,
            Node::Columns { children: meters },
            Node::Grid { rows: pivot, .. },
            Node::Section { id, open, body, .. },
        ] = summary.as_slice()
        else {
            panic!("Summary is its bars, meters, pivot and how a ticket is categorised");
        };
        let meters: Vec<(&str, f64, f64, crucible_view::Tone)> = meters
            .iter()
            .map(|meter| match meter {
                Node::Meter {
                    label,
                    value,
                    max,
                    tone,
                } => (label.as_str(), *value, *max, *tone),
                other => panic!("{other:?} is not a meter"),
            })
            .collect();
        assert_eq!(
            meters,
            [
                ("Closable", 45.0, 64.0, crucible_view::Tone::Good),
                ("Affected", 8.0, 64.0, crucible_view::Tone::Bad),
                ("Needs review", 11.0, 64.0, crucible_view::Tone::Warn),
            ]
        );
        let total = pivot.last().unwrap();
        assert_eq!(
            (total[1].text.as_str(), total[1].value),
            ("64", Some(64.0)),
            "the pivot's counts sort as numbers"
        );
        let cvss = rows
            .iter()
            .filter_map(|row| Some((row.cells[4].text.parse::<f64>().ok()?, &row.cells[4])))
            .collect::<Vec<_>>();
        assert!(!cvss.is_empty());
        for (score, cell) in &cvss {
            assert_eq!(cell.bar, Some(score / 10.0), "{score}");
        }
        assert!(
            cvss.iter()
                .any(|(score, cell)| *score >= 9.0 && cell.tone == crucible_view::Tone::Bad)
        );
        assert_eq!(columns[12].id, "opened");
        assert_eq!(
            (row.cells[12].text.as_str(), row.cells[12].value),
            ("2026-09-05", Some(1_793_491_200.0 - 57.0 * 86_400.0))
        );
        assert_eq!((id.as_str(), *open), ("categorised", false));
        let Node::Mermaid { source } = body.as_ref() else {
            panic!("the folded section is a diagram");
        };
        assert!(source.starts_with("flowchart LR\n"), "{source}");
        let [
            crucible_view::Control::Select {
                id, options, value, ..
            },
        ] = view.controls.as_slice()
        else {
            panic!("the report has one select");
        };
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(
            (id.as_str(), value.as_str(), values),
            ("task", "all", vec!["all", "triage-go", "triage-python"])
        );
    }

    #[test]
    fn the_task_select_narrows_the_report_to_one_lane() {
        let triaged = |params: &[(&str, Param)]| {
            let view = both(params);
            let Node::Stack { children } = &view.body else {
                panic!("the report is a stack");
            };
            let Node::Stats { items } = &children[0] else {
                panic!("the report starts with its stats");
            };
            let crucible_view::Control::Select { value, .. } = &view.controls[0] else {
                panic!("the report has one select");
            };
            (items[0].value.clone(), value.clone())
        };
        let select = |value: &str| Param::Select(value.into());
        assert_eq!(
            triaged(&[("task", select("triage-go"))]),
            ("34".into(), "triage-go".into())
        );
        assert_eq!(
            triaged(&[("task", select("triage-python"))]),
            ("30".into(), "triage-python".into())
        );
        assert_eq!(
            triaged(&[("task", select("triage-rust"))]),
            ("64".into(), "all".into())
        );
        assert_eq!(
            triaged(&[("task", Param::Toggle(true))]),
            ("64".into(), "all".into())
        );
    }

    #[test]
    fn a_torn_file_is_the_report_s_error() {
        let why = report(Request {
            params: Default::default(),
            files: vec![File {
                path: "verdicts.json".into(),
                task: "triage-go".into(),
                text: "{".into(),
            }],
        })
        .unwrap_err();
        assert!(why.starts_with("verdicts.json from triage-go:"), "{why}");
    }
}
