//! Headless front-end: plain stdout lines, exactly what CI and in-cluster pods want.
//!
//! The agent's pretty stream is piped and echoed line by line (so we can also
//! read its cost for budgeting), visually the same as before. Ctrl+C is handled
//! by the process-wide handler in `main`, and the checkpoint here offers the
//! operator the steer/quit prompt.

use crate::event::{AgentEvent, RawStream};
use crate::reporter::{AgentTurn, Phase, Reporter, Row, Stop};
use crate::{Args, Paths, STOP, agent};
use std::io::{IsTerminal, Write};
use std::sync::atomic::Ordering;

pub struct ConsoleReporter;

impl Reporter for ConsoleReporter {
    fn start(&mut self, goal: &str, objective: &str) {
        println!("== goal ==\n{}\n", goal.trim());
        println!("== objective: {objective} ==");
    }

    fn phase(&mut self, phase: Phase) {
        match phase {
            Phase::Baseline => println!("== baseline =="),
            Phase::Iteration(it) => println!("\n== iteration {it} =="),
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
        if solved {
            println!("  SOLVED — the goal's win condition was met.");
        }
    }

    fn run_agent(
        &mut self,
        args: &Args,
        p: &Paths,
        it: u32,
        prompt: &str,
        resume_prompt: Option<&str>,
        session: Option<&str>,
    ) -> AgentTurn {
        println!("  -> running agent (iteration {it}, {}) …", args.model);
        // Pretty stream (json=false): echo each line so the human sees the same
        // output as before, while the helper scrapes cost for budgeting and we
        // watch the result event for a failed (is_error) no-op turn.
        let mut is_error = false;
        let mut error = None;
        let prepared = match session.map(|name| crate::agent_session::prepare(&p.state, name)) {
            Some(Ok(turn)) => {
                println!(
                    "  -> agent session {} {} (turn {})",
                    turn.logical_name,
                    turn.action(),
                    turn.completed_turns + 1
                );
                Some(turn)
            }
            Some(Err(e)) => {
                return AgentTurn {
                    cost: 0.0,
                    is_error: true,
                    error: Some(format!("preparing agent session failed: {e:#}")),
                };
            }
            None => None,
        };
        let cost = agent::run_turn_with_session(
            args,
            p,
            if prepared.as_ref().is_some_and(|turn| turn.is_resume()) {
                resume_prompt.unwrap_or(prompt)
            } else {
                prompt
            },
            false,
            prepared.as_ref(),
            |raw, stream, ev| {
                if let Some(AgentEvent::Result {
                    is_error: e,
                    error: text,
                    ..
                }) = ev
                {
                    is_error = *e;
                    error = text.clone();
                }
                if let Some(AgentEvent::Error { message, .. }) = ev {
                    is_error = true;
                    error = Some(message.clone());
                }
                match stream {
                    RawStream::Stdout => println!("{}", raw.trim_end()),
                    RawStream::Stderr => eprintln!("{}", raw.trim_end()),
                }
            },
        );
        if !is_error
            && let Some(turn) = &prepared
            && let Err(e) = crate::agent_session::commit(&p.state, turn)
        {
            is_error = true;
            error = Some(format!("committing agent session failed: {e:#}"));
        }
        AgentTurn {
            cost,
            is_error,
            error,
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
        println!("  iter {:>2} {:>8}  {}", r.iter, r.decision, r.note);
    }
}
