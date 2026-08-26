//! HTML emitter for `crucible flow`: one self-contained explainer page (inline CSS +
//! inline SVG, no external requests) rendered from the flow-model IR. The page is built
//! for "here is how crucible works" walkthroughs over a real run: a header strip that
//! reads in seconds, a time-proportional swimlane of the iterations (the hero), and a
//! per-iteration detail fold. The only script is a small vanilla toggle so clicking a
//! swimlane column opens its detail; everything renders without it.
//!
//! Vocabulary is deliberately plain: a stranger doesn't know what a "rung" is, so the
//! page says "checks"; a propose turn is "the agent writes code".

use crate::flow::{FlowModel, Iteration, pr_short, score3};
use std::fmt::Write as _;

/// Push a formatted fragment; writing to a `String` cannot fail.
macro_rules! w {
    ($out:expr, $($arg:tt)*) => { let _ = write!($out, $($arg)*); };
}

// Light-committed palette: explicit backgrounds everywhere so a dark-mode browser
// renders the page identically. One accent hue (blue) + neutrals; keep/discard/reject
// always carry a shape or word, never color alone.
//
// The values are defaults on `:root` and every use site (CSS rules and SVG fill/stroke
// attributes alike) reads them back as `var(--flow-*)`, so an embedder that injects its
// own `:root` block repaints the page without touching this emitter. An older page with
// the hexes baked in just ignores such an override.
const PALETTE: [(&str, &str); 15] = [
    ("--flow-bg", "#f7f7f5"),
    ("--flow-surface", "#fcfcfb"),
    ("--flow-ink", "#1a1917"),
    ("--flow-ink-2", "#52514e"),
    ("--flow-muted", "#898781"),
    ("--flow-hairline", "#e5e4dd"),
    ("--flow-accent", "#2a78d6"),
    ("--flow-accent-deep", "#1c5cab"),
    ("--flow-pass-fill", "#dddcd4"),
    ("--flow-fail-fill", "#3a3937"),
    ("--flow-dot", "#9a9891"),
    ("--flow-line", "#c3c2b7"),
    ("--flow-line-soft", "#d0cfc8"),
    ("--flow-badge-border", "#b8b6ae"),
    ("--flow-on-accent", "#ffffff"),
];

const PAGE_BG: &str = "var(--flow-bg)";
const SURFACE: &str = "var(--flow-surface)";
const INK: &str = "var(--flow-ink)";
const INK_2: &str = "var(--flow-ink-2)";
const MUTED: &str = "var(--flow-muted)";
const HAIRLINE: &str = "var(--flow-hairline)";
const ACCENT: &str = "var(--flow-accent)";
const ACCENT_DEEP: &str = "var(--flow-accent-deep)";
const ACCENT_WASH: &str = "rgba(42,120,214,0.07)";
const TURN_FILL: &str = "rgba(42,120,214,0.24)";
const PASS_FILL: &str = "var(--flow-pass-fill)";
const FAIL_FILL: &str = "var(--flow-fail-fill)";
const DOT_GRAY: &str = "var(--flow-dot)";
const LINE: &str = "var(--flow-line)";
const LINE_SOFT: &str = "var(--flow-line-soft)";
const BADGE_BORDER: &str = "var(--flow-badge-border)";
const ON_ACCENT: &str = "var(--flow-on-accent)";

// Hero SVG geometry.
const SVG_W: f64 = 1140.0;
const GUTTER: f64 = 158.0;
const PLOT_W: f64 = SVG_W - GUTTER - 12.0;
const COL_GAP: f64 = 8.0;
const MIN_COL: f64 = 64.0;
const HDR_TOP: f64 = 10.0;
const HDR_H: f64 = 56.0;
const SCORE_TOP: f64 = HDR_TOP + HDR_H + 12.0;
const SCORE_H: f64 = 104.0;
const AGENT_TOP: f64 = SCORE_TOP + SCORE_H + 24.0;
const AGENT_H: f64 = 32.0;
const LANE_H: f64 = 20.0;
const LANE_STEP: f64 = 27.0;

pub fn emit_html(m: &FlowModel) -> String {
    let mut out = String::with_capacity(64 * 1024);
    head(&mut out, m);
    header_strip(&mut out, m);
    preflight(&mut out, m);
    hero(&mut out, m);
    details(&mut out, m);
    footer(&mut out, m);
    out.push_str("<script>\n");
    out.push_str(SCRIPT);
    out.push_str("</script>\n</body>\n</html>\n");
    out
}

fn esc(s: &str) -> String {
    let mut e = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => e.push_str("&amp;"),
            '<' => e.push_str("&lt;"),
            '>' => e.push_str("&gt;"),
            '"' => e.push_str("&quot;"),
            '\'' => e.push_str("&#39;"),
            _ => e.push(c),
        }
    }
    e
}

fn fmt_dur(secs: f64) -> String {
    let s = secs.round().max(0.0) as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{s}s")
    }
}

/// Elapsed tick label: `+h:mm`.
fn fmt_hm(secs: f64) -> String {
    let s = secs.round().max(0.0) as u64;
    format!("+{}:{:02}", s / 3600, (s % 3600) / 60)
}

fn parse_ts(s: Option<&String>) -> Option<jiff::Timestamp> {
    s.and_then(|s| s.parse().ok())
}

fn clock(ts: &jiff::Timestamp) -> String {
    ts.strftime("%H:%M").to_string()
}

/// Is a signed delta an improvement, given the run's score direction?
fn improvement(direction: Option<&str>, delta: f64) -> Option<bool> {
    match direction {
        Some("lower") => Some(delta < 0.0),
        Some("higher") => Some(delta > 0.0),
        _ => None,
    }
}

fn delta_phrase(direction: Option<&str>, delta: f64) -> String {
    match improvement(direction, delta) {
        Some(true) => format!("{:.2}% better", delta.abs()),
        Some(false) => format!("{:.2}% worse", delta.abs()),
        None => format!("{delta:+.2}%"),
    }
}

fn direction_phrase(direction: Option<&str>) -> Option<&'static str> {
    match direction {
        Some("lower") => Some("lower is better"),
        Some("higher") => Some("higher is better"),
        _ => None,
    }
}

/// Presentation label for a normalized decision.
fn decision_word(decision: &str) -> &'static str {
    match decision {
        "keep" => "kept",
        "rejected" => "rejected",
        _ => "discarded",
    }
}

/// An MCP tool id (`mcp__<server>__<tool>`) shrinks to its tool name.
fn short_tool(tool: &str) -> &str {
    if tool.starts_with("mcp__") {
        tool.rsplit("__").next().unwrap_or(tool)
    } else {
        tool
    }
}

/// `1 file`, `5 files`.
fn count_noun(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Crude but sufficient width estimate for fit checks in the SVG (system sans).
fn text_w(s: &str, font_px: f64) -> f64 {
    s.chars().count() as f64 * font_px * 0.55
}

/// Display form of a raw gate note: long float tokens round to 3 decimals (like the
/// hero numbers) and consecutive duplicate tokens collapse — "score=0.9375
/// score=0.9375" is a gate echo, not signal.
fn fmt_note(s: &str) -> String {
    let mut toks: Vec<String> = Vec::new();
    for tok in s.split_whitespace() {
        let tok = round_floats(tok);
        if toks.last() != Some(&tok) {
            toks.push(tok);
        }
    }
    toks.join(" ")
}

/// Round every float with more than three decimals inside a token; anything that
/// doesn't parse as one number (versions like "1.2.3") stays verbatim.
fn round_floats(tok: &str) -> String {
    let mut out = String::with_capacity(tok.len());
    let mut num = String::new();
    for c in tok.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            flush_num(&mut out, &mut num);
            out.push(c);
        }
    }
    flush_num(&mut out, &mut num);
    out
}

fn flush_num(out: &mut String, num: &mut String) {
    let rounded = num
        .split_once('.')
        .filter(|(_, frac)| frac.len() > 3)
        .and_then(|_| num.parse::<f64>().ok())
        .map(|v| format!("{v:.3}"));
    match rounded {
        Some(r) => out.push_str(&r),
        None => out.push_str(num),
    }
    num.clear();
}

