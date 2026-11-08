//! A run's tasks laid out in time for the Timeline pane.
//!
//! A log whose task events carry `at` is drawn as it ran, from the run's first start. An older
//! log records only how long each task took, so its schedule is derived: a task starts when the
//! last of its dependencies settles, and a fan-out's instances all start with it.

use std::collections::HashMap;

use crate::session::{TaskState, TaskView};

/// One bar: `start` and `end` in seconds from the run's start. A running task has no end yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Bar {
    pub name: String,
    pub state: TaskState,
    pub start: f64,
    pub end: Option<f64>,
    /// A fan-out's instances, each `(start, end, state)`.
    pub lanes: Vec<(f64, Option<f64>, TaskState)>,
    /// On the chain of dependencies that ends last.
    pub critical: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    pub bars: Vec<Bar>,
    /// When the last settled task ended.
    pub span: f64,
}

/// When a task, or the earliest of its instances, first started, in Unix seconds.
fn first_start(task: &TaskView) -> Option<f64> {
    task.started_at.or_else(|| {
        task.instances
            .iter()
            .filter_map(|instance| instance.started_at)
            .reduce(f64::min)
    })
}

/// Where each task sits, by index into the run's tasks: its start, and `Some(end)` once placed,
/// where `end` is `None` while it runs. `origin` is the run's first start in Unix seconds when the
/// placement came from stamps.
struct Placement {
    starts: Vec<f64>,
    ends: Vec<Option<Option<f64>>>,
    origin: Option<f64>,
}

/// Each task's start and end from the stamps in the log, against the run's first start; `None`
/// when a task with a bar has no start stamp, so the whole run is derived instead.
fn clocked(tasks: &[TaskView]) -> Option<Placement> {
    let placed: Vec<&TaskView> = tasks
        .iter()
        .filter(|task| task.state() != TaskState::Pending)
        .collect();
    if placed.is_empty() || placed.iter().any(|task| first_start(task).is_none()) {
        return None;
    }
    let origin = placed
        .iter()
        .filter_map(|task| first_start(task))
        .reduce(f64::min)?;
    let mut starts = vec![0.0; tasks.len()];
    let mut ends = vec![None; tasks.len()];
    for (i, task) in tasks.iter().enumerate() {
        let Some(start) = first_start(task).map(|at| at - origin) else {
            continue;
        };
        starts[i] = start;
        ends[i] = Some(match task.state() {
            TaskState::Settled(_) => task
                .result
                .as_ref()
                .map(|r| r.at.map_or(start + r.secs.max(0.0), |at| at - origin)),
            _ => None,
        });
    }
    Some(Placement {
        starts,
        ends,
        origin: Some(origin),
    })
}

/// Each task placed after its dependencies, from its recorded duration.
fn derived(tasks: &[TaskView], index: &HashMap<&str, usize>) -> Placement {
    let mut ends: Vec<Option<Option<f64>>> = vec![None; tasks.len()];
    let mut starts = vec![0.0; tasks.len()];
    for i in 0..tasks.len() {
        place(i, tasks, index, &mut starts, &mut ends, 0);
    }
    Placement {
        starts,
        ends,
        origin: None,
    }
}

