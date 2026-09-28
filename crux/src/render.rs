//! Compact plain text, because the reader is usually a model paying by the token.
//!
//! The controller's DTOs are built for a SPA: every issue carries timestamps nobody reads, null
//! columns, and a `LongText` wrapper object around each string. Rendering drops the JSON syntax and
//! the fields with no bearing on a decision, which is roughly a fivefold cut on a real backlog —
//! and, more usefully, turns a list into something greppable.
//!
//! Two rules hold everywhere here:
//! - A column that is empty for a row prints `-`, never a blank. A blank column in a padded table
//!   is indistinguishable from a shifted one.
//! - Nothing is reordered or reworded. Truncation is always marked.

use crate::client::{Credential, Endpoint};
use crate::dto;
use std::fmt::Write as _;

/// How much of a title survives on a list line.
const TITLE_MAX: usize = 68;

/// Char-safe truncation. Byte slicing panics mid-UTF-8, and issue titles carry em dashes and
/// arrows often enough for that to be a real crash rather than a theoretical one.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars().take(max.saturating_sub(1)).collect::<String>()
    )
}

/// The placeholder for a column with nothing in it.
fn or_dash(s: Option<&str>) -> String {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => "-".to_string(),
    }
}

/// Render rows as a left-aligned table sized to its content.
///
/// Content-sized rather than fixed-width: issue keys run from `owner/repo#3` to a 45-char
/// `scenario:<uuid>`, and a fixed width either wastes a column of spaces on every line or clips the
/// one field you were going to copy.
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let mut push = |cells: &[String]| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate().take(cols) {
            if i + 1 == cols {
                // Never pad the last column: it is the free-text one, and trailing spaces are
                // bytes the reader pays for.
                line.push_str(cell);
            } else {
                let pad = widths[i] - cell.chars().count();
                line.push_str(cell);
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };
    push(&header.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    for row in rows {
        push(row);
    }
    out
}

/// One line per issue: key, status, tier, git ref, codegen contract, truncated title.
///
/// Scenario issues have no upstream, so the `UPSTREAM` column is a dash for them — which is exactly
/// the fact you want visible, since it means there is no GitHub thread to go read.
pub fn issues(list: &[dto::Issue], total: usize) -> String {
    if total == 0 {
        return "no issues match\n".to_string();
    }
    let header = if list.len() < total {
        format!("{} of {} issues (raise limit for more)", list.len(), total)
    } else {
        format!("{} issues", list.len())
    };
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|i| {
            vec![
                i.key.clone(),
                i.status.clone(),
                or_dash(i.tier.as_deref()),
                or_dash(i.git_ref.as_deref()),
                or_dash(i.codegen_contract.as_deref()),
                or_dash(i.kind.upstream().as_deref()),
                truncate(i.title.as_deref().unwrap_or(""), TITLE_MAX),
            ]
        })
        .collect();
    format!(
        "{}\n{}",
        header,
        table(
            &[
                "KEY", "STATUS", "TIER", "REF", "CONTRACT", "UPSTREAM", "TITLE"
            ],
            &rows
        )
    )
}

/// The full single-issue view: provenance, the untruncated park reason, the approval gate, and the
/// state transitions that got it here.
pub fn issue(d: &dto::IssueDetail, body_max: usize) -> String {
    let i = &d.issue;
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}\n",
        i.key,
        i.title.as_deref().unwrap_or("(no title)")
    ));
    out.push_str(&format!("status: {}\n", i.status));
    line(&mut out, "tier", i.tier.as_deref());
    out.push_str(&format!("priority: {}\n", i.priority));
    out.push_str(&format!("kind: {}\n", i.kind.label()));
    line(&mut out, "upstream", i.kind.upstream().as_deref());
    line(&mut out, "repo", Some(i.repo.as_str()));
    line(&mut out, "git_ref", i.git_ref.as_deref());
    line(&mut out, "codegen_contract", i.codegen_contract.as_deref());
    line(&mut out, "author", i.author.as_deref());
    if !i.labels.is_empty() {
        out.push_str(&format!("labels: {}\n", i.labels.join(", ")));
    }
    line(&mut out, "updated", Some(i.updated_at.as_str()));
    line(
        &mut out,
        "upstream_updated",
        i.upstream_updated_at.as_deref(),
    );
    line(&mut out, "evidence", i.evidence_url.as_deref());
    line(&mut out, "kept_pr", i.pr_url.as_deref());

    // The park reason is why an operator opened this view at all, so it gets its own block and is
    // never cut here — `GET /api/issues/{key}` sends it whole precisely so this can print it whole.
    if let Some(reason) = &i.parked_reason {
        out.push_str(&format!(
            "\n## parked ({}{})\n{}\n",
            i.parked_by.as_deref().unwrap_or("unknown"),
            if i.stale_closable {
                ", already implemented upstream — close it"
            } else {
                ""
            },
            reason.render()
        ));
    }

    if let Some(s) = &d.scenario {
        out.push_str(&format!(
            "\n## scenario\nadopted_by: {} at {}\nauthoritative: {}\naffected_repos: {}\n",
            s.created_by,
            s.created_at,
            s.authoritative,
            s.affected_repos.join(", ")
        ));
        if !s.body.trim().is_empty() {
            out.push_str(&format!("\n{}\n", truncate(s.body.trim(), body_max)));
        }
    } else if let Some(body) = d.body.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        out.push_str(&format!("\n## body\n{}\n", truncate(body, body_max)));
    }

    if !d.scopes.is_empty() {
        out.push_str("\n## scopes\n");
        for s in &d.scopes {
            out.push_str(&scope(s));
        }
    }

    if !d.events.is_empty() {
        out.push_str("\n## events (oldest first)\n");
        for e in &d.events {
            let actor = e.actor.as_deref().unwrap_or("system");
            let reason = e
                .reason
                .as_ref()
                .map(|r| format!(" — {}", r.render()))
                .unwrap_or_default();
            out.push_str(&format!(
                "{} {} -> {} [{actor}]{reason}\n",
                day(&e.ts),
                blank_as_dash(&e.from),
                blank_as_dash(&e.to)
            ));
        }
    }
    out
}

fn scope(s: &dto::ScopeDetail) -> String {
    let sc = &s.scope;
    let mut out = format!(
        "scope {}  pack={}  check={}{}\n",
        sc.id,
        or_dash(sc.pack_digest.as_deref()),
        or_dash(sc.check_outcome.as_deref()),
        if sc.stale { "  STALE" } else { "" }
    );
    // The approval gate: the PR a human has to act on, and whether anyone has.
    if let Some(pr) = &sc.approval_pr {
        out.push_str(&format!("  approval_pr: {pr}\n"));
        match (&sc.approved_by, &sc.approved_at) {
            (Some(by), Some(at)) => out.push_str(&format!("  approved: {by} at {at}\n")),
            _ => out.push_str("  approved: NOT YET — this is the open approval\n"),
        }
    }
    for r in &s.runs {
        out.push_str(&format!(
            "  run {}  {}  score={}  cost={}  pod={}\n",
            r.run.run_id,
            r.run.status,
            num(r.run.best_score),
            usd(r.run.cost_usd),
            or_dash(r.run.pod.as_deref())
        ));
        for c in &r.candidates {
            out.push_str(&format!(
                "    iter {} lane {} {} score={} {}\n",
                num_i(c.iter),
                num_i(c.lane),
                or_dash(c.kind.as_deref()),
                num(c.score),
                or_dash(c.decision.as_deref())
            ));
            if let Some(pr) = &c.pr_url {
                out.push_str(&format!("      pr: {pr}\n"));
            }
        }
    }
    out
}

/// One line per turn: pod, kind, state, issue, when.
pub fn turns(list: &[dto::Turn]) -> String {
    if list.is_empty() {
        return "no turns match\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|t| {
            vec![
                t.pod_name.clone(),
                t.kind.clone(),
                t.state.clone(),
                or_dash(t.issue_key.as_deref()),
                t.created_at.clone(),
            ]
        })
        .collect();
    format!(
        "{} turns\n{}",
        list.len(),
        table(&["POD", "KIND", "STATE", "ISSUE", "CREATED"], &rows)
    )
}

/// One turn with `result` and `error` intact — the failure chain is the reason to open this.
pub fn turn(t: &dto::Turn) -> String {
    let mut out = format!("{} [{}/{}]\n", t.pod_name, t.kind, t.state);
    line(&mut out, "issue", t.issue_key.as_deref());
    line(&mut out, "cost_tag", Some(t.cost_tag.as_str()));
    line(&mut out, "created", Some(t.created_at.as_str()));
    line(&mut out, "updated", Some(t.updated_at.as_str()));
    line(&mut out, "terminal", t.terminal_at.as_deref());
    if let Some(r) = &t.result {
        out.push_str(&format!("\n## result\n{}\n", r.render()));
    }
    if let Some(e) = &t.error {
        out.push_str(&format!("\n## error\n{}\n", e.render()));
    }
    out
}

/// Whether the running build is the commit the caller expected.
///
/// The two shas arrive at different lengths — the controller reports twelve characters, `git
/// rev-parse` gives forty — so the shorter being a prefix of the longer is the match. A build that
/// cannot name its commit matches nothing: `unknown` is the absence of an answer, and treating it
/// as agreement would report a stale deploy as a fresh one, which is the failure this exists to
/// catch.
pub fn same_commit(running: &str, expected: &str) -> bool {
    let (running, expected) = (running.trim(), expected.trim());
    if running.is_empty() || expected.is_empty() || running == "unknown" {
        return false;
    }
    let n = running.len().min(expected.len());
    running.as_bytes()[..n].eq_ignore_ascii_case(&expected.as_bytes()[..n])
}

/// What build is answering, and whether it is the one the caller expected.
pub fn deployed(endpoint: &str, v: &dto::Version, expected: Option<&str>) -> String {
    let sha = if v.git_sha.is_empty() {
        "unknown"
    } else {
        &v.git_sha
    };
    let mut out = format!("endpoint: {endpoint}\nrunning:  {sha} (v{})\n", v.version);
    if let Some(want) = expected {
        let want = want.trim();
        out.push_str(&format!("expected: {want}\n"));
        out.push_str(if same_commit(sha, want) {
            "verdict:  live — the running build is the commit you expected\n"
        } else if sha == "unknown" {
            "verdict:  UNKNOWN — this build cannot name its commit, so nothing can be concluded\n"
        } else {
            "verdict:  STALE — the commit you expected is not what is running\n"
        });
    }
    out
}

/// A log window's shape: the lines it carries and where to resume. `next` is `None` at the end of
/// what the controller served, which is the only signal a caller has that it is caught up.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LogWindow {
    lines: Vec<String>,
    next: Option<usize>,
    remaining: usize,
}

/// The default line ceiling on one window. A run log is unbounded and a tool result is not, so a
/// window is a handle to a position rather than the log itself.
pub const LOG_WINDOW_LINES: usize = 200;

/// The byte ceiling on one window, applied before the line ceiling: a run that prints one enormous
/// line must not blow the result cap just because it is a single line.
const LOG_WINDOW_BYTES: usize = 16 * 1024;

/// One window of `text` from line `cursor`, bounded by both ceilings. A cursor past the end yields
/// an empty window rather than an error, so polling a finished run is not a failure case.
fn log_window(text: &str, cursor: usize, limit: usize) -> LogWindow {
    let all: Vec<&str> = text.lines().collect();
    let start = cursor.min(all.len());
    let mut lines = Vec::new();
    let mut bytes = 0;
    for line in &all[start..] {
        if lines.len() >= limit || (!lines.is_empty() && bytes + line.len() > LOG_WINDOW_BYTES) {
            break;
        }
        bytes += line.len() + 1;
        lines.push((*line).to_string());
    }
    let end = start + lines.len();
    LogWindow {
        lines,
        next: (end < all.len()).then_some(end),
        remaining: all.len() - end,
    }
}