/// The goal line carries markdown backticks; render `code` spans as real <code>
/// instead of leaking literal backticks. Unbalanced backticks stay verbatim.
/// Output is escaped HTML.
fn md_code(s: &str) -> String {
    if !s.matches('`').count().is_multiple_of(2) {
        return esc(s);
    }
    let mut out = String::with_capacity(s.len());
    for (i, part) in s.split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str("<code>");
            out.push_str(&esc(part));
            out.push_str("</code>");
        } else {
            out.push_str(&esc(part));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Page skeleton
// ---------------------------------------------------------------------------

fn head(out: &mut String, m: &FlowModel) {
    // A browser tab title renders no markdown, so the backticks just drop.
    let title = if m.run.goal_line.is_empty() {
        "crucible run".to_string()
    } else {
        format!("crucible run — {}", m.run.goal_line.replace('`', ""))
    };
    w!(
        out,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n{}\n</style>\n</head>\n<body>\n<main class=\"page\">\n",
        esc(&title),
        css()
    );
}

fn root_vars() -> String {
    let mut s = String::from(":root {\n");
    for (name, value) in PALETTE {
        let _ = writeln!(s, "  {name}: {value};");
    }
    s.push('}');
    s
}

fn css() -> String {
    // format! only to splice the palette consts; the rules themselves are static.
    format!(
        r##"{}
* {{ box-sizing: border-box; margin: 0; padding: 0; }}
html {{ color-scheme: light; }}
body {{ background: {PAGE_BG}; color: {INK}; font: 14px/1.55 system-ui, -apple-system, "Segoe UI", sans-serif; }}
.page {{ max-width: 1180px; margin: 0 auto; padding: 44px 28px 56px; }}
.eyebrow {{ font-size: 11.5px; font-weight: 650; letter-spacing: 0.09em; text-transform: uppercase; color: {MUTED}; margin-bottom: 8px; }}
h1 {{ font-size: 23px; font-weight: 650; letter-spacing: -0.01em; line-height: 1.3; max-width: 60ch; }}
h1 code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.88em; background: rgba(26,25,23,0.05); padding: 0 4px; border-radius: 4px; }}
h2 {{ font-size: 15px; font-weight: 650; margin-bottom: 4px; }}
.hint {{ color: {MUTED}; font-size: 12.5px; margin-bottom: 14px; }}
.stats {{ display: flex; flex-wrap: wrap; gap: 18px 0; margin: 26px 0 8px; }}
.tile {{ flex: 1 1 150px; padding: 2px 22px; border-left: 1px solid {HAIRLINE}; min-width: 130px; }}
.tile:first-child {{ border-left: none; padding-left: 0; }}
.tile.wide {{ flex: 1.7 1 230px; }}
.lbl {{ font-size: 11px; font-weight: 600; letter-spacing: 0.06em; text-transform: uppercase; color: {MUTED}; margin-bottom: 3px; white-space: nowrap; }}
.val {{ font-size: 21px; font-weight: 650; letter-spacing: -0.01em; white-space: nowrap; }}
.val.sm {{ font-size: 15px; padding-top: 5px; }}
.val .arr {{ color: {MUTED}; font-weight: 400; padding: 0 2px; }}
.sub {{ font-size: 12px; color: {INK_2}; margin-top: 2px; }}
.sub.good {{ color: {ACCENT_DEEP}; font-weight: 650; }}
.spark {{ vertical-align: -3px; margin-right: 6px; }}
section {{ margin-top: 36px; }}
details.pre {{ margin-top: 22px; border-top: 1px solid {HAIRLINE}; border-bottom: 1px solid {HAIRLINE}; }}
details.pre summary {{ padding: 9px 2px; }}
details.pre table {{ margin: 4px 0 14px; }}
summary {{ cursor: pointer; list-style: none; font-size: 13px; color: {INK_2}; }}
summary::-webkit-details-marker {{ display: none; }}
summary::before {{ content: "▸"; display: inline-block; width: 16px; color: {MUTED}; transition: transform 0.12s; }}
details[open] > summary::before {{ transform: rotate(90deg); }}
.hero-card {{ background: {SURFACE}; border: 1px solid {HAIRLINE}; border-radius: 10px; padding: 18px 14px 10px; overflow-x: auto; }}
.hero-card svg {{ display: block; min-width: 900px; width: 100%; height: auto; }}
.legend {{ font-size: 11.5px; color: {INK_2}; margin: 10px 4px 0; }}
.legend > span {{ margin-right: 18px; white-space: nowrap; }}
.sw {{ display: inline-block; width: 14px; height: 9px; border-radius: 2px; vertical-align: -1px; margin-right: 5px; }}
.sw.turn {{ background: {TURN_FILL}; }}
.sw.pass {{ background: {PASS_FILL}; }}
.sw.fail {{ background: {FAIL_FILL}; }}
.dotm {{ display: inline-block; width: 7px; height: 7px; border-radius: 50%; border: 1.5px solid {DOT_GRAY}; background: {SURFACE}; vertical-align: -1px; margin-right: 5px; }}
.details details {{ border-top: 1px solid {HAIRLINE}; }}
.details details:last-child {{ border-bottom: 1px solid {HAIRLINE}; }}
.details summary {{ padding: 11px 2px; display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; }}
.details summary:hover {{ background: rgba(26,25,23,0.025); }}
.dn {{ font-weight: 650; color: {INK}; font-size: 13.5px; }}
.dsc {{ color: {INK_2}; }}
.badge {{ font-size: 10px; font-weight: 700; letter-spacing: 0.07em; text-transform: uppercase; border-radius: 9px; padding: 2px 9px; white-space: nowrap; }}
.badge.k {{ background: {ACCENT}; color: {ON_ACCENT}; }}
.badge.d {{ border: 1px solid {BADGE_BORDER}; color: {INK_2}; }}
.badge.r {{ background: {HAIRLINE}; color: {FAIL_FILL}; }}
.dbody {{ padding: 6px 2px 22px 18px; }}
.verdict {{ font-size: 13px; color: {INK_2}; margin-bottom: 14px; max-width: 90ch; }}
.verdict b {{ color: {INK}; font-weight: 650; }}
.cols2 {{ display: flex; flex-wrap: wrap; gap: 20px 44px; }}
.cols2 > div {{ flex: 1 1 320px; min-width: 280px; }}
h4 {{ font-size: 11px; font-weight: 650; letter-spacing: 0.06em; text-transform: uppercase; color: {MUTED}; margin: 12px 0 6px; }}
.agg {{ font-size: 13px; margin-bottom: 5px; }}
.files {{ list-style: none; }}
.files li {{ font: 12px/1.7 ui-monospace, SFMono-Regular, Menlo, monospace; color: {INK_2}; }}
table {{ border-collapse: collapse; font-size: 12.5px; width: 100%; }}
th {{ text-align: left; font-size: 10.5px; font-weight: 650; letter-spacing: 0.05em; text-transform: uppercase; color: {MUTED}; padding: 3px 14px 3px 0; border-bottom: 1px solid {HAIRLINE}; }}
td {{ padding: 4px 14px 4px 0; border-bottom: 1px solid {HAIRLINE}; vertical-align: top; color: {INK_2}; }}
td.name {{ color: {INK}; font-weight: 600; white-space: nowrap; }}
td.res {{ white-space: nowrap; }}
td.res.fail {{ color: {INK}; font-weight: 650; }}
td.res.skip {{ color: {MUTED}; font-style: italic; }}
td.num {{ font-variant-numeric: tabular-nums; white-space: nowrap; }}
td.cmd {{ font: 12px/1.6 ui-monospace, SFMono-Regular, Menlo, monospace; }}
.tl {{ margin-top: 4px; }}
.win {{ font-size: 12px; color: {MUTED}; margin-top: 10px; }}
footer {{ margin-top: 42px; padding-top: 16px; border-top: 1px solid {HAIRLINE}; font-size: 13px; color: {INK_2}; }}
footer div {{ margin-bottom: 4px; }}
footer a {{ color: {ACCENT_DEEP}; }}
.mut {{ color: {MUTED}; font-size: 12px; }}
.col {{ cursor: pointer; }}
.col .hit {{ fill: transparent; }}
.col:hover .hit {{ fill: rgba(26,25,23,0.035); }}
.col:focus-visible {{ outline: 2px solid {ACCENT}; outline-offset: 2px; }}
@media print {{
  body {{ background: #fff; }}
  .hint {{ display: none; }}
  .hero-card {{ border: none; padding: 0; overflow: visible; }}
}}"##,
        root_vars()
    )
}

// ---------------------------------------------------------------------------
// Header strip
// ---------------------------------------------------------------------------

fn header_strip(out: &mut String, m: &FlowModel) {
    out.push_str("<header>\n<p class=\"eyebrow\">crucible run</p>\n");
    // The goal line arrives as "Goal: …"; the eyebrow already frames it.
    let goal = m
        .run
        .goal_line
        .strip_prefix("Goal:")
        .map(str::trim)
        .unwrap_or(&m.run.goal_line);
    w!(out, "<h1>{}</h1>\n", md_code(goal));
    out.push_str("<div class=\"stats\">\n");

    // Result tile (the hero number).
    let dirp = direction_phrase(m.run.direction.as_deref());
    let result_lbl = match dirp {
        Some(p) => format!("{} — {p}", esc(&m.run.gate)),
        None => esc(&m.run.gate),
    };
    match (m.run.baseline_score, m.run.best_score) {
        (Some(b), Some(best)) => {
            w!(
                out,
                "<div class=\"tile wide\"><div class=\"lbl\">{result_lbl}</div>\
                 <div class=\"val\">{} <span class=\"arr\">→</span> {}</div>",
                score3(b),
                score3(best)
            );
            if m.run.improved && b != 0.0 {
                let delta = (best - b) / b * 100.0;
                let kept_at = m
                    .iterations
                    .iter()
                    .rev()
                    .find(|i| i.decision == "keep")
                    .map(|i| format!(" · kept at iteration {}", i.n))
                    .unwrap_or_default();
                w!(
                    out,
                    "<div class=\"sub good\">{}{kept_at}</div>",
                    delta_phrase(m.run.direction.as_deref(), delta)
                );
            } else if !m.run.improved {
                out.push_str("<div class=\"sub\">no candidate kept</div>");
            }
            out.push_str("</div>\n");
        }
        (Some(b), None) => {
            w!(
                out,
                "<div class=\"tile wide\"><div class=\"lbl\">{result_lbl}</div>\
                 <div class=\"val\">{}</div><div class=\"sub\">baseline only</div></div>\n",
                score3(b)
            );
        }
        _ => {
            w!(
                out,
                "<div class=\"tile wide\"><div class=\"lbl\">{result_lbl}</div>\
                 <div class=\"val sm\">no scores recorded</div></div>\n"
            );
        }
    }

    // Iterations tile.
    let (mut kept, mut disc, mut rej) = (0usize, 0usize, 0usize);
    for it in &m.iterations {
        match it.decision.as_str() {
            "keep" => kept += 1,
            "rejected" => rej += 1,
            _ => disc += 1,
        }
    }
    w!(
        out,
        "<div class=\"tile\"><div class=\"lbl\">iterations</div><div class=\"val\">{}</div>\
         <div class=\"sub\">{kept} kept · {disc} discarded · {rej} rejected</div></div>\n",
        m.iterations.len()
    );

    w!(
        out,
        "<div class=\"tile\"><div class=\"lbl\">model</div><div class=\"val sm\">{}</div></div>\n",
        esc(&m.run.model)
    );

    if let Some(cost) = m.run.cost_usd {
        out.push_str("<div class=\"tile\"><div class=\"lbl\">cost</div><div class=\"val\">");
        w!(out, "${cost:.2}</div><div class=\"sub\">");
        spend_spark(out, m);
        out.push_str("spend over the run</div></div>\n");
    }
    if let Some(el) = m.run.elapsed_secs {
        w!(
            out,
            "<div class=\"tile\"><div class=\"lbl\">wall-clock</div><div class=\"val\">{}</div></div>\n",
            fmt_dur(el as f64)
        );
    }
    let outcome = m.run.outcome.as_deref().unwrap_or("interrupted");
    w!(
        out,
        "<div class=\"tile\"><div class=\"lbl\">outcome</div><div class=\"val sm\">{}</div>",
        esc(outcome)
    );
    if m.run.outcome.is_none() {
        out.push_str("<div class=\"sub\">log ends without a shutdown record</div>");
    } else if !m.publish.prs.is_empty() {
        out.push_str("<div class=\"sub\">draft PR opened</div>");
    }
    out.push_str("</div>\n</div>\n</header>\n");
}

/// Tiny cumulative-spend sparkline inside the cost tile.
fn spend_spark(out: &mut String, m: &FlowModel) {
    let Some(last) = m.budget.last() else { return };
    let (max_t, max_s) = (last.at_s.max(1) as f64, last.spent.max(f64::MIN_POSITIVE));
    let (sw, sh) = (86.0, 22.0);
    let mut pts = String::from("0.0,22.0");
    for b in &m.budget {
        w!(
            pts,
            " {:.1},{:.1}",
            b.at_s as f64 / max_t * sw,
            sh - (b.spent / max_s * (sh - 2.0))
        );
    }
    w!(
        out,
        "<svg class=\"spark\" width=\"{sw:.0}\" height=\"{sh:.0}\" viewBox=\"0 0 {sw:.0} {sh:.0}\" \
         role=\"img\" aria-label=\"cumulative spend\">\
         <polyline points=\"{pts}\" fill=\"none\" stroke=\"{ACCENT}\" stroke-width=\"1.5\"/>\
         <circle cx=\"{sw:.1}\" cy=\"2\" r=\"2.5\" fill=\"{ACCENT}\"/></svg>"
    );
}

// ---------------------------------------------------------------------------
// Preflight fold
// ---------------------------------------------------------------------------

fn preflight(out: &mut String, m: &FlowModel) {
    if m.preflight.is_empty() {
        return;
    }
    let failed = m.preflight.iter().filter(|p| p.outcome != "ok").count();
    let summary = if failed == 0 {
        format!(
            "Before iteration 1 — {} environment checks passed, baseline measured",
            m.preflight.len()
        )
    } else {
        format!(
            "Before iteration 1 — {failed} of {} environment checks failed",
            m.preflight.len()
        )
    };
    w!(
        out,
        "<details class=\"pre\"><summary>{}</summary>\n",
        esc(&summary)
    );
    out.push_str("<table><tr><th>command</th><th>result</th><th>note</th></tr>\n");
    for p in &m.preflight {
        let res = if p.outcome == "ok" {
            "passed"
        } else {
            "failed"
        };
        let cls = if p.outcome == "ok" { "res" } else { "res fail" };
        w!(
            out,
            "<tr><td class=\"cmd\">{}</td><td class=\"{cls}\">{res}</td><td>{}</td></tr>\n",
            esc(&p.cmd_short),
            esc(&fmt_note(&p.note))
        );
    }
    out.push_str("</table></details>\n");
}

// ---------------------------------------------------------------------------
// The hero swimlane
// ---------------------------------------------------------------------------

struct Col {
    x: f64,
    w: f64,
    /// Iteration wall-clock seconds; only in time-proportional mode.
    dur: Option<f64>,
}

/// Column layout: real time-proportional widths when every iteration has a span
/// window, equal widths otherwise.
fn layout(m: &FlowModel) -> (Vec<Col>, bool) {
    let n = m.iterations.len();
    if n == 0 {
        return (Vec::new(), false);
    }
    let avail = PLOT_W - COL_GAP * (n - 1) as f64;
    let durs: Option<Vec<f64>> = m
        .iterations
        .iter()
        .map(|it| {
            let (s, e) = (
                parse_ts(it.started_at.as_ref()),
                parse_ts(it.ended_at.as_ref()),
            );
            match (s, e) {
                (Some(s), Some(e)) if e > s => {
                    Some((e.as_nanosecond() - s.as_nanosecond()) as f64 / 1e9)
                }
                _ => None,
            }
        })
        .collect();
    let widths: Vec<f64> = match &durs {
        Some(d) => proportional_widths(d, avail),
        None => vec![avail / n as f64; n],
    };
    let mut cols = Vec::with_capacity(n);
    let mut x = GUTTER;
    for (i, w) in widths.iter().enumerate() {
        cols.push(Col {
            x,
            w: *w,
            dur: durs.as_ref().map(|d| d[i]),
        });
        x += w + COL_GAP;
    }
    (cols, durs.is_some())
}

/// Duration-proportional widths with a minimum so a short iteration stays clickable.
/// Undersized columns pin to the minimum; the rest re-share the remaining space.
fn proportional_widths(durs: &[f64], avail: f64) -> Vec<f64> {
    let n = durs.len();
    let mut widths = vec![0.0; n];
    let mut pinned = vec![false; n];
    for _ in 0..n {
        let pinned_w: f64 = MIN_COL * pinned.iter().filter(|p| **p).count() as f64;
        let free_d: f64 = durs
            .iter()
            .zip(&pinned)
            .filter(|(_, p)| !**p)
            .map(|(d, _)| *d)
            .sum();
        if free_d <= 0.0 {
            break;
        }
        let free_avail = (avail - pinned_w).max(0.0);
        let mut newly_pinned = false;
        for i in 0..n {
            if pinned[i] {
                widths[i] = MIN_COL;
                continue;
            }
            widths[i] = free_avail * durs[i] / free_d;
            if widths[i] < MIN_COL {
                pinned[i] = true;
                newly_pinned = true;
            }
        }
        if !newly_pinned {
            break;
        }
    }
    widths
}

/// Union of check names across iterations, first-appearance order.
fn lane_names(m: &FlowModel) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for it in &m.iterations {
        for r in &it.rungs {
            if !names.contains(&r.name) {
                names.push(r.name.clone());
            }
        }
    }
    names
}