/// The run's tasks in plan order: as they ran when the log says when, else each placed after
/// its dependencies.
pub fn schedule(tasks: &[TaskView]) -> Schedule {
    let index: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| (task.name.as_str(), i))
        .collect();
    let Placement {
        starts,
        ends,
        origin,
    } = clocked(tasks).unwrap_or_else(|| derived(tasks, &index));

    let mut bars: Vec<Bar> = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.state() != TaskState::Pending)
        .map(|(i, task)| {
            let start = starts[i];
            let lanes = task
                .instances
                .iter()
                .filter(|instance| instance.state() != TaskState::Pending)
                .map(|instance| {
                    let from = match (origin, instance.started_at) {
                        (Some(origin), Some(at)) => at - origin,
                        _ => start,
                    };
                    let end = instance.result.as_ref().map(|r| match (origin, r.at) {
                        (Some(origin), Some(at)) => at - origin,
                        _ => from + r.secs.max(0.0),
                    });
                    (from, end, instance.state())
                })
                .collect();
            Bar {
                name: task.name.clone(),
                state: task.state(),
                start,
                end: ends[i].flatten(),
                lanes,
                critical: false,
            }
        })
        .collect();
    let span = bars.iter().filter_map(|bar| bar.end).fold(0.0, f64::max);

    // Walk back from the bar that ends last, through the dependency that ended last.
    let mut at = bars
        .iter()
        .filter(|bar| bar.end.is_some())
        .max_by(|a, b| {
            a.end
                .partial_cmp(&b.end)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|bar| bar.name.clone());
    let mut guard = 0;
    while let Some(name) = at.take() {
        guard += 1;
        if guard > tasks.len() {
            break;
        }
        if let Some(bar) = bars.iter_mut().find(|bar| bar.name == name) {
            bar.critical = true;
        }
        let Some(&i) = index.get(name.as_str()) else {
            break;
        };
        at = tasks[i]
            .depends_on
            .iter()
            .filter_map(|dep| index.get(dep.as_str()).map(|&j| (dep, ends[j].flatten())))
            .filter_map(|(dep, end)| end.map(|end| (dep, end)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(dep, _)| dep.clone());
    }
    Schedule { bars, span }
}

/// Places task `i` after its dependencies. `ends[i]` is `Some(end)` once placed, where `end` is
/// `None` for a task still running or not started.
fn place(
    i: usize,
    tasks: &[TaskView],
    index: &HashMap<&str, usize>,
    starts: &mut [f64],
    ends: &mut [Option<Option<f64>>],
    depth: usize,
) -> Option<f64> {
    if let Some(end) = ends[i] {
        return end;
    }
    if depth > tasks.len() {
        return Some(0.0);
    }
    let task = &tasks[i];
    let start = task
        .depends_on
        .iter()
        .filter_map(|dep| index.get(dep.as_str()))
        .filter_map(|&j| place(j, tasks, index, starts, ends, depth + 1))
        .fold(0.0, f64::max);
    starts[i] = start;
    let end = match task.state() {
        TaskState::Settled(_) => {
            Some(start + task.result.as_ref().map_or(0.0, |r| r.secs.max(0.0)))
        }
        _ => None,
    };
    ends[i] = Some(end);
    end
}

/// How many bars (counting a fan-out's instances one each) run over time: `(from, to, count)`
/// steps covering `0..span`. Running bars count up to `span`.
pub fn concurrency(schedule: &Schedule) -> Vec<(f64, f64, usize)> {
    let span = schedule.span;
    let mut edges: Vec<(f64, i32)> = Vec::new();
    for bar in &schedule.bars {
        let intervals: Vec<(f64, f64)> = if bar.lanes.is_empty() {
            vec![(bar.start, bar.end.unwrap_or(span))]
        } else {
            bar.lanes
                .iter()
                .map(|(start, end, _)| (*start, end.unwrap_or(span)))
                .collect()
        };
        for (start, end) in intervals {
            if end > start {
                edges.push((start, 1));
                edges.push((end, -1));
            }
        }
    }
    edges.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    let mut steps = Vec::new();
    let (mut at, mut count) = (0.0, 0i32);
    for (time, change) in edges {
        if time > at && count > 0 {
            steps.push((at, time, count as usize));
        }
        at = at.max(time);
        count += change;
    }
    steps
}

/// A round tick step for an axis `span` seconds long, so it shows about four to eight ticks.
pub fn tick(span: f64) -> f64 {
    const STEPS: [f64; 14] = [
        1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0,
    ];
    STEPS
        .into_iter()
        .find(|step| span / step <= 8.0)
        .unwrap_or_else(|| (span / 6.0 / 3600.0).ceil() * 3600.0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::session::{RunFold, TaskState, TaskView};
    use crate::timeline::{concurrency, schedule, tick};

    /// Tasks as the session fold reads them from a plan, then each `(task, secs)` result,
    /// then each task started but not finished.
    fn run(plan: &[(&str, &[&str])], results: &[(&str, f64)], running: &[&str]) -> Vec<TaskView> {
        let tasks: Vec<_> = plan
            .iter()
            .map(|(name, deps)| {
                json!({"name": name, "kind": "command", "depends_on": deps, "required": true})
            })
            .collect();
        let mut fold = RunFold::default();
        let admitted = json!({"v": 1, "kind": "plan_admitted", "plan_version": 1, "budget_usd": 1.0, "tasks": tasks});
        assert!(fold.apply_line(&admitted.to_string()));
        for (task, secs) in results {
            let started = json!({"v": 1, "kind": "task_started", "task": task, "attempt": 1, "estimate_usd": 0.0});
            let result = json!({"v": 1, "kind": "task_result", "task": task, "status": "pass", "task_kind": "command", "attempts": 1, "cost_usd": 0.0, "note": "", "secs": secs});
            assert!(fold.apply_line(&started.to_string()));
            assert!(fold.apply_line(&result.to_string()));
        }
        for task in running {
            let started = json!({"v": 1, "kind": "task_started", "task": task, "attempt": 1, "estimate_usd": 0.0});
            assert!(fold.apply_line(&started.to_string()));
        }
        fold.tasks
    }

    const DIAMOND: [(&str, &[&str]); 4] = [
        ("draft", &[]),
        ("shape", &["draft"]),
        ("side", &["draft"]),
        ("audit", &["shape", "side"]),
    ];

    fn diamond() -> Vec<TaskView> {
        run(
            &DIAMOND,
            &[
                ("draft", 10.0),
                ("shape", 5.0),
                ("side", 30.0),
                ("audit", 2.0),
            ],
            &[],
        )
    }

    #[test]
    fn tasks_start_when_their_last_dependency_settles() {
        let s = schedule(&diamond());
        let at = |name: &str| {
            let bar = s.bars.iter().find(|b| b.name == name).unwrap();
            (bar.start, bar.end)
        };
        assert_eq!(at("draft"), (0.0, Some(10.0)));
        assert_eq!(at("shape"), (10.0, Some(15.0)));
        assert_eq!(at("side"), (10.0, Some(40.0)));
        assert_eq!(at("audit"), (40.0, Some(42.0)));
        assert_eq!(s.span, 42.0);
    }

    #[test]
    fn the_critical_path_follows_the_dependency_that_ended_last() {
        let critical: Vec<String> = schedule(&diamond())
            .bars
            .into_iter()
            .filter(|b| b.critical)
            .map(|b| b.name)
            .collect();
        assert_eq!(critical, ["draft", "side", "audit"]);
    }

    #[test]
    fn pending_tasks_have_no_bar_and_running_ones_no_end() {
        let tasks = run(
            &[("draft", &[]), ("edit", &["draft"]), ("later", &["edit"])],
            &[("draft", 4.0)],
            &["edit"],
        );
        let s = schedule(&tasks);
        assert_eq!(s.bars.len(), 2);
        assert_eq!(s.bars[1].state, TaskState::Running(1));
        assert_eq!((s.bars[1].start, s.bars[1].end), (4.0, None));
        assert_eq!(s.span, 4.0);
    }

    #[test]
    fn concurrency_counts_overlapping_bars() {
        assert_eq!(
            concurrency(&schedule(&diamond())),
            vec![
                (0.0, 10.0, 1),
                (10.0, 15.0, 2),
                (15.0, 40.0, 1),
                (40.0, 42.0, 1)
            ]
        );
    }

    #[test]
    fn a_cycle_does_not_hang() {
        let tasks = run(
            &[("a", &["b"]), ("b", &["a"])],
            &[("a", 1.0), ("b", 1.0)],
            &[],
        );
        assert_eq!(schedule(&tasks).bars.len(), 2);
    }

    /// A fold of stamped session lines: `(kind, task, at, extra)` with `at` in seconds past
    /// 12:00:00Z and `extra` merged into the event.
    fn stamped(
        plan: &[(&str, &[&str])],
        events: &[(&str, &str, f64, serde_json::Value)],
    ) -> Vec<TaskView> {
        let tasks: Vec<_> = plan
            .iter()
            .map(|(name, deps)| {
                json!({"name": name, "kind": "command", "depends_on": deps, "required": true})
            })
            .collect();
        let mut fold = RunFold::default();
        let admitted = json!({"v": 1, "kind": "plan_admitted", "plan_version": 1, "budget_usd": 1.0, "tasks": tasks});
        assert!(fold.apply_line(&admitted.to_string()));
        for (kind, task, at, extra) in events {
            let at = format!("2026-11-08T12:00:{:06.3}Z", at);
            let mut event = match *kind {
                "start" => {
                    json!({"v": 1, "kind": "task_started", "task": task, "attempt": 1, "estimate_usd": 0.0, "at": at})
                }
                _ => {
                    json!({"v": 1, "kind": "task_result", "task": task, "status": "pass", "task_kind": "command", "attempts": 1, "cost_usd": 0.0, "note": "", "secs": 0.0, "at": at})
                }
            };
            if let (Some(event), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
                event.extend(extra.clone());
            }
            assert!(fold.apply_line(&event.to_string()), "{event}");
        }
        fold.tasks
    }

    #[test]
    fn a_stamped_run_is_drawn_as_it_ran() {
        let none = json!({});
        let tasks = stamped(
            &[("a", &[]), ("b", &[]), ("c", &["a", "b"])],
            &[
                ("start", "a", 2.0, none.clone()),
                ("start", "b", 3.5, none.clone()),
                ("start", "a", 6.0, json!({"attempt": 2})),
                ("result", "a", 8.0, json!({"attempts": 2})),
                ("result", "b", 5.0, none.clone()),
                ("start", "c", 9.0, none.clone()),
            ],
        );
        let s = schedule(&tasks);
        let bar = |name: &str| s.bars.iter().find(|b| b.name == name).unwrap();
        assert_eq!(
            (bar("a").start, bar("a").end),
            (0.0, Some(6.0)),
            "a retry does not move the task's start"
        );
        assert_eq!(
            (bar("b").start, bar("b").end),
            (1.5, Some(3.0)),
            "b started when it did, not with the run"
        );
        assert_eq!((bar("c").start, bar("c").end), (7.0, None));
        assert!(bar("a").critical && !bar("b").critical);
        assert_eq!(s.span, 6.0);
    }

    #[test]
    fn a_stamped_fan_out_draws_each_instance_when_it_ran() {
        let none = json!({});
        let tasks = stamped(
            &[("edit", &[])],
            &[
                ("start", "edit[x]", 1.0, none.clone()),
                ("start", "edit[y]", 4.0, none.clone()),
                ("result", "edit[x]", 3.0, none.clone()),
                ("result", "edit[y]", 7.0, none.clone()),
                ("result", "edit", 7.0, none.clone()),
            ],
        );
        let s = schedule(&tasks);
        let edit = &s.bars[0];
        assert_eq!(
            edit.start, 0.0,
            "the node starts with its earliest instance"
        );
        let lanes: Vec<(f64, Option<f64>)> = edit.lanes.iter().map(|(a, b, _)| (*a, *b)).collect();
        assert_eq!(lanes, vec![(0.0, Some(2.0)), (3.0, Some(6.0))]);
    }

    #[test]
    fn a_run_with_any_unstamped_task_is_derived_whole() {
        let mut tasks = stamped(
            &[("a", &[]), ("b", &["a"])],
            &[
                ("start", "a", 2.0, json!({})),
                ("result", "a", 5.0, json!({"secs": 3.0})),
            ],
        );
        let unstamped = run(&[("a", &[]), ("b", &["a"])], &[("b", 4.0)], &[]);
        tasks[1] = unstamped[1].clone();
        let s = schedule(&tasks);
        let b = s.bars.iter().find(|bar| bar.name == "b").unwrap();
        assert_eq!(
            (b.start, b.end),
            (3.0, Some(7.0)),
            "b is placed after a's recorded duration"
        );
    }

    #[test]
    fn ticks_are_round_and_few() {
        assert_eq!(tick(9.0), 2.0);
        assert_eq!(tick(42.0), 10.0);
        assert_eq!(tick(600.0), 120.0);
        assert_eq!(tick(5_000.0), 900.0);
        assert!(tick(200_000.0) >= 3600.0);
    }
}