/// One window of a run's log, headed by where it came from and how to resume.
pub fn run_log(run_id: &str, log: &dto::RunLog, cursor: usize, limit: usize) -> String {
    let Some(text) = log.text.as_deref().filter(|t| !t.is_empty()) else {
        let where_it_is = log
            .location
            .as_deref()
            .unwrap_or("no output recorded for this run");
        return format!("run {run_id} [{}]: {where_it_is}\n", log.dispatch);
    };
    let w = log_window(text, cursor, limit);
    let mut out = format!("run {run_id} [{}]", log.dispatch);
    if log.truncated {
        out.push_str(" (head dropped by the controller's cap)");
    }
    let _ = writeln!(
        out,
        " lines {}..{}, {} remaining",
        cursor,
        cursor + w.lines.len(),
        w.remaining
    );
    for line in &w.lines {
        let _ = writeln!(out, "{line}");
    }
    match w.next {
        Some(next) => {
            let _ = writeln!(out, "-- resume with cursor={next}");
        }
        None => out.push_str("-- end of log\n"),
    }
    out
}

pub fn run_files(run_id: &str, listing: &dto::RunFiles) -> String {
    if listing.files.is_empty() {
        return format!("run {run_id}: no captured files\n");
    }
    let mut rows = Vec::new();
    for f in &listing.files {
        let task = match &f.instance {
            Some(key) => format!("{}[{key}]", f.task),
            None => f.task.clone(),
        };
        rows.push(vec![
            task,
            f.path.clone(),
            f.size_bytes.to_string(),
            f.key.clone(),
        ]);
    }
    let mut out = table(&["TASK", "PATH", "BYTES", "KEY"], &rows);
    let _ = writeln!(out, "-- fetch one with `crux run-file {run_id} <KEY>`");
    out
}

/// The task graph in dependency order, each task carrying its latest result.
///
/// Latest, not every iteration: a loop that ran twelve times prints twelve copies of the same
/// verdict, and the only question anyone asks of this view is "where is it stuck now". The iteration
/// number stays on the line, so a task lagging the rest is still visible.
pub fn graph(run_id: &str, g: &dto::RunGraph) -> String {
    if g.tasks.is_empty() {
        return format!("run {run_id}: plan v{} has no tasks\n", g.plan_version);
    }
    let order = topo_order(&g.tasks);
    let mut rows = Vec::new();
    for name in &order {
        let Some(t) = g.tasks.iter().find(|t| &t.name == name) else {
            continue;
        };
        let latest = g.results.iter().rfind(|r| &r.task == name);
        let deps = if t.depends_on.is_empty() {
            "-".to_string()
        } else {
            let joined = t.depends_on.join(",");
            // `needs=any` is load-bearing: it means the task runs when ONE dependency passes, so a
            // red dependency above it is not necessarily what is blocking it.
            if t.needs == "any" {
                format!("{joined} (any)")
            } else {
                joined
            }
        };
        rows.push(vec![
            latest.map_or("pending".into(), |r| r.status.clone()),
            t.name.clone(),
            t.kind.clone(),
            if t.required {
                "req".into()
            } else {
                "opt".into()
            },
            deps,
            latest.map_or("-".into(), |r| {
                let mut s = format!("i{}", r.iter);
                if let Some(c) = r.cost_usd {
                    s.push_str(&format!(" {}", usd(Some(c))));
                }
                if let Some(secs) = r.secs {
                    s.push_str(&format!(" {secs:.0}s"));
                }
                if !r.note.trim().is_empty() {
                    s.push_str(&format!(" {}", truncate(r.note.trim(), 60)));
                }
                s
            }),
        ]);
    }
    let iters = g.results.iter().map(|r| r.iter).max().unwrap_or(0);
    let mut out = format!(
        "run {run_id}  plan v{}  {} tasks  {} iterations\n{}",
        g.plan_version,
        g.tasks.len(),
        iters + 1,
        table(
            &["STATUS", "TASK", "KIND", "REQ", "DEPENDS ON", "LATEST"],
            &rows
        )
    );
    out.push_str(&outputs_block(g));
    out.push_str(&full_notes(&order, g));
    out
}

/// The notes the table had to cut, in full. A failed run's cause is usually the longest note it
/// has, so a cut one is the case where the table is least worth reading on its own.
fn full_notes(order: &[String], g: &dto::RunGraph) -> String {
    let mut cut = Vec::new();
    for name in order {
        let Some(r) = g.results.iter().rfind(|r| &r.task == name) else {
            continue;
        };
        let note = r.note.trim();
        if !note.is_empty() && truncate(note, 60) != note {
            cut.push(format!("{name}: {note}"));
        }
    }
    if cut.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nnotes in full\n");
    for line in cut {
        out.push_str(&format!("  {}\n", line.replace('\n', "\n    ")));
    }
    out
}

/// The same graph as a mermaid flowchart, for pasting into the docs or a PR.
pub fn graph_mermaid(g: &dto::RunGraph) -> String {
    let mut out = String::from("flowchart TD\n");
    for t in &g.tasks {
        let latest = g.results.iter().rfind(|r| r.task == t.name);
        let status = latest.map_or("pending", |r| r.status.as_str());
        out.push_str(&format!(
            "  {}[\"{}<br/>{} · {status}\"]\n",
            node_id(&t.name),
            t.name,
            t.kind
        ));
    }
    for t in &g.tasks {
        for dep in &t.depends_on {
            out.push_str(&format!("  {} --> {}\n", node_id(dep), node_id(&t.name)));
        }
    }
    out.push_str(&outputs_mermaid(g));
    out
}

/// The declared outputs as terminal nodes, shaped `([…])` so they never read as tasks. An
/// unattached bound hangs off a sink node; a revision with no stored exposure gets an explicit
/// marker node instead of silence. Engine-default bounds are not drawn: they are not work the
/// pack asked for.
fn outputs_mermaid(g: &dto::RunGraph) -> String {
    let Some(outputs) = g.outputs.as_ref() else {
        return "  outputs_undeclared([\"outputs undeclared\"])\n".to_string();
    };
    let mut out = String::new();
    let sinks: Vec<&str> = g
        .tasks
        .iter()
        .filter(|t| !g.tasks.iter().any(|o| o.depends_on.contains(&t.name)))
        .map(|t| t.name.as_str())
        .collect();
    for (i, o) in outputs.iter().filter(|o| o.is_declared()).enumerate() {
        let id = format!("o_{i}");
        let target = o
            .target
            .as_ref()
            .map(|t| format!("<br/>{}", t.render()))
            .unwrap_or_default();
        out.push_str(&format!("  {id}([\"{} x{}{target}\"])\n", o.kind, o.count));
        match o.attached_to.as_deref() {
            Some(task) => out.push_str(&format!("  {} --> {id}\n", node_id(task))),
            None => {
                for sink in &sinks {
                    out.push_str(&format!("  {} --> {id}\n", node_id(sink)));
                }
            }
        }
    }
    out
}

/// One line per declared output bound, drawn off the task that spends it, then the engine-default
/// bounds under their own heading so a pack that declares nothing reads as one. A revision with no
/// stored exposure prints the undeclared marker rather than an empty section.
fn outputs_block(g: &dto::RunGraph) -> String {
    let Some(outputs) = g.outputs.as_ref() else {
        return "\noutputs undeclared (this revision stored no exposure)\n".to_string();
    };
    let (declared, defaults): (Vec<&dto::GraphOutput>, Vec<&dto::GraphOutput>) =
        outputs.iter().partition(|o| o.is_declared());
    let mut out = if declared.is_empty() {
        "\noutputs: none declared\n".to_string()
    } else {
        format!("\n{} declared outputs\n", declared.len())
    };
    for o in &declared {
        out.push_str(&output_line(o));
    }
    if !defaults.is_empty() {
        out.push_str(&format!("{} engine-default bounds\n", defaults.len()));
        for o in &defaults {
            out.push_str(&output_line(o));
        }
    }
    out
}

fn output_line(o: &dto::GraphOutput) -> String {
    let target = o
        .target
        .as_ref()
        .map(|t| format!(" -> {}", t.render()))
        .unwrap_or_default();
    let from = o
        .attached_to
        .as_deref()
        .map(|t| format!("  (from {t})"))
        .unwrap_or_else(|| "  (sink)".to_string());
    format!("  {} x{}{target}{from}\n", o.kind, o.count)
}

/// Mermaid node ids can't carry the punctuation a task name can, so they're sanitized. The label
/// keeps the real name.
fn node_id(name: &str) -> String {
    let id: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("t_{id}")
}

/// Dependency order, with anything cyclic or dangling appended rather than dropped.
///
/// A plan should be a DAG, but this renderer runs *while* someone is debugging a plan, which is
/// exactly when it might not be. Silently omitting a task from a view titled "the task graph" would
/// hide the bug being hunted.
fn topo_order(tasks: &[dto::PlanTask]) -> Vec<String> {
    let mut placed: Vec<String> = Vec::with_capacity(tasks.len());
    let mut remaining: Vec<&dto::PlanTask> = tasks.iter().collect();
    while !remaining.is_empty() {
        let ready: Vec<usize> = remaining
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.depends_on
                    .iter()
                    .all(|d| placed.iter().any(|p| p == d) || !tasks.iter().any(|x| &x.name == d))
            })
            .map(|(i, _)| i)
            .collect();
        if ready.is_empty() {
            // A cycle. Emit the rest in declaration order so the view is still complete.
            placed.extend(remaining.iter().map(|t| t.name.clone()));
            break;
        }
        for i in ready.iter().rev() {
            placed.push(remaining[*i].name.clone());
        }
        for i in ready.iter().rev() {
            remaining.remove(*i);
        }
    }
    placed
}

pub fn contracts(c: &dto::BrokerContracts) -> String {
    if c.names.is_empty() {
        return "no broker contracts configured on this controller (codegen_contract must be omitted on adopt)\n"
            .to_string();
    }
    format!(
        "{} broker contracts\n{}\n",
        c.names.len(),
        c.names.join("\n")
    )
}

/// The approval queue: the open approvals, and the PRs already kept.
pub fn approvals(d: &dto::Approvals) -> String {
    let mut out = String::new();
    if d.awaiting_approval.is_empty() {
        out.push_str("no scopes awaiting approval\n");
    } else {
        let rows: Vec<Vec<String>> = d
            .awaiting_approval
            .iter()
            .map(|a| {
                vec![
                    a.key.clone(),
                    a.scope_id.to_string(),
                    if a.stale { "STALE".into() } else { "-".into() },
                    a.approval_pr.clone(),
                ]
            })
            .collect();
        out.push_str(&format!(
            "{} awaiting approval\n{}",
            d.awaiting_approval.len(),
            table(&["KEY", "SCOPE", "STALE", "APPROVAL PR"], &rows)
        ));
    }
    if !d.kept_prs.is_empty() {
        out.push_str(&format!("\n{} kept PRs\n", d.kept_prs.len()));
        for p in &d.kept_prs {
            out.push_str(&format!("{}  {}\n", p.issue, p.pr_url));
        }
    }
    out
}

/// One approval for one issue: the PR to click and the pack digest under it.
pub fn approval_detail(key: &str, d: &dto::IssueDetail) -> String {
    let open: Vec<&dto::ScopeDetail> = d
        .scopes
        .iter()
        .filter(|s| s.scope.approval_pr.is_some())
        .collect();
    if open.is_empty() {
        return format!(
            "{key}: no scope has an approval PR (status {})\n",
            d.issue.status
        );
    }
    let mut out = String::new();
    for s in open {
        let sc = &s.scope;
        out.push_str(&format!(
            "{key}  scope {}  pack {}\n{}\n",
            sc.id,
            or_dash(sc.pack_digest.as_deref()),
            sc.approval_pr.as_deref().unwrap_or("-")
        ));
        match (&sc.approved_by, &sc.approved_at) {
            (Some(by), Some(at)) => out.push_str(&format!("approved: {by} at {at}\n")),
            _ => out.push_str("approved: NOT YET\n"),
        }
        out.push_str(&exposure_block(sc));
    }
    out
}