fn hero(out: &mut String, m: &FlowModel) {
    out.push_str("<section>\n<h2>The run, left to right</h2>\n");
    if m.iterations.is_empty() {
        out.push_str("<p class=\"hint\">No iterations were recorded.</p>\n</section>\n");
        return;
    }
    let (cols, timed) = layout(m);
    let width_note = if timed {
        // The header's wall-clock counts preflight and post-run too; say so or the
        // axis end looks hours short of it.
        "column width is real wall-clock time (the chart covers the iterations only — \
         preflight and post-run time sit outside it)"
    } else {
        "columns are equal width (no span timings)"
    };
    w!(
        out,
        "<p class=\"hint\">Each column is one iteration: the agent writes code, then frozen \
         checks grade the candidate — {width_note}. Click a column for its detail.</p>\n"
    );
    out.push_str("<div class=\"hero-card\">\n");

    let lanes = lane_names(m);
    let checks_label_y = AGENT_TOP + AGENT_H + 24.0;
    let checks_top = checks_label_y + 8.0;
    let lanes_end = if lanes.is_empty() {
        AGENT_TOP + AGENT_H
    } else {
        checks_top + lanes.len() as f64 * LANE_STEP - (LANE_STEP - LANE_H)
    };
    let axis_y = lanes_end + 20.0;
    let svg_h = if timed {
        axis_y + 26.0
    } else {
        lanes_end + 16.0
    };

    w!(
        out,
        "<svg viewBox=\"0 0 {SVG_W:.0} {svg_h:.0}\" role=\"img\" \
         aria-label=\"swimlane of {} iterations: agent turns, checks, and scores over time\">\n",
        m.iterations.len()
    );

    // Kept-column wash first, under everything.
    for (it, c) in m.iterations.iter().zip(&cols) {
        if it.decision == "keep" {
            w!(
                out,
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"8\" \
                 fill=\"{ACCENT_WASH}\" stroke=\"rgba(42,120,214,0.30)\"/>\n",
                c.x - 3.0,
                HDR_TOP - 4.0,
                c.w + 6.0,
                lanes_end - HDR_TOP + 12.0
            );
        }
    }

    gutter_labels(out, m, &lanes, checks_label_y, checks_top);
    score_strip(out, m, &cols);
    for (it, c) in m.iterations.iter().zip(&cols) {
        column(out, it, c, &lanes, checks_top, lanes_end, timed);
    }
    if timed {
        time_axis(out, m, &cols, axis_y);
    }
    out.push_str("</svg>\n</div>\n");
    out.push_str(
        "<p class=\"legend\"><span><span class=\"sw turn\"></span>agent writes code</span>\
         <span><span class=\"sw pass\"></span>check passed</span>\
         <span><span class=\"sw fail\"></span>check failed ✕</span>\
         <span><span class=\"dotm\"></span>check skipped or instant</span></p>\n",
    );
    out.push_str("</section>\n");
}

