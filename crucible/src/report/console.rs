//! Headless front-end: plain stdout lines, exactly what CI and in-cluster pods want.
//!
//! The agent's pretty stream is piped and echoed line by line (so we can also
//! read its cost for budgeting), visually the same as before. Ctrl+C is handled
//! by the process-wide handler in `main`, and the checkpoint here offers the
//! operator the steer/quit prompt.

use crate::args::Paths;
use crate::process::STOP;
use crate::report::reporter::{Reporter, Stop};
use crate::report::session::Row;
use crucible_contract::LoopPhase;
use std::io::{IsTerminal, Write};
use std::sync::atomic::Ordering;

pub struct ConsoleReporter;

impl Reporter for ConsoleReporter {
    fn start(&mut self, goal: &str, objective: &str) {
        println!("== goal ==\n{}\n", goal.trim());
        println!("== objective: {objective} ==");
    }

    fn phase(&mut self, phase: LoopPhase, iter: u32) {
        match phase {
            LoopPhase::Iteration => println!("\n== iteration {iter} =="),
            other => println!("== {other} =="),
        }
    }

    fn note(&mut self, msg: &str) {
        println!("  {msg}");
    }

    fn row(&mut self, row: &Row, solved: bool) {
        match row.decision.as_str() {
            "baseline" => println!("baseline: {}", row.note),
            "keep" => println!("  KEEP: {}", row.note),
            "discard" => println!("  DISCARD ({}); rolled back to best", row.note),
            other => println!("  {other}: {}", row.note),
        }
        if !row.diffstat.is_empty() {
            println!("  diff: {}", row.diffstat);
        }
        if !row.evidence.is_empty() {
            println!(
                "  evidence: {}",
                crate::report::reporter::evidence_line(&row.evidence)
            );
        }
        if solved {
            println!("  SOLVED — the goal's win condition was met.");
        }
    }

    fn agent_starting(
        &mut self,
        iter: u32,
        model: &str,
        session: Option<&crate::agent::agent_session::SessionTurn>,
    ) {
        println!("  -> running agent (iteration {iter}, {model}) …");
        if let Some(turn) = session {
            println!(
                "  -> agent session {} {} (turn {})",
                turn.logical_name,
                turn.action(),
                turn.completed_turns + 1
            );
        }
    }

    /// The harness already prints the agent's stderr, prefixed with the task name.
    fn agent_line(&mut self, line: &str, stream: crate::agent::event::RawStream) {
        if stream == crate::agent::event::RawStream::Stdout {
            println!("{}", line.trim_end());
        }
    }

    fn budget(&mut self, spent: f64, elapsed: std::time::Duration) {
        println!(
            "  budget: spent ${spent:.4} · elapsed {}m{:02}s",
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60
        );
    }

    fn check_interrupt(&mut self, p: &Paths, rows: &[Row]) -> Stop {
        if !STOP.load(Ordering::SeqCst) {
            return Stop::Continue;
        }
        if !std::io::stdin().is_terminal() {
            println!("\n[crucible] stopping (non-interactive).");
            return Stop::Quit;
        }
        print_rows(rows);
        print!("\n[crucible] (q)uit, (s)teer next iteration, or (c)ontinue? ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Stop::Quit;
        }
        match line.trim() {
            "s" | "steer" => {
                println!("enter steer text; finish with an empty line:");
                let mut buf = String::new();
                loop {
                    let mut l = String::new();
                    if std::io::stdin().read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                        break;
                    }
                    buf.push_str(&l);
                }
                let _ = std::fs::write(&p.steer, buf);
                STOP.store(false, Ordering::SeqCst);
                Stop::Continue
            }
            "c" | "continue" => {
                STOP.store(false, Ordering::SeqCst);
                Stop::Continue
            }
            _ => Stop::Quit,
        }
    }

    fn summary(&mut self, rows: &[Row], objective: &str, best_score: f64) {
        println!("\n== summary ==");
        print_rows(rows);
        if best_score.is_finite() {
            println!("\nbest {objective} = {best_score}");
        } else if objective == "task" {
            let kept = rows.iter().filter(|r| r.decision == "keep").count();
            println!("\ntask: {kept} iteration(s) kept");
        } else {
            let solved = rows.iter().any(|r| r.decision == "keep");
            println!(
                "\n{objective}: {}",
                if solved {
                    "kept a winning candidate"
                } else {
                    "no improvement"
                }
            );
        }
    }
}

fn print_rows(rows: &[Row]) {
    println!("\n-- progress --");
    for r in rows {
        let evidence = if r.evidence.is_empty() {
            String::new()
        } else {
            format!(
                "  [evidence: {}]",
                crate::report::reporter::evidence_line(&r.evidence)
            )
        };
        println!(
            "  iter {:>2} {:>8}  {}{evidence}",
            r.iter, r.decision, r.note
        );
    }
}