/// What approving this scope accepts: the outputs a run may spend, the reach it holds, and the
/// digest an approval binds to.
fn exposure_block(sc: &dto::Scope) -> String {
    let mut out = String::new();
    if sc.exposure.lines.is_empty() {
        out.push_str("exposure: not reported by this controller\n");
        return out;
    }
    out.push_str("exposure");
    match &sc.exposure.digest {
        Some(digest) => out.push_str(&format!(" {digest}")),
        None => out.push_str(" (none stored)"),
    }
    out.push('\n');
    for line in &sc.exposure.lines {
        out.push_str(&format!("  {line}\n"));
    }
    if let Some(bound) = &sc.exposure.approved_digest {
        out.push_str(&format!("approval bound to exposure {bound}\n"));
        if sc.exposure.digest.as_deref() != Some(bound.as_str()) {
            out.push_str("WARNING: the stored exposure has changed since that approval\n");
        }
    }
    out
}

/// A proposed import: what got pinned, what the engine made of it, and the link a human opens to
/// register or discard it.
pub fn pack_import(i: &dto::PackImport, preview_url: &str) -> String {
    let mut out = format!("import {}: {}\n", i.id, blank_as_dash(&i.status));
    out.push_str(&format!(
        "repo: {} @ {}\n",
        i.repo,
        or_dash(i.git_ref.as_deref())
    ));
    line(&mut out, "path", Some(&i.path));
    out.push_str(&format!("rev: {}\n", i.rev));
    out.push_str(&format!(
        "schema: {}\n",
        i.schema_digest
            .as_deref()
            .unwrap_or("none (the pack's source did not compile)")
    ));
    out.push_str(&diagnostic_lines(
        &i.diagnostics
            .iter()
            .map(|d| truncate(d, 300))
            .collect::<Vec<_>>(),
    ));
    line(&mut out, "proposed_by", i.proposed_by.as_deref());
    out.push_str(&format!("preview: {preview_url}\n"));
    out
}

/// A draft version's whole file map. The contents are the point: an agent reads this to pick up
/// whatever a human typed into the studio before writing over it.
pub fn draft_files(id: &str, f: &dto::DraftFiles) -> String {
    let mut out = format!("draft {id} version {}\n", f.version);
    out.push_str(&format!(
        "saved_by: {} at {}\n",
        or_dash(f.saved_by.as_deref()),
        blank_as_dash(&f.saved_at)
    ));
    out.push_str(&diagnostics(&f.diagnostics));
    out.push_str(&format!("files: {}\n", f.files.len()));
    for (path, content) in &f.files {
        out.push_str(&format!("\n--- {path}\n{content}"));
        if !content.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// A pull that landed: where the pack is, and the version a push back must be based on.
pub fn draft_pulled(id: &str, version: i64, dir: &std::path::Path, files: &[String]) -> String {
    let mut out = format!(
        "pulled draft {id} version {version} into {}\n",
        dir.display()
    );
    out.push_str(&format!("files: {}\n", files.len()));
    for path in files {
        out.push_str(&format!("  {path}\n"));
    }
    out.push_str(&format!(
        "push it back with base_version={version}; a newer save refuses this one instead of \
         overwriting it.\n"
    ));
    out
}

/// A save that landed: the new version, its diagnostics, and the studio page a human opens on it.
pub fn draft_created(id: &str, saved: &dto::DraftCompile, studio_url: &str) -> String {
    let mut out = format!("created draft {id} at version {}\n", saved.version);
    out.push_str(&compiled(saved, studio_url));
    out
}

pub fn draft_saved(id: &str, saved: &dto::DraftCompile, studio_url: &str) -> String {
    let mut out = format!("saved draft {id} as version {}\n", saved.version);
    out.push_str(&saved_by(saved));
    out.push_str(&compiled(saved, studio_url));
    out
}

/// What a stored version compiled to, read without launching it.
pub fn draft_preview(id: &str, saved: &dto::DraftCompile, studio_url: &str) -> String {
    let mut out = format!("draft {id} version {} compiled\n", saved.version);
    out.push_str(&saved_by(saved));
    out.push_str(&compiled(saved, studio_url));
    out
}

/// A graduation that landed. The PR is the whole answer: merging it and importing the pack from
/// the same repo/path is what retires the draft.
pub fn draft_graduated(id: &str, ack: &dto::GraduateAck) -> String {
    format!(
        "graduated draft {id}\nPR: {}\nthe draft retires once that merged pack is imported from \
         the same repo and path.\n",
        ack.pr_url
    )
}

/// A publish that landed: the playbook is registered already, and the draft stays the place to
/// edit it.
pub fn draft_published(id: &str, ack: &dto::PublishAck) -> String {
    let mut out = format!(
        "published draft {id} as playbook {} @ {}\n",
        ack.id,
        ack.rev.chars().take(19).collect::<String>()
    );
    if ack.schema_changed {
        out.push_str("the launch form changed.\n");
    }
    if ack.exposure_changed {
        out.push_str("the declared exposure changed.\n");
    }
    out.push_str(&format!(
        "the draft stays live; publishing it again re-pins {}.\n",
        ack.id
    ));
    out
}

fn saved_by(saved: &dto::DraftCompile) -> String {
    format!(
        "saved_by: {} at {}\n",
        or_dash(saved.saved_by.as_deref()),
        blank_as_dash(&saved.saved_at)
    )
}

fn compiled(saved: &dto::DraftCompile, studio_url: &str) -> String {
    let mut out = format!(
        "schema: {}\n",
        saved
            .schema_digest
            .as_deref()
            .unwrap_or("none (this version did not compile)")
    );
    out.push_str(&diagnostics(&saved.diagnostics));
    out.push_str(&format!("studio: {studio_url}\n"));
    out
}

/// A save refused because another editor landed first. The controller's own sentence, then the
/// two calls that resolve it — an agent that reads only the last line still does the right thing.
pub fn stale_base(id: &str, stale: &dto::StaleBase) -> String {
    format!(
        "REFUSED: {}\ncurrent version: {}\nnothing was written. Re-read that version \
         (crucible_draft_files draft_id={id} version={}), merge your edits onto it, and save again \
         with base_version={}.\n",
        stale.error, stale.current_version, stale.current_version, stale.current_version
    )
}

fn diagnostics(list: &[dto::Diagnostic]) -> String {
    diagnostic_lines(&list.iter().map(dto::Diagnostic::render).collect::<Vec<_>>())
}

fn diagnostic_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        return "diagnostics: none\n".to_string();
    }
    let mut out = format!("diagnostics: {}\n", lines.len());
    for l in lines {
        out.push_str(&format!("  {}\n", l.replace('\n', "\n  ")));
    }
    out
}

/// Every mutating command ends here: the ack, with the identity the controller actually attributed
/// the change to. That last part is not decoration — a bearer without identity headers silently
/// books the change to `anonymous`, and this is where that becomes visible.
pub fn ack(action: &str, key: &str, actor: Option<&str>, detail: &str) -> String {
    let mut out = format!("{action} {key}: accepted\n");
    if !detail.is_empty() {
        out.push_str(&format!("{detail}\n"));
    }
    out.push_str(&format!(
        "actor: {}\n",
        actor.unwrap_or("anonymous (the controller recorded no identity for this change)")
    ));
    out
}

fn line(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
        out.push_str(&format!("{key}: {v}\n"));
    }
}

fn owner_state(enabled: bool, signin_required: bool) -> &'static str {
    match (enabled, signin_required) {
        (_, true) => "blocked",
        (true, false) => "enabled",
        (false, false) => "disabled",
    }
}

fn blank_as_dash(s: &str) -> &str {
    if s.trim().is_empty() { "-" } else { s }
}

/// Timestamps trimmed to the day in list contexts: `2026-07-01T09:12:33.123456Z` spends a dozen
/// tokens to say "July 1st".
fn day(ts: &str) -> &str {
    ts.split('T').next().unwrap_or(ts)
}

fn num(v: Option<f64>) -> String {
    v.map_or("-".to_string(), |v| format!("{v:.3}"))
}

fn num_i(v: Option<i64>) -> String {
    v.map_or("-".to_string(), |v| v.to_string())
}

fn usd(v: Option<f64>) -> String {
    v.map_or("-".to_string(), |v| format!("${v:.2}"))
}

/// The registry: what an agent may launch, and the revision it would launch.
pub fn playbook_caps(caps: &dto::PlaybookCaps) -> String {
    format!(
        "max_cost: ${:.2}\nmax_time: {}\na launch asking for more than either is refused\n",
        caps.max_cost, caps.max_time
    )
}

pub fn secrets(list: &[dto::Secret]) -> String {
    if list.is_empty() {
        return "no secrets you own\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.name.clone(),
                s.kind.clone(),
                s.visibility.clone(),
                s.mode.clone(),
                s.owner.clone(),
            ]
        })
        .collect();
    format!(
        "{} secrets\n{}",
        list.len(),
        table(
            &["ID", "NAME", "KIND", "VISIBILITY", "MODE", "OWNER"],
            &rows
        )
    )
}

pub fn secret_bound(b: &dto::SecretBinding) -> String {
    format!(
        "bound {} to {}/{}: accepted\n  as {} -> {} {}\n  binding: {}\nactor: {}\n",
        b.secret_id,
        b.scope_kind,
        b.scope_id,
        b.declared_name,
        b.projection_kind,
        b.projection,
        b.id,
        b.created_by.as_deref().unwrap_or("anonymous")
    )
}

pub fn playbooks(list: &[dto::Playbook]) -> String {
    if list.is_empty() {
        return "no playbooks registered\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|p| {
            vec![
                p.id.clone(),
                p.source.label(),
                p.rev.chars().take(8).collect(),
                or_dash(p.created_by.as_deref()),
                truncate(&p.description, TITLE_MAX),
            ]
        })
        .collect();
    format!(
        "{} playbooks\n{}",
        list.len(),
        table(&["ID", "REPO", "REV", "BY", "DESCRIPTION"], &rows)
    )
}

/// `provider · model`, `provider` alone when the launch took its default model, or a dash when
/// the launch pinned nothing.
fn agent_pin(provider: Option<&str>, model: Option<&str>) -> String {
    match (provider, model) {
        (Some(p), Some(m)) => format!("{p} · {m}"),
        (Some(p), None) => p.to_string(),
        (None, _) => "-".to_string(),
    }
}

/// The launches. `AGENT` is the provider and model the launch pinned, since a pin is the one thing
/// that makes a run land somewhere other than the platform default. `WHY` carries the one thing
/// that stops a launch dead — a secrets refusal, a park, or a schema that moved underneath it —
/// because a status alone does not say which.
pub fn playbook_runs(list: &[dto::PlaybookRun]) -> String {
    if list.is_empty() {
        return "no playbook runs match\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|r| {
            let why = r
                .secrets_refusal
                .as_deref()
                .or(r.parked_reason.as_deref())
                .map(str::to_string)
                .or_else(|| r.schema_drifted.then(|| "schema drifted".to_string()));
            vec![
                r.key.clone(),
                r.playbook.clone(),
                r.status.clone(),
                r.runs.to_string(),
                format!("{}/{}", usd(r.cost_usd), format_args!("${:.2}", r.max_cost)),
                agent_pin(r.agent_provider.as_deref(), r.agent_model.as_deref()),
                or_dash(r.created_by.as_deref()),
                truncate(why.as_deref().unwrap_or("-"), TITLE_MAX),
            ]
        })
        .collect();
    format!(
        "{} playbook runs\n{}",
        list.len(),
        table(
            &[
                "KEY", "PLAYBOOK", "STATUS", "N", "COST/CAP", "AGENT", "BY", "WHY"
            ],
            &rows
        )
    )
}