fn gutter_labels(out: &mut String, m: &FlowModel, lanes: &[String], label_y: f64, checks_top: f64) {
    let rx = GUTTER - 16.0;
    // Score strip label: the gate name + direction, top-left of the strip.
    w!(
        out,
        "<text x=\"{rx:.0}\" y=\"{:.0}\" text-anchor=\"end\" font-size=\"11\" font-weight=\"650\" fill=\"{INK}\">{}</text>\n",
        SCORE_TOP + 18.0,
        esc(&m.run.gate)
    );
    if let Some(p) = direction_phrase(m.run.direction.as_deref()) {
        w!(
            out,
            "<text x=\"{rx:.0}\" y=\"{:.0}\" text-anchor=\"end\" font-size=\"10\" fill=\"{MUTED}\">{p}</text>\n",
            SCORE_TOP + 32.0
        );
    }
    w!(
        out,
        "<text x=\"{rx:.0}\" y=\"{:.0}\" text-anchor=\"end\" font-size=\"11\" font-weight=\"650\" fill=\"{INK}\">agent writes code</text>\n",
        AGENT_TOP + AGENT_H / 2.0 + 4.0
    );
    if !lanes.is_empty() {
        w!(
            out,
            "<text x=\"{rx:.0}\" y=\"{label_y:.0}\" text-anchor=\"end\" font-size=\"9.5\" \
             font-weight=\"650\" letter-spacing=\"0.7\" fill=\"{MUTED}\">FROZEN CHECKS</text>\n"
        );
    }
    for (i, name) in lanes.iter().enumerate() {
        w!(
            out,
            "<text x=\"{rx:.0}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"10.5\" fill=\"{INK_2}\">{}</text>\n",
            checks_top + i as f64 * LANE_STEP + LANE_H / 2.0 + 3.5,
            esc(name)
        );
    }
}

