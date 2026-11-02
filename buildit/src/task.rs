//! MCP tasks (`io.modelcontextprotocol/tasks`) over detached steps. A task id names the build and
//! the step's log, and its state is read back from the builder pod on every `tasks/get`.

use crate::logs::LogName;
use crate::mcp::BuildId;
use crate::step::{Fetch, Published, Snapshot, State};

pub const POLL_INTERVAL_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRef {
    pub build: BuildId,
    pub log: LogName,
}

impl TaskRef {
    pub fn encode(&self) -> String {
        match self.log {
            LogName::Build => format!("build/{}", self.build.as_str()),
            LogName::Run(n) => format!("run/{}/{n}", self.build.as_str()),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let parts: Vec<&str> = id.split('/').collect();
        let (build, log) = match parts[..] {
            ["build", build] => (build, LogName::Build),
            ["run", build, n] => {
                let n: u32 = n.parse().ok()?;
                if parts[2] != n.to_string() {
                    return None;
                }
                (build, LogName::Run(n))
            }
            _ => return None,
        };
        Some(Self {
            build: BuildId::parse(build).ok()?,
            log,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Working,
    // the run ended with fetch_paths still to be published to the sandbox
    Publishing,
    Done { state: State, published: Published },
    Cancelled,
}

pub fn progress(snap: &Snapshot) -> Progress {
    match (snap.state, &snap.published, snap.fetch) {
        (State::Running, _, _) => Progress::Working,
        (State::Cancelled, _, _) => Progress::Cancelled,
        (state, Some(p), _) => Progress::Done {
            state,
            published: p.clone(),
        },
        (state, None, Fetch::Nothing) => Progress::Done {
            state,
            published: Published::Fetched(Vec::new()),
        },
        (_, None, Fetch::Ready | Fetch::Failed) => Progress::Publishing,
    }
}

#[cfg(test)]
mod tests {
    use crate::logs::LogName;
    use crate::mcp::BuildId;
    use crate::step::{Fetch, Published, Snapshot, State};
    use crate::task::{Progress, TaskRef, progress};

    #[test]
    fn task_ids_round_trip_and_reject_garbage() {
        let build = BuildId::parse("buildit-0123456789abcdef").unwrap();
        for (log, id) in [
            (LogName::Build, "build/buildit-0123456789abcdef"),
            (LogName::Run(7), "run/buildit-0123456789abcdef/7"),
        ] {
            let t = TaskRef {
                build: build.clone(),
                log,
            };
            assert_eq!(t.encode(), id);
            assert_eq!(TaskRef::parse(id), Some(t));
        }
        for bad in [
            "",
            "build",
            "build/",
            "build/b/1",
            "run/b",
            "run/b/",
            "run/b/x",
            "run/b/01",
            "run/b/+1",
            "run/b/-1",
            "run/b/1/2",
            "run//1",
            "Build/b",
            "build/B",
            "build/a,buildit.dev/sandbox=b",
            "build/a=b",
            "logs/b",
        ] {
            assert_eq!(TaskRef::parse(bad), None, "{bad:?}");
        }
    }

    fn snap(state: State, fetch: Fetch, published: Option<Published>) -> Snapshot {
        Snapshot {
            created: "c".to_string(),
            updated: "u".to_string(),
            state,
            fetch,
            published,
        }
    }

    #[test]
    fn progress_follows_the_step_files() {
        let exited = |code, timed_out| State::Exited { code, timed_out };
        let fetched = |f: &[&str]| Published::Fetched(f.iter().map(|s| s.to_string()).collect());
        let done = |state, published| Progress::Done { state, published };
        let cases = [
            (
                snap(State::Running, Fetch::Nothing, None),
                Progress::Working,
            ),
            (snap(State::Running, Fetch::Ready, None), Progress::Working),
            (
                snap(exited(0, false), Fetch::Nothing, None),
                done(exited(0, false), fetched(&[])),
            ),
            (
                snap(exited(124, true), Fetch::Nothing, None),
                done(exited(124, true), fetched(&[])),
            ),
            (
                snap(State::Lost, Fetch::Nothing, None),
                done(State::Lost, fetched(&[])),
            ),
            (
                snap(State::Expired, Fetch::Nothing, None),
                done(State::Expired, fetched(&[])),
            ),
            (
                snap(exited(5, false), Fetch::Ready, None),
                Progress::Publishing,
            ),
            (
                snap(exited(0, false), Fetch::Failed, None),
                Progress::Publishing,
            ),
            (
                snap(exited(5, false), Fetch::Nothing, Some(fetched(&["a"]))),
                done(exited(5, false), fetched(&["a"])),
            ),
            (
                snap(
                    exited(0, false),
                    Fetch::Failed,
                    Some(Published::Error("x".to_string())),
                ),
                done(exited(0, false), Published::Error("x".to_string())),
            ),
            (
                snap(State::Cancelled, Fetch::Nothing, None),
                Progress::Cancelled,
            ),
            (
                snap(State::Cancelled, Fetch::Ready, None),
                Progress::Cancelled,
            ),
        ];
        for (s, want) in cases {
            assert_eq!(progress(&s), want, "{s:?}");
        }
    }
}