/// The runs leaderboard.
pub fn runs(list: &[dto::RunRow]) -> String {
    if list.is_empty() {
        return "no runs match\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|r| {
            vec![
                r.run_id.clone(),
                r.status.clone(),
                or_dash(r.repo.as_deref()),
                or_dash(r.issue_key.as_deref()),
                r.best_score
                    .map_or_else(|| "-".to_string(), |s| format!("{s:.1}")),
                usd(r.cost_usd),
                or_dash(r.cluster.as_deref()),
                or_dash(r.created.as_deref()),
            ]
        })
        .collect();
    format!(
        "{} runs\n{}",
        list.len(),
        table(
            &[
                "RUN", "STATUS", "REPO", "ISSUE", "BEST", "COST", "CLUSTER", "CREATED"
            ],
            &rows
        )
    )
}

/// The recurrences. A schedule whose owner has to sign in again is listed as blocked rather than
/// enabled: it is enabled and still will not fire, which is the confusing case worth naming.
pub fn schedules(list: &[dto::Schedule]) -> String {
    if list.is_empty() {
        return "no schedules\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            let state = owner_state(s.enabled, s.owner_signin_required).to_string();
            vec![
                s.id.clone(),
                s.playbook.clone(),
                s.cron_expr.clone(),
                state,
                or_dash(s.next_due_at.as_deref()),
                or_dash(s.last_fired_at.as_deref()),
                s.consecutive_failures.to_string(),
                or_dash(s.owner_principal.as_deref()),
            ]
        })
        .collect();
    format!(
        "{} schedules\n{}",
        list.len(),
        table(
            &[
                "ID", "PLAYBOOK", "CRON", "STATE", "NEXT", "LAST", "FAILS", "OWNER"
            ],
            &rows
        )
    )
}

/// The tracker watches. Same state vocabulary as schedules: `blocked` is enabled and still will
/// not launch, because the owner has to sign in again.
pub fn watches(list: &[dto::Watch]) -> String {
    if list.is_empty() {
        return "no watches\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|w| {
            let state = owner_state(w.enabled, w.owner_signin_required).to_string();
            vec![
                w.id.clone(),
                w.playbook.clone(),
                w.tracker.clone(),
                w.query.clone(),
                state,
                or_dash(w.last_swept_at.as_deref()),
                or_dash(w.last_launched_at.as_deref()),
                w.consecutive_failures.to_string(),
                or_dash(w.owner_principal.as_deref()),
            ]
        })
        .collect();
    format!(
        "{} watches\n{}",
        list.len(),
        table(
            &[
                "ID", "PLAYBOOK", "TRACKER", "QUERY", "STATE", "SWEPT", "LAUNCHED", "FAILS",
                "OWNER"
            ],
            &rows
        )
    )
}

/// What a launch became, and where to watch it.
pub fn launched(ack: &dto::LaunchAck, run_url: &str) -> String {
    let mut out = format!(
        "launched {} as {}\n  cap {} / {}\n",
        ack.playbook,
        ack.key,
        format_args!("${:.2}", ack.max_cost),
        ack.max_time
    );
    if let Some(actor) = &ack.actor {
        out.push_str(&format!("  as {actor}\n"));
    }
    if let Some(target) = &ack.dispatch_target {
        out.push_str(&format!("  dispatching to {target}\n"));
    }
    if let Some(provider) = &ack.provider {
        match &ack.model {
            Some(model) => out.push_str(&format!("  agent {provider} · {model}\n")),
            None => out.push_str(&format!("  agent {provider} (its default model)\n")),
        }
    }
    if let Some(exposure) = ack.exposure.as_ref().filter(|e| !e.lines.is_empty()) {
        out.push_str(&format!(
            "  exposure {}\n",
            exposure.digest.as_deref().unwrap_or("(none stored)")
        ));
        for line in &exposure.lines {
            out.push_str(&format!("    {line}\n"));
        }
    }
    out.push_str(&format!("  {run_url}\n"));
    out
}

/// Where the requests went, what they carried, and who the controller says that makes you.
///
/// The three are printed together because the interesting case is when they disagree: an
/// anonymous credential against a named identity means the endpoint is not the one that was
/// configured, or the credential is not the one that was resolved.
pub fn whoami(endpoint: &Endpoint, credential: &Credential, w: &dto::Whoami) -> String {
    let mut out = format!("endpoint: {}\n", endpoint_line(endpoint));
    out.push_str(&format!("credential: {}\n", credential_line(credential)));
    out.push_str(&format!(
        "user: {}\nrole: {}\nadmin: {}\n",
        w.user.as_deref().unwrap_or("anonymous"),
        w.role,
        w.admin
    ));
    out.push_str(&format!(
        "groups: {}\n",
        or_dash(Some(&w.groups.join(", ")))
    ));
    out.push_str(&format!(
        "controller auth mode: {}\n",
        or_dash(
            w.mode
                .as_ref()
                .map(dto::ControllerAuthMode::to_string)
                .as_deref()
        )
    ));
    if w.downgraded {
        out.push_str(
            "\nNOTE: the controller could not refresh this session's groups, so the role above \
             may be lower than the one the issuer would grant.\n",
        );
    }
    out
}

fn endpoint_line(endpoint: &Endpoint) -> String {
    match endpoint {
        Endpoint::Configured { url } if url == crate::config::DEFAULT_URL => {
            format!("{url} (the default: a controller on this machine)")
        }
        Endpoint::Configured { url } => format!("{url} (configured CONTROLLER_URL)"),
        Endpoint::InProcess { links } => {
            format!("this controller, in process (links are built against {links})")
        }
    }
}