fn score_strip(out: &mut String, m: &FlowModel, cols: &[Col]) {
    let mut vals: Vec<f64> = m.iterations.iter().filter_map(|i| i.score).collect();
    if let Some(b) = m.run.baseline_score {
        vals.push(b);
    }
    let (Some(min), Some(max)) = (
        vals.iter().copied().reduce(f64::min),
        vals.iter().copied().reduce(f64::max),
    ) else {
        return;
    };
    let span = (max - min).max(1e-9);
    let pad = span * 0.14;
    let (lo, hi) = (min - pad, max + pad);
    let (top, bot) = (SCORE_TOP + 12.0, SCORE_TOP + SCORE_H - 12.0);
    let y_of = |v: f64| top + (hi - v) / (hi - lo) * (bot - top);

    // Baseline reference line + its label at the line's left end.
    if let Some(b) = m.run.baseline_score {
        let by = y_of(b);
        w!(
            out,
            "<line x1=\"{GUTTER:.0}\" y1=\"{by:.1}\" x2=\"{:.1}\" y2=\"{by:.1}\" stroke=\"{LINE}\" stroke-width=\"1\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"{MUTED}\">baseline {}</text>\n",
            GUTTER + PLOT_W,
            GUTTER + 2.0,
            by - 5.0,
            score3(b)
        );
    }

    // Trajectory: connect consecutive measured iterations; a rejected one leaves a gap.
    let pts: Vec<(usize, f64, f64)> = m
        .iterations
        .iter()
        .enumerate()
        .filter_map(|(i, it)| it.score.map(|s| (i, cols[i].x + cols[i].w / 2.0, y_of(s))))
        .collect();
    for w2 in pts.windows(2) {
        if w2[1].0 == w2[0].0 + 1 {
            w!(
                out,
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{LINE_SOFT}\" stroke-width=\"1.5\"/>\n",
                w2[0].1,
                w2[0].2,
                w2[1].1,
                w2[1].2
            );
        }
    }

    for (i, it) in m.iterations.iter().enumerate() {
        let cx = cols[i].x + cols[i].w / 2.0;
        match it.score {
            Some(s) => {
                let cy = y_of(s);
                if it.decision == "keep" {
                    w!(
                        out,
                        "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"5.5\" fill=\"{ACCENT}\" stroke=\"{SURFACE}\" stroke-width=\"2\"/>\n"
                    );
                    let label = match it.delta_pct_vs_baseline {
                        Some(d) => format!(
                            "{} · {}",
                            score3(s),
                            delta_phrase(m.run.direction.as_deref(), d)
                        ),
                        None => score3(s),
                    };
                    let ly = if cy - top > 20.0 {
                        cy - 11.0
                    } else {
                        cy + 20.0
                    };
                    w!(
                        out,
                        "<text x=\"{cx:.1}\" y=\"{ly:.1}\" text-anchor=\"middle\" font-size=\"11\" font-weight=\"650\" fill=\"{INK}\">{}</text>\n",
                        esc(&label)
                    );
                } else {
                    w!(
                        out,
                        "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"4.5\" fill=\"{DOT_GRAY}\" stroke=\"{SURFACE}\" stroke-width=\"2\"/>\n"
                    );
                }
            }
            // No measurement happened; mark the gap rather than inventing a point.
            None => {
                w!(
                    out,
                    "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"11\" fill=\"{DOT_GRAY}\">✕</text>\n\
                     <text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"9\" fill=\"{MUTED}\">not measured</text>\n",
                    bot - 8.0,
                    bot + 4.0
                );
            }
        }
    }
}

fn column(
    out: &mut String,
    it: &Iteration,
    c: &Col,
    lanes: &[String],
    checks_top: f64,
    lanes_end: f64,
    timed: bool,
) {
    let cx = c.x + c.w / 2.0;
    w!(
        out,
        "<g class=\"col\" data-iter=\"{}\" role=\"button\" tabindex=\"0\" aria-expanded=\"false\" \
         aria-label=\"iteration {} — {}: open detail\">\n",
        it.n,
        it.n,
        decision_word(&it.decision)
    );

    // Header: iteration id (+ real duration), score, decision badge.
    let id_line = match c.dur {
        Some(d) => format!("iter {} · {}", it.n, fmt_dur(d)),
        None => format!("iter {}", it.n),
    };
    w!(
        out,
        "<text x=\"{cx:.1}\" y=\"{:.0}\" text-anchor=\"middle\" font-size=\"10\" fill=\"{MUTED}\">{}</text>\n",
        HDR_TOP + 10.0,
        esc(&id_line)
    );
    match it.score {
        Some(s) => {
            let delta = it
                .delta_pct_vs_baseline
                .map(|d| format!(" {d:+.2}%"))
                .unwrap_or_default();
            w!(
                out,
                "<text x=\"{cx:.1}\" y=\"{:.0}\" text-anchor=\"middle\" font-size=\"13\" font-weight=\"650\" fill=\"{INK}\">{}\
                 <tspan font-size=\"10\" font-weight=\"400\" fill=\"{MUTED}\">{delta}</tspan></text>\n",
                HDR_TOP + 28.0,
                score3(s)
            );
        }
        None => {
            w!(
                out,
                "<text x=\"{cx:.1}\" y=\"{:.0}\" text-anchor=\"middle\" font-size=\"10.5\" font-style=\"italic\" fill=\"{MUTED}\">not scored</text>\n",
                HDR_TOP + 28.0
            );
        }
    }
    badge(out, it, cx, HDR_TOP + 36.0);

    // Agent-turn band. In timed mode its width is the turn's real share of the column.
    let turn_frac = match (c.dur, it.turn.duration_s) {
        (Some(d), Some(t)) if d > 0.0 => Some((t / d).clamp(0.0, 1.0)),
        _ => None,
    };
    let (bx, bw) = match turn_frac {
        Some(f) if timed => (c.x, (c.w * f).max(3.0)),
        _ => (c.x + 4.0, (c.w - 8.0).max(3.0)),
    };
    w!(
        out,
        "<rect x=\"{bx:.1}\" y=\"{:.1}\" width=\"{bw:.1}\" height=\"24\" rx=\"3\" fill=\"{TURN_FILL}\"/>\n",
        AGENT_TOP + (AGENT_H - 24.0) / 2.0
    );
    if timed && !it.turn.tool_calls.is_empty() {
        let label = count_noun(it.turn.tool_calls.len(), "call");
        if text_w(&label, 9.5) < c.w - bw - 8.0 {
            w!(
                out,
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"9.5\" fill=\"{MUTED}\">{label}</text>\n",
                bx + bw + 5.0,
                AGENT_TOP + AGENT_H / 2.0 + 3.5
            );
        }
    }

    // Check bands, laid out sequentially after the turn when real durations exist.
    let mut cursor = match turn_frac {
        Some(f) if timed => f * c.w,
        _ => 0.0,
    };
    for r in &it.rungs {
        let Some(lane) = lanes.iter().position(|n| *n == r.name) else {
            continue;
        };
        let ly = checks_top + lane as f64 * LANE_STEP + (LANE_STEP - LANE_H) / 2.0 - 3.5;
        let band_fill = if r.pass { PASS_FILL } else { FAIL_FILL };
        if r.skipped {
            // Never ran: a hollow marker in either mode, never a band.
            w!(
                out,
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3.5\" fill=\"{SURFACE}\" stroke=\"{DOT_GRAY}\" stroke-width=\"1.5\"/>\n",
                c.x + cursor + 6.0,
                ly + LANE_H / 2.0
            );
            continue;
        }
        if timed {
            match (r.duration_s, c.dur) {
                (Some(d), Some(total)) if total > 0.0 => {
                    let bw = (d / total * c.w)
                        .max(3.0)
                        .min((c.w - cursor - 1.0).max(2.5));
                    w!(
                        out,
                        "<rect x=\"{:.1}\" y=\"{ly:.1}\" width=\"{bw:.1}\" height=\"{LANE_H:.0}\" rx=\"3\" fill=\"{band_fill}\"/>\n",
                        c.x + cursor
                    );
                    if !r.pass {
                        fail_mark(out, c.x + cursor + bw / 2.0, ly);
                    }
                    cursor += d / total * c.w;
                }
                _ if r.pass => {
                    // Ran with no measurable duration (no matching span, or decided
                    // instantly): a hollow marker, not a fabricated band.
                    w!(
                        out,
                        "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3.5\" fill=\"{SURFACE}\" stroke=\"{DOT_GRAY}\" stroke-width=\"1.5\"/>\n",
                        c.x + cursor + 6.0,
                        ly + LANE_H / 2.0
                    );
                }
                _ => {
                    // A failure with no span (e.g. the synthesized pre-measure
                    // rejection): fixed-width mark at the point it fired.
                    let bw = 16.0_f64.min(c.w - cursor - 1.0).max(10.0);
                    w!(
                        out,
                        "<rect x=\"{:.1}\" y=\"{ly:.1}\" width=\"{bw:.1}\" height=\"{LANE_H:.0}\" rx=\"3\" fill=\"{band_fill}\"/>\n",
                        c.x + cursor
                    );
                    fail_mark(out, c.x + cursor + bw / 2.0, ly);
                }
            }
        } else {
            let bw = (c.w - 8.0).max(3.0);
            w!(
                out,
                "<rect x=\"{:.1}\" y=\"{ly:.1}\" width=\"{bw:.1}\" height=\"{LANE_H:.0}\" rx=\"3\" fill=\"{band_fill}\"/>\n",
                c.x + 4.0
            );
            if !r.pass {
                fail_mark(out, cx, ly);
            }
        }
    }

    // Full-column hit target, last so it sits on top.
    w!(
        out,
        "<rect class=\"hit\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"6\"/>\n</g>\n",
        c.x - 2.0,
        HDR_TOP - 4.0,
        c.w + 4.0,
        lanes_end - HDR_TOP + 12.0
    );
}

