//! The report view: every lane's `verdicts.json` as one row per ticket, by disposition.

use std::collections::{BTreeMap, BTreeSet};

use crucible_view::{
    Bar, Cell, Column, ColumnKind, Context2d, Control, FlowLink, FlowNode, LineCap, Node, PlotKind,
    Request,
    Row, Scale, Segment, Series, Stat, Tab, TextAlign, TextBaseline, Tone, View, Width, file_link,
    task_link,
};
use serde::Deserialize;

crucible_view::plugin!(inputs = ["verdicts.json"], view = report);

const TICKETS: &str = "https://issues.example.com/browse/";

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
            .count()
            .to_string()
    };
    let line = |label: &str, tone: Tone, category: Option<Category>| {
        let mut cells = vec![
            Cell::text(label).tone(tone),
            Cell::text(tally(category, None)),
        ];
        cells.extend(
            statuses
                .iter()
                .map(|status| Cell::text(tally(category, Some(status)))),
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
                            Some(cvss) => Cell::text(format!("{:.1}", cvss.score)),
                            None => Cell::text(""),
                        },
                        Cell::text(format!("{} d", ticket.age_days)).value(ticket.age_days),
                        Cell::text(&v.release.name),
                        Cell::text(v.cve_ids.join(", ")),
                        Cell::text(&v.package.name),
                        Cell::text(&v.package.version),
                        Cell::text(&v.artifact.locator),
                        Cell::text(&ticket.source).link(task_link(&ticket.source)),
                        Cell::text(why),
                    ],
                    detail: Some(detail(ticket)),
                }
            })
            .collect(),
    }
}