fn credential_line(credential: &Credential) -> String {
    match credential {
        Credential::Wire(auth) => auth.mode().to_string(),
        Credential::EdgeApiKey => {
            "api key, checked by the MCP surface before the request existed".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Auth;

    fn log_of(text: Option<&str>, location: Option<&str>) -> dto::RunLog {
        dto::RunLog {
            dispatch: "pod".to_string(),
            text: text.map(str::to_string),
            truncated: false,
            location: location.map(str::to_string),
        }
    }

    /// The controller reports twelve characters and `git rev-parse` gives forty, so the comparison
    /// is by prefix in whichever direction is shorter.
    #[test]
    fn a_short_sha_matches_the_long_one_it_prefixes() {
        let long = "8affb8ed7cdc764cf711e782ddb7ad89cbfbe1cd";
        assert!(same_commit("8affb8ed7cdc", long));
        assert!(same_commit(long, "8affb8ed7cdc"));
        assert!(
            same_commit("8AFFB8ED7CDC", long),
            "case is not the difference"
        );
        assert!(!same_commit("0f2d4621ba05", long));
    }

    /// A build that cannot name its commit matches nothing. Treating `unknown` as agreement would
    /// report a stale deploy as a live one, which is the failure this exists to catch.
    #[test]
    fn an_unknown_build_matches_nothing() {
        assert!(!same_commit("unknown", "8affb8ed7cdc"));
        assert!(!same_commit("", "8affb8ed7cdc"));
        assert!(!same_commit("8affb8ed7cdc", ""));
    }

    /// The verdict line is the whole point, so it says which of the three cases happened.
    #[test]
    fn the_verdict_names_live_stale_and_unknown() {
        let v = |sha: &str| dto::Version {
            git_sha: sha.to_string(),
            version: "0.2.0".to_string(),
        };
        let want = "8affb8ed7cdc764cf711e782ddb7ad89cbfbe1cd";
        assert!(deployed("https://c", &v("8affb8ed7cdc"), Some(want)).contains("live"));
        assert!(deployed("https://c", &v("0f2d4621ba05"), Some(want)).contains("STALE"));
        assert!(deployed("https://c", &v("unknown"), Some(want)).contains("UNKNOWN"));
        assert!(
            !deployed("https://c", &v("8affb8ed7cdc"), None).contains("verdict"),
            "with nothing to compare against there is no verdict to give"
        );
    }

    /// A window ends where the caller can resume, and the last one says so instead of handing back
    /// a cursor that would read nothing.
    #[test]
    fn a_log_window_reports_where_to_resume_and_where_it_ends() {
        let text = (0..5)
            .map(|i| format!("line {i}\n"))
            .collect::<Vec<_>>()
            .concat();

        let first = log_window(&text, 0, 2);
        assert_eq!(first.lines, vec!["line 0", "line 1"]);
        assert_eq!(first.next, Some(2));
        assert_eq!(first.remaining, 3);

        let last = log_window(&text, 4, 2);
        assert_eq!(last.lines, vec!["line 4"]);
        assert_eq!(last.next, None, "the final window resumes nowhere");
        assert_eq!(last.remaining, 0);
    }

    /// Polling past the end of a finished run is an empty window, not an error.
    #[test]
    fn a_cursor_past_the_end_reads_empty() {
        let w = log_window("only\n", 99, 10);
        assert!(w.lines.is_empty());
        assert_eq!(w.next, None);
        assert_eq!(w.remaining, 0);
    }

    /// One enormous line still comes back — the byte ceiling only stops a window growing past it,
    /// so a log that never breaks a line cannot stall on an empty window forever.
    #[test]
    fn one_oversize_line_is_served_rather_than_stalling() {
        let text = format!("{}\nnext\n", "x".repeat(LOG_WINDOW_BYTES * 2));
        let w = log_window(&text, 0, LOG_WINDOW_LINES);
        assert_eq!(w.lines.len(), 1);
        assert_eq!(w.next, Some(1));
    }

    /// A run with no output says where to look instead of printing an empty window.
    #[test]
    fn no_output_names_where_it_lives() {
        let out = run_log(
            "run-1",
            &log_of(
                None,
                Some("pod loop-abc in namespace crucible-system on cluster wharf"),
            ),
            0,
            10,
        );
        assert!(out.contains("on cluster wharf"), "{out}");
        assert!(!out.contains("resume with cursor"), "{out}");
    }

    /// Every whoami test reads the same controller answer, serialized the way
    /// `GET /api/whoami` serializes it, so the client-side half is the only thing that varies.
    fn controller_says(json: &str) -> dto::Whoami {
        serde_json::from_str(json).expect("a whoami body")
    }

    fn wynn() -> dto::Whoami {
        controller_says(
            r#"{"user":"wynn","admin":true,"role":"admin","groups":["/groups/crucible"],
                "mode":"proxy","downgraded":false}"#,
        )
    }

    #[test]
    fn whoami_names_the_configured_url_when_it_answered() {
        let text = whoami(
            &Endpoint::Configured {
                url: "https://crucible.example.com".into(),
            },
            &Credential::Wire(Auth::ApiKey {
                token: "crk_a_b".into(),
            }),
            &wynn(),
        );
        assert!(
            text.contains("endpoint: https://crucible.example.com (configured CONTROLLER_URL)"),
            "{text}"
        );
        assert!(text.contains("credential: api key\n"), "{text}");
    }

    #[test]
    fn whoami_says_when_the_url_is_the_local_default() {
        let text = whoami(
            &Endpoint::Configured {
                url: crate::config::DEFAULT_URL.into(),
            },
            &Credential::Wire(Auth::None),
            &wynn(),
        );
        assert!(
            text.contains("(the default: a controller on this machine)"),
            "{text}"
        );
    }

    /// The bug this fixes: the in-process wire carries no per-request credential, and reporting
    /// that as `none` beside a named admin is the contradiction an operator cannot act on.
    #[test]
    fn whoami_on_the_in_process_wire_reports_the_api_key_not_none() {
        let text = whoami(
            &Endpoint::InProcess {
                links: "https://crucible.example.com".into(),
            },
            &Credential::EdgeApiKey,
            &wynn(),
        );
        assert!(
            text.contains("endpoint: this controller, in process"),
            "{text}"
        );
        assert!(
            text.contains("credential: api key, checked by the MCP surface"),
            "{text}"
        );
        assert!(!text.contains("credential: none"), "{text}");
    }

    /// Each supported mode prints its own credential beside the identity the controller returned,
    /// so the two can be read against each other rather than one standing in for the other.
    #[test]
    fn every_auth_mode_prints_its_credential_beside_the_controllers_identity() {
        let endpoint = Endpoint::Configured {
            url: "https://crucible.example.com".into(),
        };
        let modes = [
            (
                Auth::ApiKey {
                    token: "crk_a_b".into(),
                },
                "api key",
            ),
            (Auth::Bearer { token: "t".into() }, "bearer (anonymous)"),
            (Auth::None, "none"),
        ];
        for (auth, label) in modes {
            let text = whoami(&endpoint, &Credential::Wire(auth), &wynn());
            assert!(text.contains(&format!("credential: {label}\n")), "{text}");
            assert!(text.contains("user: wynn\n"), "{text}");
            assert!(text.contains("role: admin\n"), "{text}");
            assert!(text.contains("admin: true\n"), "{text}");
            assert!(text.contains("groups: /groups/crucible\n"), "{text}");
            assert!(text.contains("controller auth mode: proxy\n"), "{text}");
        }
    }

    /// An older controller sends none of the three added fields; the answer still renders, with
    /// the columns it could not fill marked rather than guessed.
    #[test]
    fn whoami_renders_a_controller_that_reports_no_mode_or_groups() {
        let text = whoami(
            &Endpoint::Configured {
                url: "https://crucible.example.com".into(),
            },
            &Credential::Wire(Auth::Bearer { token: "t".into() }),
            &controller_says(r#"{"user":null,"admin":false,"role":"viewer"}"#),
        );
        assert!(text.contains("user: anonymous\n"), "{text}");
        assert!(text.contains("groups: -\n"), "{text}");
        assert!(text.contains("controller auth mode: -\n"), "{text}");
        assert!(!text.contains("NOTE:"), "{text}");
    }

    /// A refused group refresh is why a role can read lower than the one the issuer would grant,
    /// which is the same 403 this command gets run about.
    #[test]
    fn whoami_says_when_the_controller_downgraded_the_session() {
        let text = whoami(
            &Endpoint::Configured {
                url: "https://crucible.example.com".into(),
            },
            &Credential::Wire(Auth::ApiKey {
                token: "crk_a_b".into(),
            }),
            &controller_says(
                r#"{"user":"wynn","admin":false,"role":"viewer","groups":[],
                    "mode":"native","downgraded":true}"#,
            ),
        );
        assert!(text.contains("controller auth mode: native\n"), "{text}");
        assert!(
            text.contains("could not refresh this session's groups"),
            "{text}"
        );
    }

    /// A mode this build has never heard of is carried through verbatim instead of failing the
    /// one request an operator makes when nothing else works.
    #[test]
    fn an_unknown_controller_mode_survives_deserialization() {
        let w = controller_says(
            r#"{"user":"wynn","admin":false,"role":"viewer","mode":"mesh","downgraded":false}"#,
        );
        assert_eq!(w.mode, Some(dto::ControllerAuthMode::Other("mesh".into())));
    }

    /// Wire-shaped fixtures, like the issue list above: parsed from what the controller actually
    /// serializes rather than built from a struct literal.
    #[test]
    fn playbook_runs_names_what_stopped_a_launch() {
        let list: Vec<dto::PlaybookRun> = serde_json::from_str(
            r#"[
              {"key":"pb-1","playbook":"report","status":"parked","runs":2,"cost_usd":1.5,
               "max_cost":10.0,"created_by":"alice","created_at":"2026-08-01T00:00:00Z",
               "schedule":null,"parked_reason":"budget","secrets_refusal":null,
               "schema_digest":"d","schema_drifted":false,"advance_dedupe":false,
               "origin":"manual","params":{}},
              {"key":"pb-2","playbook":"report","status":"new","runs":0,"cost_usd":null,
               "max_cost":5.0,"created_by":null,"created_at":"2026-08-02T00:00:00Z",
               "schedule":null,"parked_reason":null,"secrets_refusal":"no crucible-slack",
               "schema_digest":"d","schema_drifted":true,"advance_dedupe":false,
               "origin":"manual","params":{}}
            ]"#,
        )
        .expect("wire shape");
        let out = playbook_runs(&list);
        assert!(out.contains("2 playbook runs"), "{out}");
        // A secrets refusal outranks the drift flag: it is the thing actually blocking the launch.
        assert!(out.contains("no crucible-slack"), "{out}");
        assert!(out.contains("budget"), "{out}");
        assert!(out.contains("$1.50/$10.00"), "{out}");
        // An unspent launch shows a dash rather than $0.00, which would read as "ran, cost nothing".
        assert!(out.contains("-/$5.00"), "{out}");
    }

    #[test]
    fn a_drifted_schema_is_the_reason_when_nothing_else_is() {
        let list: Vec<dto::PlaybookRun> = serde_json::from_str(
            r#"[{"key":"pb-3","playbook":"report","status":"new","runs":0,"cost_usd":null,
                 "max_cost":5.0,"created_by":null,"created_at":"2026-08-02T00:00:00Z",
                 "schedule":null,"parked_reason":null,"secrets_refusal":null,
                 "schema_digest":"d","schema_drifted":true,"advance_dedupe":false,
                 "origin":"manual","params":{}}]"#,
        )
        .expect("wire shape");
        assert!(playbook_runs(&list).contains("schema drifted"));
    }

    /// The pin is the one thing that moves a run off the platform default, so the table names
    /// it: the pair when both were pinned, the provider alone when it took its default model,
    /// and a dash for a launch that pinned nothing.
    #[test]
    fn playbook_runs_show_the_agent_a_launch_pinned() {
        let list: Vec<dto::PlaybookRun> = serde_json::from_str(
            r#"[
              {"key":"pb-1","playbook":"report","status":"new","runs":0,"cost_usd":null,
               "max_cost":5.0,"created_by":"alice","created_at":"2026-08-02T00:00:00Z",
               "schedule":null,"parked_reason":null,"secrets_refusal":null,
               "schema_digest":"d","schema_drifted":false,"advance_dedupe":false,
               "origin":"manual","params":{},"agent_provider":"plat-openai",
               "agent_model":"gpt-5.6-luna"},
              {"key":"pb-2","playbook":"report","status":"new","runs":0,"cost_usd":null,
               "max_cost":5.0,"created_by":"alice","created_at":"2026-08-02T00:00:00Z",
               "schedule":null,"parked_reason":null,"secrets_refusal":null,
               "schema_digest":"d","schema_drifted":false,"advance_dedupe":false,
               "origin":"manual","params":{},"agent_provider":"vertex","agent_model":null},
              {"key":"pb-3","playbook":"report","status":"new","runs":0,"cost_usd":null,
               "max_cost":5.0,"created_by":"bob","created_at":"2026-08-02T00:00:00Z",
               "schedule":null,"parked_reason":null,"secrets_refusal":null,
               "schema_digest":"d","schema_drifted":false,"advance_dedupe":false,
               "origin":"manual","params":{}}
            ]"#,
        )
        .expect("wire shape");
        let out = playbook_runs(&list);
        assert!(out.contains("AGENT"), "{out}");
        assert!(out.contains("plat-openai · gpt-5.6-luna"), "{out}");
        let rows: Vec<&str> = out.lines().collect();
        let bare = rows.iter().find(|l| l.starts_with("pb-2")).expect("row");
        assert!(bare.contains("vertex") && !bare.contains("·"), "{bare}");
        let unpinned = rows.iter().find(|l| l.starts_with("pb-3")).expect("row");
        assert!(
            unpinned.contains("  -  "),
            "a launch that pinned nothing: {unpinned}"
        );
    }

    /// An enabled schedule whose owner must sign in again still will not fire. Reporting it as
    /// `enabled` is the confusing answer; `blocked` is the useful one.
    #[test]
    fn a_schedule_blocked_on_its_owner_does_not_read_as_enabled() {
        let list: Vec<dto::Schedule> = serde_json::from_str(
            r#"[
              {"id":"s1","playbook":"report","cron_expr":"0 9 * * *","enabled":true,
               "next_due_at":"2026-08-29T09:00:00Z","last_fired_at":null,
               "consecutive_failures":0,"owner_principal":"user:alice",
               "owner_signin_required":true,"advance_dedupe":false,"max_cost":1.0,
               "max_time":"30m","params":{},"created_at":"2026-08-01T00:00:00Z"},
              {"id":"s2","playbook":"report","cron_expr":"0 9 * * *","enabled":false,
               "next_due_at":null,"last_fired_at":null,"consecutive_failures":3,
               "owner_principal":null,"owner_signin_required":false,"advance_dedupe":false,
               "max_cost":1.0,"max_time":"30m","params":{},"created_at":"2026-08-01T00:00:00Z"}
            ]"#,
        )
        .expect("wire shape");
        let out = schedules(&list);
        assert!(out.contains("blocked"), "{out}");
        assert!(out.contains("disabled"), "{out}");
        assert!(
            !out.contains("enabled"),
            "an owner-blocked schedule must not read as enabled: {out}"
        );
    }

    #[test]
    fn runs_carry_the_cluster_they_were_dispatched_to() {
        let list: Vec<dto::RunRow> = serde_json::from_str(
            r#"[{"run_id":"run-1","status":"finished","repo":"owner/repo","issue_key":"owner/repo#1",
                 "best_score":1114.1,"cost_usd":2.0,"created":"2026-08-01T00:00:00Z",
                 "pr_url":null,"score_series":[],"cluster":"wharf"}]"#,
        )
        .expect("wire shape");
        let out = runs(&list);
        assert!(out.contains("wharf"), "{out}");
        assert!(out.contains("1114.1"), "{out}");
    }

    /// A controller that predates recorded dispatch location sends no `cluster`; the column has to
    /// degrade to a dash rather than fail the parse mid-incident.
    #[test]
    fn a_run_without_a_recorded_cluster_still_renders() {
        let list: Vec<dto::RunRow> = serde_json::from_str(
            r#"[{"run_id":"run-2","status":"running","repo":null,"issue_key":null,
                 "best_score":null,"cost_usd":null,"created":null,"pr_url":null,
                 "score_series":[]}]"#,
        )
        .expect("wire shape");
        assert!(runs(&list).contains("run-2"));
    }

    #[test]
    fn a_launch_says_what_it_became_and_where_to_watch_it() {
        let ack: dto::LaunchAck = serde_json::from_str(
            r#"{"key":"pb-9","playbook":"report","max_cost":10.0,"max_time":"30m",
                "actor":"alice","dispatch_target":"wharf","provider":"plat-openai",
                "model":"gpt-5.6-luna"}"#,
        )
        .expect("wire shape");
        let out = launched(&ack, "https://crucible.example.com/playbook-runs/pb-9");
        assert!(out.contains("launched report as pb-9"), "{out}");
        assert!(out.contains("$10.00 / 30m"), "{out}");
        assert!(out.contains("dispatching to wharf"), "{out}");
        assert!(out.contains("agent plat-openai · gpt-5.6-luna"), "{out}");
        assert!(
            out.contains("https://crucible.example.com/playbook-runs/pb-9"),
            "{out}"
        );
        assert!(
            !out.contains("exposure"),
            "a registered launch has no per-launch disclosure to print: {out}"
        );
    }

    /// A draft one-shot recomputes its disclosure from the exact launched content, and the
    /// acknowledgement carries it.
    #[test]
    fn a_draft_one_shot_launch_prints_the_exposure_it_recorded() {
        let ack: dto::LaunchAck = serde_json::from_str(
            r#"{"key":"pb-9","playbook":"report","max_cost":10.0,"max_time":"30m",
                "actor":"alice","dispatch_target":"wharf",
                "exposure":{"digest":"sha256:expo",
                            "lines":["outputs:","  draft-pr x1 -> owner/repo"]}}"#,
        )
        .expect("wire shape");
        let out = launched(&ack, "https://crucible.example.com/playbook-runs/pb-9");
        assert!(out.contains("exposure sha256:expo"), "{out}");
        assert!(out.contains("    outputs:"), "{out}");
        assert!(out.contains("      draft-pr x1 -> owner/repo"), "{out}");
    }

    /// A draft whose pinned engine has no exposure extraction prints the undeclared marker.
    #[test]
    fn a_draft_launch_under_a_legacy_engine_prints_the_undeclared_marker() {
        let ack: dto::LaunchAck = serde_json::from_str(
            r#"{"key":"pb-9","playbook":"report","max_cost":1.0,"max_time":"5m",
                "exposure":{"lines":["outputs undeclared (registered by an engine with no exposure extraction)"]}}"#,
        )
        .expect("wire shape");
        let out = launched(&ack, "https://crucible.example.com/playbook-runs/pb-9");
        assert!(out.contains("exposure (none stored)"), "{out}");
        assert!(out.contains("outputs undeclared"), "{out}");
    }

    #[test]
    fn empty_lists_say_so_rather_than_printing_a_bare_header() {
        assert_eq!(playbooks(&[]), "no playbooks registered\n");
        assert_eq!(playbook_runs(&[]), "no playbook runs match\n");
        assert_eq!(runs(&[]), "no runs match\n");
        assert_eq!(schedules(&[]), "no schedules\n");
    }

    /// Fixtures are parsed from JSON in the exact shape the controller serializes (snake_case, no
    /// renames, `LongText` as an object) — a struct literal would test the renderer against my
    /// memory of the wire format instead of against the wire format.
    fn issue_list() -> Vec<dto::Issue> {
        serde_json::from_str(
            r#"[
              {"key":"owner/repo#12","repo":"owner/repo",
               "kind":{"type":"github","owner":"owner","repo":"repo","number":12},
               "tier":"T1","status":"awaiting_approval","priority":40,"evidence_url":null,
               "parked_reason":null,"parked_by":null,"updated_at":"2026-07-01T11:00:00Z",
               "upstream_updated_at":"2026-06-30T09:00:00Z","title":"Cache-aware request routing",
               "author":"someone","labels":["routing"],"pr_url":null,"stale_closable":false,
               "git_ref":null,"codegen_contract":null},
              {"key":"scenario:0192f4a1-8c3e-7000-9abc-1234567890ab","repo":"owner/other",
               "kind":{"type":"scenario","id":"0192f4a1-8c3e-7000-9abc-1234567890ab"},
               "tier":"T1","status":"scoping","priority":50,"evidence_url":null,
               "parked_reason":{"text":"the ranker found the ask already implemented and the","truncated":true},
               "parked_by":"machine","updated_at":"2026-07-02T08:00:00Z","upstream_updated_at":null,
               "title":"Adopt the EPP calibration scenario and drive it to a measured candidate set",
               "author":null,"labels":[],"pr_url":null,"stale_closable":true,
               "git_ref":"nv_dev","codegen_contract":"epp-measure"}
            ]"#,
        )
        .expect("the issue list fixture parses")
    }

    #[test]
    fn issue_lines_carry_every_required_column() {
        let out = issues(&issue_list(), issue_list().len());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "2 issues");
        assert!(lines[1].starts_with("KEY"), "{}", lines[1]);
        assert!(
            lines[2].contains("owner/repo#12")
                && lines[2].contains("awaiting_approval")
                && lines[2].contains("T1")
                && lines[2].contains("owner/repo#12"),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].contains("nv_dev") && lines[3].contains("epp-measure"),
            "the git ref and contract are the point of adopting with them: {}",
            lines[3]
        );
    }

    /// A scenario has no upstream issue. That must render as a dash, not as an absent column that
    /// shifts every field after it.
    #[test]
    fn a_scenario_issue_renders_with_no_upstream() {
        let out = issues(&issue_list(), issue_list().len());
        let scenario = out
            .lines()
            .find(|l| l.starts_with("scenario:"))
            .expect("the scenario row");
        let cols: Vec<&str> = scenario.split_whitespace().collect();
        assert_eq!(cols[1], "scoping");
        assert_eq!(cols[2], "T1");
        assert_eq!(cols[3], "nv_dev");
        assert_eq!(cols[4], "epp-measure");
        assert_eq!(cols[5], "-", "no upstream for a scenario: {scenario}");
    }

    #[test]
    fn a_github_issue_with_no_ref_or_contract_shows_dashes() {
        let out = issues(&issue_list(), issue_list().len());
        let gh = out
            .lines()
            .find(|l| l.starts_with("owner/repo#12"))
            .expect("the github row");
        let cols: Vec<&str> = gh.split_whitespace().collect();
        assert_eq!((cols[3], cols[4]), ("-", "-"), "{gh}");
    }

    #[test]
    fn long_titles_are_cut_and_marked() {
        let out = issues(&issue_list(), issue_list().len());
        assert!(out.contains('…'), "a 74-char title must be cut: {out}");
        assert!(
            !out.contains("drive it to a measured candidate set"),
            "the tail is gone, not wrapped: {out}"
        );
    }

    #[test]
    fn an_empty_list_says_so_instead_of_printing_a_bare_header() {
        assert_eq!(issues(&[], 0), "no issues match\n");
    }

    #[test]
    fn a_capped_list_names_the_cap_instead_of_counting_the_rows_it_printed() {
        let list = issue_list();
        let out = issues(&list[..1], list.len());
        assert_eq!(
            out.lines().next().expect("a header"),
            "1 of 2 issues (raise limit for more)"
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        // The crash this prevents: slicing bytes inside a multi-byte char.
        assert_eq!(truncate("→→→→", 2), "→…");
    }

    fn detail() -> dto::IssueDetail {
        serde_json::from_str(
            r#"{
              "issue":{"key":"owner/repo#12","repo":"owner/repo",
                "kind":{"type":"github","owner":"owner","repo":"repo","number":12},
                "tier":"T2","status":"parked","priority":10,"evidence_url":"https://ev/1",
                "parked_reason":{"text":"scope check failed: the pack named no measurable target","truncated":false},
                "parked_by":"machine","updated_at":"2026-07-01T11:00:00Z","upstream_updated_at":null,
                "title":"Cache-aware routing","author":"someone","labels":["routing","rfe"],
                "pr_url":null,"stale_closable":false,"git_ref":"main","codegen_contract":null},
              "body":"the long upstream body",
              "comments":[],
              "scopes":[{"scope":{"id":7,"pack_digest":"sha256:abcd","check_outcome":"fail",
                "stale":false,"approval_pr":"https://github.com/owner/repo/pull/9",
                "approved_by":null,"approved_at":null},
                "runs":[{"run":{"run_id":"0192run","scope":7,"issue_key":"owner/repo#12",
                  "repo":"owner/repo","identity_digest":null,"status":"failed","pod":"turn-abc",
                  "session_uri":null,"best_score":0.42,"cost_usd":1.5},
                  "candidates":[{"run_id":"0192run","kind":"patch","lane":0,"iter":2,"score":0.42,
                    "decision":"drop","worktree":null,"sandbox":null,"pr_url":null,"branch":null}]}]}],
              "events":[{"ts":"2026-07-01T10:00:00Z","key":"owner/repo#12","from":"new","to":"scoping",
                "reason":null,"evidence":null,"actor":"system"},
                {"ts":"2026-07-01T11:00:00Z","key":"owner/repo#12","from":"scoping","to":"parked",
                "reason":{"text":"scope check failed","truncated":false},"evidence":null,"actor":"wren"}],
              "scenario":null
            }"#,
        )
        .expect("the issue detail fixture parses")
    }

    #[test]
    fn detail_prints_the_park_reason_whole() {
        let out = issue(&detail(), 4000);
        assert!(
            out.contains(
                "## parked (machine)\nscope check failed: the pack named no measurable target\n"
            ),
            "the detail endpoint sends it untruncated, so print it untruncated: {out}"
        );
        assert!(!out.contains("[truncated"), "{out}");
    }

    /// The approval PR and whether anyone has acted on it is the single most-asked question of this
    /// view; it must be unmissable.
    #[test]
    fn detail_names_the_open_approval() {
        let out = issue(&detail(), 4000);
        assert!(
            out.contains("approval_pr: https://github.com/owner/repo/pull/9"),
            "{out}"
        );
        assert!(
            out.contains("approved: NOT YET — this is the open approval"),
            "{out}"
        );
        assert!(out.contains("pack=sha256:abcd"), "{out}");
    }

    #[test]
    fn detail_carries_provenance_and_the_event_trail() {
        let out = issue(&detail(), 4000);
        assert!(
            out.starts_with("owner/repo#12 Cache-aware routing\nstatus: parked\n"),
            "{out}"
        );
        assert!(out.contains("git_ref: main\n"), "{out}");
        assert!(
            !out.contains("codegen_contract:"),
            "a null field is absent, not empty: {out}"
        );
        assert!(
            out.contains("2026-07-01 scoping -> parked [wren] — scope check failed\n"),
            "{out}"
        );
        assert!(
            out.contains("run 0192run  failed  score=0.420  cost=$1.50  pod=turn-abc"),
            "{out}"
        );
    }

    #[test]
    fn a_truncated_long_text_says_where_the_rest_is() {
        let lt = dto::LongText {
            text: "the ranker found".into(),
            truncated: true,
        };
        assert_eq!(
            lt.render(),
            "the ranker found… [truncated; see `issue <key>`]"
        );
    }

    fn run_graph() -> dto::RunGraph {
        serde_json::from_str(
            r#"{"plan_version":3,
              "tasks":[
                {"name":"judge","kind":"top_k","depends_on":["measure","build"],"session":"s","needs":"any","required":true},
                {"name":"build","kind":"command","depends_on":[],"session":"s","needs":"all","required":true},
                {"name":"measure","kind":"agent","depends_on":["build"],"session":"s","needs":"all","required":true},
                {"name":"publish","kind":"command","depends_on":["judge"],"session":"s","needs":"all","required":false}],
              "results":[
                {"iter":0,"task":"build","status":"pass","note":"","cost_usd":0.0,"secs":21.0},
                {"iter":0,"task":"measure","status":"fail","note":"no baseline","cost_usd":0.5,"secs":90.0},
                {"iter":1,"task":"build","status":"pass","note":"","cost_usd":0.0,"secs":19.0},
                {"iter":1,"task":"measure","status":"pass","note":"tokens/s 1240","cost_usd":0.7,"secs":88.0},
                {"iter":1,"task":"judge","status":"blocked","note":"waiting on measure","cost_usd":null,"secs":null}]}"#,
        )
        .expect("the run graph fixture parses")
    }

    /// Each declared bound rides the graph off the task that spends it, and a revision that stored
    /// no exposure says so rather than reading as a pack that writes nothing.
    #[test]
    fn the_graph_carries_the_declared_outputs_and_names_an_undeclared_one() {
        let bare = graph("0192run", &run_graph());
        assert!(
            bare.contains("outputs undeclared (this revision stored no exposure)"),
            "{bare}"
        );

        let mut g = run_graph();
        g.outputs = Some(
            serde_json::from_str(
                r#"[{"kind":"draft-pr","count":1,
                     "target":{"kind":"address","address":"owner/repo"},"attached_to":"publish"},
                    {"kind":"gpu-capture","count":2,"target":null,"attached_to":null}]"#,
            )
            .expect("outputs parse"),
        );
        let out = graph("0192run", &g);
        assert!(
            out.contains("draft-pr x1 -> owner/repo  (from publish)"),
            "{out}"
        );
        assert!(out.contains("gpu-capture x2  (sink)"), "{out}");

        let mermaid = graph_mermaid(&g);
        assert!(
            mermaid.contains("o_0([\"draft-pr x1<br/>owner/repo\"])")
                && mermaid.contains("t_publish --> o_0"),
            "an output is a terminal node in its own shape, drawn off its producer: {mermaid}"
        );
        assert!(
            mermaid.contains("t_publish --> o_1"),
            "an unattached bound hangs off the plan's sink: {mermaid}"
        );
    }

    /// An engine default is a bound the pack never asked for. It is listed under its own heading
    /// and stays out of the graph, so a pack declaring nothing reads as one; a bound with no
    /// source (an older controller) still counts as declared.
    #[test]
    fn engine_default_bounds_are_listed_apart_and_never_drawn() {
        let mut g = run_graph();
        g.outputs = Some(
            serde_json::from_str(
                r#"[{"kind":"image-push","count":100,"target":null,"attached_to":null,"source":"engine-default"},
                    {"kind":"draft-pr","count":2,"target":null,"attached_to":null,"source":"engine-default"}]"#,
            )
            .expect("outputs parse"),
        );
        let out = graph("0192run", &g);
        assert!(out.contains("outputs: none declared"), "{out}");
        assert!(out.contains("2 engine-default bounds"), "{out}");
        assert!(out.contains("image-push x100  (sink)"), "{out}");
        let mermaid = graph_mermaid(&g);
        assert!(
            !mermaid.contains("o_0("),
            "a default is not a graph node: {mermaid}"
        );

        g.outputs = Some(
            serde_json::from_str(
                r#"[{"kind":"tracker-comment","count":3,"target":null,"attached_to":"publish","source":"manifest"},
                    {"kind":"chat-message","count":8,"target":null,"attached_to":null},
                    {"kind":"gpu-capture","count":100,"target":null,"attached_to":null,"source":"engine-default"}]"#,
            )
            .expect("outputs parse"),
        );
        let out = graph("0192run", &g);
        assert!(out.contains("2 declared outputs"), "{out}");
        assert!(out.contains("1 engine-default bounds"), "{out}");
        let mermaid = graph_mermaid(&g);
        assert!(
            mermaid.contains("o_0([\"tracker-comment x3\"])"),
            "{mermaid}"
        );
        assert!(mermaid.contains("o_1([\"chat-message x8\"])"), "{mermaid}");
        assert!(!mermaid.contains("o_2("), "{mermaid}");
    }

    /// The case this exists for: a spawn failure's cause is longer than the column, and cutting it
    /// mid-sentence used to leave `json=true` as the only way to read why a run died.
    #[test]
    fn a_note_too_long_for_the_table_is_still_readable_in_full() {
        let mut g = run_graph();
        let cause = "transport retries exhausted (3 attempts): agent spawn failed: failed to \
                     launch agent source: No such file or directory (os error 2)";
        g.results
            .iter_mut()
            .find(|r| r.task == "measure" && r.iter == 1)
            .expect("the fixture has a latest measure result")
            .note = cause.to_string();
        let out = graph("0192run", &g);
        assert!(
            out.contains("agent spawn faile"),
            "the table still shows the head of the note: {out}"
        );
        assert!(
            out.contains("notes in full"),
            "a cut note earns the section: {out}"
        );
        assert!(
            out.contains(cause),
            "and the whole cause is there without json=true: {out}"
        );
    }

    /// A note that fits is already fully visible, so repeating it below the table is noise.
    #[test]
    fn a_note_that_fits_the_table_is_not_repeated() {
        let out = graph("0192run", &run_graph());
        assert!(out.contains("tokens/s 1240"), "{out}");
        assert!(!out.contains("notes in full"), "{out}");
    }

    /// Dependency order, not declaration order: the fixture declares `judge` first precisely to
    /// catch a renderer that just echoes the array.
    #[test]
    fn graph_renders_in_dependency_order() {
        let out = graph("0192run", &run_graph());
        let names: Vec<&str> = out
            .lines()
            .skip(2)
            .take_while(|l| !l.trim().is_empty())
            .filter_map(|l| l.split_whitespace().nth(1))
            .collect();
        assert_eq!(names, vec!["build", "measure", "judge", "publish"], "{out}");
    }

    #[test]
    fn graph_shows_the_latest_status_per_task_not_every_iteration() {
        let out = graph("0192run", &run_graph());
        let measure = out
            .lines()
            .find(|l| l.contains("measure "))
            .expect("measure row");
        assert!(
            measure.starts_with("pass"),
            "iteration 1 passed, so pass: {measure}"
        );
        assert!(measure.contains("i1 $0.70 88s"), "{measure}");
        assert!(measure.contains("tokens/s 1240"), "{measure}");
        assert!(
            !out.contains("no baseline"),
            "the superseded iter-0 note is gone: {out}"
        );
    }

    /// `needs=any` changes how you read a red dependency, so it has to survive into the render.
    #[test]
    fn graph_marks_optional_tasks_and_any_dependencies() {
        let out = graph("0192run", &run_graph());
        let judge = out
            .lines()
            .find(|l| l.contains(" judge "))
            .expect("judge row");
        assert!(judge.contains("measure,build (any)"), "{judge}");
        assert!(judge.starts_with("blocked"), "{judge}");
        let publish = out
            .lines()
            .find(|l| l.contains("publish"))
            .expect("publish row");
        assert!(publish.starts_with("pending"), "never ran: {publish}");
        assert!(publish.contains("opt"), "{publish}");
    }

    #[test]
    fn graph_header_counts_tasks_and_iterations() {
        let out = graph("0192run", &run_graph());
        assert_eq!(
            out.lines().next().expect("header"),
            "run 0192run  plan v3  4 tasks  2 iterations"
        );
    }

    /// A run whose plan never got admitted still deserves a sentence rather than an empty table.
    #[test]
    fn an_empty_graph_says_so() {
        let empty: dto::RunGraph =
            serde_json::from_str(r#"{"plan_version":0,"tasks":[],"results":[]}"#).expect("parse");
        assert_eq!(graph("r", &empty), "run r: plan v0 has no tasks\n");
    }

    /// A cyclic plan is a bug someone is currently chasing. Printing a short table would hide it.
    #[test]
    fn a_cyclic_plan_still_lists_every_task() {
        let cyclic: dto::RunGraph = serde_json::from_str(
            r#"{"plan_version":1,"results":[],"tasks":[
                {"name":"a","kind":"command","depends_on":["b"],"session":"s","needs":"all","required":true},
                {"name":"b","kind":"command","depends_on":["a"],"session":"s","needs":"all","required":true}]}"#,
        )
        .expect("parse");
        let out = graph("r", &cyclic);
        assert!(out.contains(" a "), "{out}");
        assert!(out.contains(" b "), "{out}");
    }

    #[test]
    fn mermaid_sanitizes_ids_and_keeps_real_names_in_labels() {
        let g: dto::RunGraph = serde_json::from_str(
            r#"{"plan_version":1,"results":[],"tasks":[
                {"name":"build-image","kind":"command","depends_on":[],"session":"s","needs":"all","required":true},
                {"name":"measure.p50","kind":"agent","depends_on":["build-image"],"session":"s","needs":"all","required":true}]}"#,
        )
        .expect("parse");
        let out = graph_mermaid(&g);
        assert!(out.starts_with("flowchart TD\n"), "{out}");
        assert!(
            out.contains(r#"t_measure_p50["measure.p50<br/>agent · pending"]"#),
            "{out}"
        );
        assert!(out.contains("t_build_image --> t_measure_p50"), "{out}");
    }

    #[test]
    fn turn_lines_are_one_per_pod() {
        let list: Vec<dto::Turn> = serde_json::from_str(
            r#"[{"pod_name":"turn-abc","kind":"scope","issue_key":"owner/repo#12","state":"failed",
                 "cost_tag":"scope","result":null,
                 "error":{"text":"timed out waiting for a verdict","truncated":true},
                 "created_at":"2026-07-01T10:00:00Z","updated_at":"2026-07-01T10:20:00Z",
                 "terminal_at":"2026-07-01T10:20:00Z"}]"#,
        )
        .expect("parse");
        let out = turns(&list);
        assert_eq!(out.lines().next().expect("header"), "1 turns");
        let row = out.lines().nth(2).expect("the row");
        assert!(
            row.starts_with("turn-abc  scope  failed  owner/repo#12"),
            "{row}"
        );
        // The list truncates `error`; the one-line view doesn't print it at all, so nothing lies.
        assert!(!out.contains("timed out"), "{out}");

        let full = turn(&list[0]);
        assert!(full.starts_with("turn-abc [scope/failed]\n"), "{full}");
        assert!(
            full.contains("timed out waiting for a verdict… [truncated; see `issue <key>`]"),
            "{full}"
        );
    }

    #[test]
    fn contracts_name_themselves_or_say_there_are_none() {
        let c: dto::BrokerContracts =
            serde_json::from_str(r#"{"names":["epp-measure","vllm-bench"]}"#).expect("parse");
        assert_eq!(
            contracts(&c),
            "2 broker contracts\nepp-measure\nvllm-bench\n"
        );
        let none: dto::BrokerContracts = serde_json::from_str(r#"{"names":[]}"#).expect("parse");
        assert!(contracts(&none).contains("must be omitted on adopt"));
    }

    /// The acking line an operator scans for. A change booked to `anonymous` is the failure mode
    /// this whole identity story exists to prevent, so it must not read like success.
    #[test]
    fn an_ack_without_an_actor_says_the_change_was_anonymous() {
        let out = ack("park", "owner/repo#12", None, "reason: flaky measure");
        assert!(
            out.starts_with("park owner/repo#12: accepted\nreason: flaky measure\n"),
            "{out}"
        );
        assert!(
            out.contains("actor: anonymous (the controller recorded no identity"),
            "{out}"
        );
    }

    #[test]
    fn an_ack_echoes_the_actor_the_controller_recorded() {
        let out = ack("unpark", "owner/repo#12", Some("wren"), "");
        assert_eq!(out, "unpark owner/repo#12: accepted\nactor: wren\n");
    }

    #[test]
    fn approvals_list_the_open_ones_and_the_kept_prs() {
        let d: dto::Approvals = serde_json::from_str(
            r#"{"awaiting_approval":[{"key":"owner/repo#12","repo":"owner/repo","scope_id":7,
                 "approval_pr":"https://github.com/owner/repo/pull/9","stale":true}],
                "kept_prs":[{"issue":"owner/repo#3","pr_url":"https://github.com/owner/repo/pull/4"}]}"#,
        )
        .expect("parse");
        let out = approvals(&d);
        assert!(out.starts_with("1 awaiting approval\n"), "{out}");
        assert!(
            out.contains("STALE"),
            "a stale pack must not look approvable: {out}"
        );
        assert!(
            out.contains("1 kept PRs\nowner/repo#3  https://github.com/owner/repo/pull/4"),
            "{out}"
        );
    }

    #[test]
    fn approval_for_one_issue_prints_the_pr_and_the_pack() {
        let out = approval_detail("owner/repo#12", &detail());
        assert!(out.contains("scope 7  pack sha256:abcd"), "{out}");
        assert!(
            out.contains("https://github.com/owner/repo/pull/9"),
            "{out}"
        );
        assert!(out.contains("approved: NOT YET"), "{out}");
    }

    /// Approving a scope accepts its exposure, so the approval surface prints it.
    #[test]
    fn approval_prints_the_exposure_it_binds_to() {
        let mut d = detail();
        {
            let scope = &mut d.scopes[0].scope;
            scope.exposure.lines = vec![
                "outputs:".to_string(),
                "  draft-pr x1 -> owner/repo".to_string(),
                "capabilities:".to_string(),
                "  credential GH_TOKEN (agent) github".to_string(),
            ];
            scope.exposure.digest = Some("sha256:expo".to_string());
        }
        let out = approval_detail("owner/repo#12", &d);
        assert!(out.contains("exposure sha256:expo"), "{out}");
        assert!(out.contains("  draft-pr x1 -> owner/repo"), "{out}");

        d.scopes[0].scope.exposure.approved_digest = Some("sha256:older".to_string());
        let out = approval_detail("owner/repo#12", &d);
        assert!(
            out.contains("approval bound to exposure sha256:older")
                && out.contains("WARNING: the stored exposure has changed"),
            "an approval that bound to a different blast radius says so: {out}"
        );
    }

    /// A revision registered by an engine with no exposure extraction renders the controller's
    /// undeclared marker rather than a blank section.
    #[test]
    fn approval_prints_the_undeclared_marker_for_a_legacy_revision() {
        let mut d = detail();
        d.scopes[0].scope.exposure.lines = vec![
            "outputs undeclared (registered by an engine with no exposure extraction)".to_string(),
        ];
        let out = approval_detail("owner/repo#12", &d);
        assert!(out.contains("exposure (none stored)"), "{out}");
        assert!(out.contains("outputs undeclared"), "{out}");
    }

    #[test]
    fn approval_says_so_when_there_is_none() {
        let mut d = detail();
        d.scopes.clear();
        assert_eq!(
            approval_detail("owner/repo#12", &d),
            "owner/repo#12: no scope has an approval PR (status parked)\n"
        );
    }

    /// The reason the renderers exist at all. If this stops holding, the module is not paying for
    /// itself.
    #[test]
    fn rendering_beats_the_raw_json_by_a_lot() {
        // The same two rows the other tests use, as the controller would serialize them.
        let raw = serde_json::to_string(&serde_json::json!([
            {"key":"owner/repo#12","repo":"owner/repo",
             "kind":{"type":"github","owner":"owner","repo":"repo","number":12},
             "tier":"T1","status":"awaiting_approval","priority":40,"evidence_url":null,
             "parked_reason":null,"parked_by":null,"updated_at":"2026-07-01T11:00:00Z",
             "upstream_updated_at":"2026-06-30T09:00:00Z","title":"Cache-aware request routing",
             "author":"someone","labels":["routing"],"pr_url":null,"stale_closable":false,
             "git_ref":null,"codegen_contract":null},
            {"key":"scenario:0192f4a1-8c3e-7000-9abc-1234567890ab","repo":"owner/other",
             "kind":{"type":"scenario","id":"0192f4a1-8c3e-7000-9abc-1234567890ab"},
             "tier":"T1","status":"scoping","priority":50,"evidence_url":null,
             "parked_reason":{"text":"the ranker found the ask already implemented and the","truncated":true},
             "parked_by":"machine","updated_at":"2026-07-02T08:00:00Z","upstream_updated_at":null,
             "title":"Adopt the EPP calibration scenario and drive it to a measured candidate set",
             "author":null,"labels":[],"pr_url":null,"stale_closable":true,
             "git_ref":"nv_dev","codegen_contract":"epp-measure"}
        ]))
        .expect("encode")
        .len();
        let rendered = issues(&issue_list(), issue_list().len()).len();
        assert!(
            rendered * 2 < raw,
            "rendered {rendered} bytes vs raw {raw}: not worth the module"
        );
    }

    fn import() -> dto::PackImport {
        serde_json::from_str(
            r#"{"id":"0192f4a1-8c3e-7000-9abc-1234567890ab","repo":"owner/packs",
               "git_ref":"main","path":"packs/calibrate","rev":"9f1c2b3",
               "tar_digest":"sha256:aa","params_schema":null,"schema_digest":"sha256:bb",
               "graph":null,"diagnostics":[],"core_rev":"c0ffee","status":"pending",
               "playbook":null,"draft_id":null,"proposed_by":"agent-7",
               "created_at":"2026-08-23T09:00:00Z","resolved_by":null,"resolved_at":null}"#,
        )
        .expect("the import fixture parses")
    }

    #[test]
    fn an_import_renders_its_pin_its_digest_and_the_link_to_hand_over() {
        let out = pack_import(&import(), "https://c.example.com/playbooks/import/0192f4a1");
        assert!(
            out.starts_with("import 0192f4a1-8c3e-7000-9abc-1234567890ab: pending\n"),
            "{out}"
        );
        assert!(out.contains("repo: owner/packs @ main\n"), "{out}");
        assert!(out.contains("path: packs/calibrate\n"), "{out}");
        assert!(out.contains("rev: 9f1c2b3\n"), "{out}");
        assert!(out.contains("schema: sha256:bb\n"), "{out}");
        assert!(out.contains("diagnostics: none\n"), "{out}");
        assert!(out.contains("proposed_by: agent-7\n"), "{out}");
        assert!(
            out.ends_with("preview: https://c.example.com/playbooks/import/0192f4a1\n"),
            "the link is the last thing a reader needs: {out}"
        );
    }

    /// A pack whose source the engine refused still lands a row. Saying "registered ok" for it
    /// would be the worst possible summary, so the absence of a schema is spelled out.
    #[test]
    fn an_import_that_did_not_compile_says_so_and_carries_the_errors() {
        let mut i = import();
        i.schema_digest = None;
        i.diagnostics = vec!["wf.crux:12:3: unknown task `mesure`".into()];
        let out = pack_import(&i, "https://c/x");
        assert!(
            out.contains("schema: none (the pack's source did not compile)"),
            "{out}"
        );
        assert!(
            out.contains("diagnostics: 1\n  wf.crux:12:3: unknown task `mesure`\n"),
            "{out}"
        );
    }

    fn files() -> dto::DraftFiles {
        serde_json::from_str(
            r#"{"version":7,"saved_by":"wynn","saved_at":"2026-08-23T10:00:00Z",
               "diagnostics":[{"file":"wf.crux","line":3,"col":1,"message":"unused param `n`"}],
               "files":{"crucible.toml":"[workflow]\nfile = \"wf.crux\"\n","wf.crux":"task a {}"}}"#,
        )
        .expect("the files fixture parses")
    }

    #[test]
    fn draft_files_carry_the_base_version_the_saver_and_every_byte() {
        let out = draft_files("calibrate", &files());
        assert!(out.starts_with("draft calibrate version 7\n"), "{out}");
        assert!(
            out.contains("saved_by: wynn at 2026-08-23T10:00:00Z\n"),
            "{out}"
        );
        assert!(
            out.contains("diagnostics: 1\n  wf.crux:3:1: unused param `n`\n"),
            "{out}"
        );
        assert!(out.contains("files: 2\n"), "{out}");
        assert!(
            out.contains("\n--- wf.crux\ntask a {}\n"),
            "a file without a trailing newline still ends one: {out}"
        );
        assert!(out.contains("\n--- crucible.toml\n[workflow]\n"), "{out}");
    }

    #[test]
    fn a_save_returns_its_new_version_and_the_studio_link() {
        let saved: dto::DraftCompile = serde_json::from_str(
            r#"{"version":8,"saved_by":"agent-7","saved_at":"2026-08-23T11:00:00Z",
               "params_schema":null,"schema_digest":"sha256:cc","graph":null,"diagnostics":[]}"#,
        )
        .expect("the save fixture parses");
        let out = draft_saved("calibrate", &saved, "https://c/playbooks/drafts/calibrate");
        assert!(
            out.starts_with("saved draft calibrate as version 8\n"),
            "{out}"
        );
        assert!(
            out.contains("saved_by: agent-7 at 2026-08-23T11:00:00Z\n"),
            "{out}"
        );
        assert!(out.contains("schema: sha256:cc\n"), "{out}");
        assert!(
            out.ends_with("studio: https://c/playbooks/drafts/calibrate\n"),
            "{out}"
        );
    }

    /// The refusal is read by an agent mid-loop. It has to say that nothing was written, which
    /// version to fetch, and which base to save against — otherwise the retry clobbers the human.
    #[test]
    fn a_stale_base_refusal_names_the_version_to_merge_onto() {
        let stale: dto::StaleBase = serde_json::from_str(
            r#"{"error":"this save edited version 7, but wynn saved version 8 at 2026-08-23T11:00:00Z; re-read that version and merge",
               "base_version":7,"current_version":8,"saved_by":"wynn",
               "saved_at":"2026-08-23T11:00:00Z"}"#,
        )
        .expect("the stale fixture parses");
        let out = stale_base("calibrate", &stale);
        assert!(
            out.starts_with("REFUSED: this save edited version 7"),
            "{out}"
        );
        assert!(out.contains("current version: 8\n"), "{out}");
        assert!(out.contains("nothing was written"), "{out}");
        assert!(
            out.contains("crucible_draft_files draft_id=calibrate version=8"),
            "{out}"
        );
        assert!(out.contains("base_version=8"), "{out}");
    }

    /// A preview is read to decide whether a launch is even possible, so a version that never
    /// compiled has to say so where the digest would be, not print an empty field.
    #[test]
    fn a_preview_of_a_version_that_never_compiled_says_so_instead_of_a_digest() {
        let saved: dto::DraftCompile = serde_json::from_str(
            r#"{"version":3,"saved_by":"agent-7","saved_at":"2026-08-23T11:00:00Z",
               "schema_digest":null,
               "diagnostics":[{"file":"wf.crux","line":9,"col":2,"message":"unknown task `bulid`"}]}"#,
        )
        .expect("the preview fixture parses");
        let out = draft_preview("calibrate", &saved, "https://c/playbooks/drafts/calibrate");
        assert!(
            out.starts_with("draft calibrate version 3 compiled\n"),
            "{out}"
        );
        assert!(
            out.contains("schema: none (this version did not compile)\n"),
            "{out}"
        );
        assert!(
            out.contains("diagnostics: 1\n  wf.crux:9:2: unknown task `bulid`\n"),
            "{out}"
        );
        assert!(
            out.ends_with("studio: https://c/playbooks/drafts/calibrate\n"),
            "{out}"
        );
    }

    /// The PR is the whole answer to a graduation: an agent that cannot read the URL back has no
    /// way to finish the promotion.
    #[test]
    fn a_graduation_returns_the_pr_and_what_retires_the_draft() {
        let ack: dto::GraduateAck =
            serde_json::from_str(r#"{"pr_url":"https://github.com/wren/packs/pull/12"}"#)
                .expect("the graduate fixture parses");
        let out = draft_graduated("calibrate", &ack);
        assert!(out.starts_with("graduated draft calibrate\n"), "{out}");
        assert!(
            out.contains("PR: https://github.com/wren/packs/pull/12\n"),
            "{out}"
        );
        assert!(
            out.contains("imported from the same repo and path"),
            "{out}"
        );
    }

    /// The registry lists where each pack came from: a git pin by repo and directory, a published
    /// draft by the version it was published from.
    #[test]
    fn the_registry_names_a_git_pin_and_a_published_draft_by_their_sources() {
        let list: Vec<dto::Playbook> = serde_json::from_str(
            r#"[{"id":"survey","description":"reads a paper","rev":"7c2c1a563813ce95",
                 "source":{"kind":"git","repo":"owner/packs","git_ref":null,"path":"packs/survey"},
                 "created_by":"wren"},
                {"id":"mlr-pack","description":"mlr sweep","rev":"sha256:beefcafe",
                 "source":{"kind":"draft","draft":"studio","version":3},
                 "created_by":"reed"}]"#,
        )
        .expect("the registry fixture parses");
        let out = playbooks(&list);
        assert!(out.contains("owner/packs/packs/survey"), "{out}");
        assert!(out.contains("draft studio v3"), "{out}");
    }

    /// A publish lands a playbook at once; what the agent needs back is its id, its pin, and that
    /// the draft is still where edits go.
    #[test]
    fn a_publish_names_the_playbook_and_keeps_the_draft_live() {
        let ack: dto::PublishAck = serde_json::from_str(
            r#"{"id":"mlr-pack","rev":"sha256:0123456789abcdef0123","tar_digest":"sha256:0123",
               "schema_digest":"sha256:form","schema_changed":true,"exposure_digest":null,
               "exposure_changed":false}"#,
        )
        .expect("the publish fixture parses");
        let out = draft_published("studio", &ack);
        assert!(
            out.starts_with("published draft studio as playbook mlr-pack @ sha256:0123456789ab\n"),
            "{out}"
        );
        assert!(out.contains("the launch form changed.\n"), "{out}");
        assert!(!out.contains("exposure"), "{out}");
        assert!(
            out.ends_with("publishing it again re-pins mlr-pack.\n"),
            "{out}"
        );
    }
}