fn fail_mark(out: &mut String, cx: f64, band_y: f64) {
    w!(
        out,
        "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"11\" font-weight=\"700\" fill=\"{SURFACE}\">✕</text>\n",
        band_y + LANE_H / 2.0 + 3.5
    );
}

fn badge(out: &mut String, it: &Iteration, cx: f64, y: f64) {
    let (label, fill, stroke, text_fill) = match it.decision.as_str() {
        "keep" => ("✓ KEPT", ACCENT, "none", ON_ACCENT),
        "rejected" => ("✕ REJECTED", HAIRLINE, "none", FAIL_FILL),
        _ => ("DISCARDED", "none", BADGE_BORDER, INK_2),
    };
    let bw = text_w(label, 9.0) + 18.0;
    w!(
        out,
        "<rect x=\"{:.1}\" y=\"{y:.1}\" width=\"{bw:.1}\" height=\"15\" rx=\"7.5\" fill=\"{fill}\" stroke=\"{stroke}\"/>\n\
         <text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"9\" font-weight=\"700\" letter-spacing=\"0.5\" fill=\"{text_fill}\">{label}</text>\n",
        cx - bw / 2.0,
        y + 11.0
    );
}

fn time_axis(out: &mut String, m: &FlowModel, cols: &[Col], axis_y: f64) {
    let Some(t0) = parse_ts(m.iterations.first().and_then(|i| i.started_at.as_ref())) else {
        return;
    };
    let elapsed = |ts: &jiff::Timestamp| (ts.as_nanosecond() - t0.as_nanosecond()) as f64 / 1e9;
    w!(
        out,
        "<line x1=\"{GUTTER:.0}\" y1=\"{axis_y:.1}\" x2=\"{:.1}\" y2=\"{axis_y:.1}\" stroke=\"{HAIRLINE}\" stroke-width=\"1\"/>\n\
         <text x=\"{:.0}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"9.5\" fill=\"{MUTED}\">elapsed since iter 1 (h:mm)</text>\n",
        GUTTER + PLOT_W,
        GUTTER - 16.0,
        axis_y + 14.0
    );
    for (it, c) in m.iterations.iter().zip(cols) {
        if let Some(ts) = parse_ts(it.started_at.as_ref()) {
            tick(out, c.x, axis_y, &fmt_hm(elapsed(&ts)));
        }
    }
    if let (Some(last), Some(c)) = (m.iterations.last(), cols.last())
        && let Some(ts) = parse_ts(last.ended_at.as_ref())
    {
        tick(out, c.x + c.w, axis_y, &fmt_hm(elapsed(&ts)));
    }
}

fn tick(out: &mut String, x: f64, axis_y: f64, label: &str) {
    w!(
        out,
        "<line x1=\"{x:.1}\" y1=\"{axis_y:.1}\" x2=\"{x:.1}\" y2=\"{:.1}\" stroke=\"{LINE}\" stroke-width=\"1\"/>\n\
         <text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"9.5\" fill=\"{MUTED}\">{label}</text>\n",
        axis_y + 4.0,
        axis_y + 14.0
    );
}

// ---------------------------------------------------------------------------
// Per-iteration detail
// ---------------------------------------------------------------------------

fn details(out: &mut String, m: &FlowModel) {
    if m.iterations.is_empty() {
        return;
    }
    out.push_str(
        "<section class=\"details\">\n<h2>Iteration detail</h2>\n\
         <p class=\"hint\">Click a column above, or expand an iteration here.</p>\n",
    );
    for it in &m.iterations {
        let open = if it.decision == "keep" { " open" } else { "" };
        let badge_cls = match it.decision.as_str() {
            "keep" => "k",
            "rejected" => "r",
            _ => "d",
        };
        let mut desc = String::new();
        match (it.score, it.delta_pct_vs_baseline) {
            (Some(s), Some(d)) => {
                w!(
                    desc,
                    "{} · {} vs baseline",
                    score3(s),
                    delta_phrase(m.run.direction.as_deref(), d)
                );
            }
            (Some(s), None) => desc.push_str(&score3(s)),
            _ => desc.push_str("rejected before measuring"),
        }
        if it.edits.files_changed > 0 {
            w!(
                desc,
                " · {} changed",
                count_noun(it.edits.files_changed, "file")
            );
        }
        w!(
            out,
            "<details id=\"it-{}\"{open}><summary><span class=\"dn\">Iteration {}</span>\
             <span class=\"badge {badge_cls}\">{}</span><span class=\"dsc\">{}</span></summary>\n\
             <div class=\"dbody\">\n",
            it.n,
            it.n,
            decision_word(&it.decision),
            esc(&desc)
        );
        if !it.note_short.is_empty() {
            w!(
                out,
                "<p class=\"verdict\"><b>verdict:</b> {}</p>\n",
                esc(&fmt_note(&it.note_short))
            );
        }
        out.push_str("<div class=\"cols2\">\n<div>\n<h4>Code changes</h4>\n");
        if it.edits.files_changed == 0 {
            out.push_str("<p class=\"agg mut\">no edits recorded</p>\n");
        } else {
            w!(
                out,
                "<p class=\"agg\">{} · +{} −{}</p>\n<ul class=\"files\">\n",
                count_noun(it.edits.files_changed, "file"),
                it.edits.insertions,
                it.edits.deletions
            );
            for f in &it.edits.files {
                w!(out, "<li>{}</li>\n", esc(f));
            }
            out.push_str("</ul>\n");
        }
        out.push_str("</div>\n<div>\n<h4>Checks</h4>\n");
        if it.rungs.is_empty() {
            out.push_str("<p class=\"agg mut\">no checks ran</p>\n");
        } else {
            out.push_str(
                "<table><tr><th>check</th><th>result</th><th>time</th><th>note</th></tr>\n",
            );
            for r in &it.rungs {
                let (res, cls) = if r.skipped {
                    ("skipped", "res skip")
                } else if r.pass {
                    ("passed", "res")
                } else {
                    ("failed ✕", "res fail")
                };
                let dur = r.duration_s.map(fmt_dur).unwrap_or_else(|| "—".into());
                w!(
                    out,
                    "<tr><td class=\"name\">{}</td><td class=\"{cls}\">{res}</td>\
                     <td class=\"num\">{dur}</td><td>{}</td></tr>\n",
                    esc(&r.name),
                    esc(&fmt_note(&r.note_short))
                );
            }
            out.push_str("</table>\n");
        }
        out.push_str("</div>\n</div>\n");
        tool_timeline(out, it);
        window_line(out, it);
        out.push_str("</div>\n</details>\n");
    }
    out.push_str("</section>\n");
}

/// The agent's in-turn actions, one row per tool, marks at their real offsets.
fn tool_timeline(out: &mut String, it: &Iteration) {
    if it.turn.tool_calls.is_empty() {
        if let Some(d) = it.turn.duration_s {
            w!(out, "<p class=\"win\">agent turn: {}</p>\n", fmt_dur(d));
        }
        return;
    }
    // Rows in first-use order; a long tail folds into "other". Broker (mcp__*) calls are the
    // agent↔measurement interplay a reader came to see, so they never fold.
    struct ToolRow {
        name: String,
        broker: bool,
        calls: Vec<(f64, f64)>,
    }
    let mut rows: Vec<ToolRow> = Vec::new();
    for c in &it.turn.tool_calls {
        let name = short_tool(&c.tool).to_string();
        let broker = c.tool.starts_with("mcp__");
        match rows.iter_mut().find(|r| r.name == name) {
            Some(r) => r.calls.push((c.at_s, c.duration_s)),
            None => rows.push(ToolRow {
                name,
                broker,
                calls: vec![(c.at_s, c.duration_s)],
            }),
        }
    }
    if rows.len() > 9 {
        let (keep_tail, fold_tail): (Vec<_>, Vec<_>) =
            rows.split_off(8).into_iter().partition(|r| r.broker);
        rows.extend(keep_tail);
        if !fold_tail.is_empty() {
            let mut merged: Vec<(f64, f64)> = fold_tail.into_iter().flat_map(|r| r.calls).collect();
            merged.sort_by(|a, b| a.0.total_cmp(&b.0));
            rows.push(ToolRow {
                name: "other".into(),
                broker: false,
                calls: merged,
            });
        }
    }
    let last_end = it
        .turn
        .tool_calls
        .iter()
        .map(|c| c.at_s + c.duration_s)
        .fold(0.0, f64::max);
    let total = it.turn.duration_s.unwrap_or(0.0).max(last_end).max(1.0);

    let (vw, gutter, row_h) = (1000.0, 150.0, 19.0);
    let plot_w = vw - gutter - 14.0;
    let axis = rows.len() as f64 * row_h + 8.0;
    let vh = axis + 18.0;
    w!(
        out,
        "<h4>Agent activity — {} tool calls over {}</h4>\n\
         <svg class=\"tl\" viewBox=\"0 0 {vw:.0} {vh:.0}\" role=\"img\" \
         aria-label=\"tool call timeline for iteration {}\">\n",
        it.turn.tool_calls.len(),
        fmt_dur(total),
        it.n
    );
    for (i, row) in rows.iter().enumerate() {
        let (name, calls) = (&row.name, &row.calls);
        let y = i as f64 * row_h;
        w!(
            out,
            "<text x=\"{:.0}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"11\" fill=\"{INK_2}\">{} · {}</text>\n",
            gutter - 10.0,
            y + 13.0,
            esc(name),
            calls.len()
        );
        for (at, dur) in calls {
            w!(
                out,
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"9\" rx=\"1.5\" fill=\"rgba(42,120,214,0.55)\"/>\n",
                gutter + at / total * plot_w,
                y + 5.0,
                (dur / total * plot_w).max(1.6)
            );
        }
    }
    w!(
        out,
        "<line x1=\"{gutter:.0}\" y1=\"{axis:.1}\" x2=\"{:.1}\" y2=\"{axis:.1}\" stroke=\"{HAIRLINE}\" stroke-width=\"1\"/>\n\
         <text x=\"{gutter:.0}\" y=\"{:.1}\" font-size=\"10\" fill=\"{MUTED}\">0</text>\n\
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"10\" fill=\"{MUTED}\">{}</text>\n</svg>\n",
        gutter + plot_w,
        axis + 13.0,
        gutter + plot_w,
        axis + 13.0,
        fmt_dur(total)
    );
}

fn window_line(out: &mut String, it: &Iteration) {
    let (Some(s), Some(e)) = (
        parse_ts(it.started_at.as_ref()),
        parse_ts(it.ended_at.as_ref()),
    ) else {
        return;
    };
    let dur = (e.as_nanosecond() - s.as_nanosecond()) as f64 / 1e9;
    w!(
        out,
        "<p class=\"win\">{} → {} UTC · iteration wall-clock {}</p>\n",
        clock(&s),
        clock(&e),
        fmt_dur(dur)
    );
}

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------

fn footer(out: &mut String, m: &FlowModel) {
    out.push_str("<footer>\n");
    if m.publish.prs.is_empty() {
        // A run can keep candidates and still record no PR (the open failed, or publish never
        // ran); "nothing to publish" is only true when nothing was kept.
        if m.run.improved {
            out.push_str("<div>No draft PR recorded — kept commits exist but no PR link reached the session log.</div>\n");
        } else {
            out.push_str("<div>No draft PR — no kept candidate to publish.</div>\n");
        }
    } else {
        for pr in &m.publish.prs {
            w!(
                out,
                "<div>Draft PR with the kept commits: <a href=\"{}\">{}</a> \
                 <span class=\"mut\">branch {}</span></div>\n",
                esc(&pr.url),
                esc(&pr_short(&pr.url)),
                esc(&pr.branch)
            );
        }
    }
    for e in &m.epilogue {
        let verdict = if e.pass { "passed" } else { "failed" };
        w!(
            out,
            "<div>Post-run check: {} — {verdict}</div>\n",
            esc(&fmt_note(&e.note))
        );
    }
    if let Some(id) = &m.run.id {
        w!(out, "<div class=\"mut\">run record {}</div>\n", esc(id));
    }
    out.push_str("<div class=\"mut\">generated by crucible flow</div>\n</footer>\n</main>\n");
}

/// Column click/keyboard toggle + open-everything on print. The page works without it.
const SCRIPT: &str = r#"for (const g of document.querySelectorAll('[data-iter]')) {
  const toggle = () => {
    const d = document.getElementById('it-' + g.dataset.iter);
    if (!d) return;
    const was = d.open;
    for (const x of document.querySelectorAll('.details details')) x.open = false;
    d.open = !was;
    for (const c of document.querySelectorAll('[data-iter]'))
      c.setAttribute('aria-expanded', 'false');
    g.setAttribute('aria-expanded', String(d.open));
    if (d.open) d.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  };
  g.addEventListener('click', toggle);
  g.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggle(); }
  });
}
addEventListener('beforeprint', () => {
  for (const d of document.querySelectorAll('details')) d.open = true;
});
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{Edits, Iteration, Rung, build_model};
    use std::path::Path;

    fn fixture(name: &str) -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/flow")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
    }

    fn html() -> String {
        emit_html(&build_model(&fixture("run7-session.jsonl"), None).unwrap())
    }

    fn html_with_spans() -> String {
        emit_html(
            &build_model(
                &fixture("run7-session.jsonl"),
                Some(&fixture("run7-dd-spans.json")),
            )
            .unwrap(),
        )
    }

    #[test]
    fn header_strip_shows_goal_gate_model_and_result() {
        let h = html();
        assert!(
            h.contains("generalize vLLM&#39;s <code>fused_a_gemm</code>"),
            "goal missing or backticks leaked"
        );
        // The tab title drops the markdown backticks instead of showing them.
        assert!(h.contains("<title>crucible run — Goal: generalize vLLM&#39;s fused_a_gemm"));
        assert!(h.contains("latency — lower is better"));
        assert!(h.contains("claude-opus-4-6"));
        assert!(h.contains("8.751 <span class=\"arr\">→</span> 8.656"));
        assert!(h.contains("1.08% better · kept at iteration 3"));
        assert!(h.contains("$33.32"));
        assert!(h.contains("12h 12m")); // 43966 s wall-clock
        assert!(h.contains("finished"));
    }

    #[test]
    fn swimlane_carries_badges_scores_and_check_lanes() {
        let h = html();
        assert_eq!(h.matches("✓ KEPT").count(), 1);
        assert_eq!(h.matches(">DISCARDED<").count(), 4);
        assert_eq!(h.matches("✕ REJECTED").count(), 1);
        for lane in ["build", "correctness", "latency", "ab-toggle", "mechanism"] {
            assert!(
                h.contains(&format!(">{lane}</text>")),
                "lane {lane} missing"
            );
        }
        assert!(h.contains("baseline 8.751"));
        assert!(h.contains("8.656 · 1.08% better"));
        assert!(h.contains("+1.23%"), "iter 1 delta missing");
        assert!(h.contains("not measured"), "rejected iteration gap missing");
        assert!(h.contains("FROZEN CHECKS"));
    }

    #[test]
    fn detail_sections_expose_edits_checks_and_notes() {
        let h = html();
        assert_eq!(h.matches("<details id=\"it-").count(), 6);
        assert!(
            h.contains("<details id=\"it-3\" open>"),
            "kept detail not open"
        );
        assert!(h.contains("5 files · +180 −57"));
        assert!(h.contains("csrc/libtorch_stable/dsv3_fused_a_gemm.cu"));
        assert!(h.contains("vllm/envs.py"));
        // Raw gate floats round to the hero's 3 decimals in the detail tables.
        assert!(h.contains("latency: tpot_ms=8.656"));
        assert!(!h.contains("8.65613441703772"));
        assert!(h.contains("correctness: score=0.93"));
        assert!(h.contains("mechanism: trace captured"));
        assert!(h.contains("correctness rejected the candidate"));
        // A skipped check says so instead of claiming a pass.
        assert!(h.contains("ab-toggle skipped: no VLLM_GLM_TOGGLE declared"));
        assert_eq!(h.matches("<td class=\"res skip\">skipped</td>").count(), 5);
    }

    #[test]
    fn preflight_and_footer_carry_the_run_bookends() {
        let h = html();
        assert!(h.contains("Before iteration 1 — 5 environment checks passed"));
        assert!(h.contains("--only-rung 1 --mode derive"));
        // Preflight notes round their floats and drop the gate's duplicated token.
        assert!(h.contains("correctness: score=0.938</td>"));
        assert!(h.contains("latency: tpot_ms=8.751 score=8.751"));
        assert!(
            h.contains("<a href=\"https://github.com/wseaton/vllm/pull/8\">wseaton/vllm#8</a>")
        );
        assert!(h.contains("branch autoresearch/"));
        assert!(h.contains("run record 20260811T192719Z-goal-generalize-vllm-s-fused-a-gemm-for"));
        assert!(h.contains("Post-run check: full-eval: ok — passed"));
    }

    #[test]
    fn spans_add_time_axis_turn_bands_and_tool_timelines() {
        let h = html_with_spans();
        assert!(h.contains("column width is real wall-clock time"));
        assert!(h.contains("preflight and post-run time sit outside it"));
        assert!(h.contains("elapsed since iter 1 (h:mm)"));
        assert!(h.contains("Agent activity — 198 tool calls over"));
        assert!(h.contains("Bash · 99"));
        assert!(h.contains("Read · 88"));
        assert!(h.contains("Edit · 11"));
        assert!(h.contains("fetch_log · 2"), "mcp tool name not shortened");
        assert!(h.contains("iter 3 · 1h 57m"));
        assert!(h.contains("→ ") && h.contains(" UTC · iteration wall-clock"));
    }

    #[test]
    fn without_spans_the_page_falls_back_to_equal_columns() {
        let h = html();
        assert!(h.contains("columns are equal width (no span timings)"));
        assert!(!h.contains("elapsed since iter 1 (h:mm)"));
        assert!(!h.contains("Agent activity"));
    }

    #[test]
    fn untrusted_text_is_escaped() {
        let mut m = FlowModel::default();
        m.run.goal_line = "Goal: <script>alert(1)</script> & \"quotes\"".into();
        m.run.gate = "a<b".into();
        m.run.model = "m".into();
        m.iterations.push(Iteration {
            n: 1,
            decision: "discard".into(),
            score: Some(1.0),
            delta_pct_vs_baseline: None,
            note_short: "note with <tags> & stuff".into(),
            edits: Edits {
                files_changed: 1,
                insertions: 1,
                deletions: 0,
                files: vec!["a/<weird>.rs".into()],
            },
            rungs: vec![Rung {
                name: "check<1>".into(),
                pass: false,
                skipped: false,
                note_short: "fail <note>".into(),
                duration_s: None,
            }],
            turn: Default::default(),
            started_at: None,
            ended_at: None,
        });
        let h = emit_html(&m);
        assert!(!h.contains("<script>alert"));
        assert!(h.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(h.contains("a/&lt;weird&gt;.rs"));
        assert!(h.contains("fail &lt;note&gt;"));
        assert!(h.contains("a&lt;b"));
    }

    #[test]
    fn page_is_self_contained_and_structurally_sound() {
        let h = html_with_spans();
        assert!(h.starts_with("<!doctype html>"));
        assert!(h.trim_end().ends_with("</html>"));
        // No external requests: the only http(s) href is the PR link.
        assert!(!h.contains("<link"));
        assert!(!h.contains("src=\"http"));
        assert!(!h.contains("@import"));
        assert_eq!(
            h.matches("href=\"http").count(),
            h.matches("href=\"https://github.com/wseaton/vllm/pull/8\"")
                .count()
        );
        for tag in ["details", "table", "svg", "section", "div"] {
            assert_eq!(
                h.matches(&format!("<{tag}")).count(),
                h.matches(&format!("</{tag}>")).count(),
                "unbalanced <{tag}>"
            );
        }
        // Explicit page background so dark-mode browsers don't repaint it.
        assert!(h.contains("color-scheme: light"));
        assert!(h.contains(&format!("background: {PAGE_BG}")));
    }

    #[test]
    fn palette_lives_only_in_the_root_block() {
        let h = html_with_spans();
        let root = h
            .split_once(":root {\n")
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map(|(block, _)| block)
            .expect(":root block missing");
        assert!(root.contains("--flow-bg: #f7f7f5;"));
        for (name, value) in PALETTE {
            assert!(
                root.contains(&format!("{name}: {value};")),
                "{name} missing"
            );
            assert_eq!(
                h.matches(value).count(),
                1,
                "{value} ({name}) leaks outside :root"
            );
            assert!(h.contains(&format!("var({name})")), "{name} unused");
        }
    }

    #[test]
    fn a_degenerate_model_still_renders() {
        let h = emit_html(&FlowModel::default());
        assert!(h.contains("No iterations were recorded"));
        assert!(h.contains("no scores recorded"));
        assert!(h.contains("No draft PR"));
        assert!(h.contains("interrupted"));
    }

    #[test]
    fn helpers_format_durations_and_deltas() {
        assert_eq!(fmt_dur(43966.0), "12h 12m");
        assert_eq!(fmt_dur(1665.7), "27m 46s");
        assert_eq!(fmt_dur(42.0), "42s");
        assert_eq!(fmt_hm(5985.0), "+1:39");
        assert_eq!(delta_phrase(Some("lower"), -1.08), "1.08% better");
        assert_eq!(delta_phrase(Some("lower"), 1.23), "1.23% worse");
        assert_eq!(delta_phrase(Some("higher"), 1.23), "1.23% better");
        assert_eq!(delta_phrase(None, 1.23), "+1.23%");
        assert_eq!(short_tool("mcp__vllm-broker__fetch_log"), "fetch_log");
        assert_eq!(short_tool("Bash"), "Bash");
        assert_eq!(count_noun(1, "file"), "1 file");
        assert_eq!(count_noun(5, "file"), "5 files");
    }

    #[test]
    fn fmt_note_rounds_floats_and_drops_echoed_tokens() {
        assert_eq!(
            fmt_note("latency: tpot_ms=8.65613441703772"),
            "latency: tpot_ms=8.656"
        );
        assert_eq!(
            fmt_note("correctness: score=0.9375 score=0.9375"),
            "correctness: score=0.938"
        );
        // Different keys with the same value are signal, not an echo.
        assert_eq!(
            fmt_note("tpot_ms=8.750550938259494 score=8.750550938259494"),
            "tpot_ms=8.751 score=8.751"
        );
        assert_eq!(fmt_note("score=0.93"), "score=0.93");
        assert_eq!(
            fmt_note("v1.2.3 at 12:44:28.015Z"),
            "v1.2.3 at 12:44:28.015Z"
        );
    }

    #[test]
    fn md_code_renders_backtick_spans() {
        assert_eq!(md_code("use `x<y`"), "use <code>x&lt;y</code>");
        assert_eq!(md_code("odd ` tick"), "odd ` tick");
        assert_eq!(md_code("plain"), "plain");
    }

    #[test]
    fn proportional_widths_respect_the_minimum() {
        let w = proportional_widths(&[100.0, 100.0, 1.0], 300.0);
        assert!(w[2] >= MIN_COL);
        let total: f64 = w.iter().sum();
        assert!((total - 300.0).abs() < 0.5, "widths sum to {total}");
        assert!((w[0] - w[1]).abs() < 0.01);
        // Even split when durations are equal.
        let eq = proportional_widths(&[5.0, 5.0], 200.0);
        assert!((eq[0] - 100.0).abs() < 0.01);
    }
}
