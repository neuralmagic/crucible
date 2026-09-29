//! Per-attempt deadlines, and their enforcement on a child process.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::duration::{Shown, TaskTimeout};

/// Which limit set an attempt's deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// The task's own `timeout`, counted from the attempt's start.
    Task(TaskTimeout),
    /// The run's wall-clock ceiling.
    Run(Duration),
}

/// The run's wall-clock ceiling as the instant it falls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCeiling {
    pub ends: Instant,
    pub ceiling: Duration,
}

impl RunCeiling {
    pub fn starting(at: Instant, ceiling: Duration) -> Option<Self> {
        Some(RunCeiling {
            ends: at.checked_add(ceiling)?,
            ceiling,
        })
    }

    pub fn reached(&self) -> bool {
        Instant::now() >= self.ends
    }
}

/// The instant one attempt must be over by, and the limit that put it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    pub at: Instant,
    pub bound: Bound,
}

impl Deadline {
    /// Whichever falls first of the task's own limit counted from `now` and the run's ceiling.
    pub fn for_attempt(
        now: Instant,
        timeout: Option<TaskTimeout>,
        run: Option<RunCeiling>,
    ) -> Option<Deadline> {
        let own = timeout.and_then(|t| {
            Some(Deadline {
                at: now.checked_add(t.get())?,
                bound: Bound::Task(t),
            })
        });
        let run = run.map(|r| Deadline {
            at: r.ends,
            bound: Bound::Run(r.ceiling),
        });
        match (own, run) {
            (Some(own), Some(run)) if own.at < run.at => Some(own),
            (_, Some(run)) => Some(run),
            (own, None) => own,
        }
    }

    /// The note an attempt killed at this deadline settles with.
    pub fn note(&self) -> String {
        match self.bound {
            Bound::Task(limit) => format!("timed out: the task ran past its {limit} limit"),
            Bound::Run(ceiling) => format!(
                "timed out: the run reached its {} wall-clock ceiling",
                Shown(ceiling)
            ),
        }
    }
}

/// Process groups spawned under a deadline and not yet reaped. They left the terminal's
/// foreground group, so an interrupt handler can only reach them through this list.
static LIVE_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// SIGTERM every process group [`Supervised`] has not reaped yet.
pub fn terminate_live_groups() {
    let groups = LIVE_GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
    for &pgid in groups.iter() {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
    }
}

fn kill_group(pgid: i32) {
    let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
}

/// How often a reap under a deadline polls. Only reached once the child's output is drained, so
/// the child has almost always exited already.
const REAP_POLL: Duration = Duration::from_millis(10);

/// A child running under an optional deadline. Under one, the child leads a process group of its
/// own and the whole group is killed when the deadline passes, so neither the child nor anything
/// it started can hold the attempt, or the pipes the caller is draining, past it.
pub struct Supervised {
    child: Child,
    deadline: Option<Deadline>,
    group: Option<i32>,
    watchdog: Option<Watchdog>,
}

/// A reaped child, and the deadline it was killed at, if it was.
#[derive(Debug)]
pub struct Reaped {
    pub status: ExitStatus,
    pub killed_at: Option<Deadline>,
}

impl Supervised {
    pub fn spawn(cmd: &mut Command, deadline: Option<Deadline>) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let Some(deadline) = deadline else {
            return Ok(Supervised {
                child: cmd.spawn()?,
                deadline: None,
                group: None,
                watchdog: None,
            });
        };
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let pgid = match i32::try_from(child.id()) {
            Ok(pgid) => pgid,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other(format!(
                    "child pid {} does not fit a process group id: {e}",
                    child.id()
                )));
            }
        };
        LIVE_GROUPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pgid);
        let watchdog = match Watchdog::arm(pgid, deadline.at) {
            Ok(watchdog) => watchdog,
            Err(e) => {
                let mut unwatched = Supervised {
                    child,
                    deadline: Some(deadline),
                    group: Some(pgid),
                    watchdog: None,
                };
                kill_group(pgid);
                let _ = unwatched.reap(true);
                return Err(e);
            }
        };
        Ok(Supervised {
            child,
            deadline: Some(deadline),
            group: Some(pgid),
            watchdog: Some(watchdog),
        })
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Reap the child once its output is drained, killing its group if the deadline passes first.
    pub fn wait(mut self) -> io::Result<Reaped> {
        let fired = self.watchdog.take().is_some_and(Watchdog::disarm);
        self.reap(fired)
    }

    fn reap(&mut self, fired: bool) -> io::Result<Reaped> {
        let result = self.reap_inner(fired);
        if let Some(pgid) = self.group.take() {
            LIVE_GROUPS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|&g| g != pgid);
        }
        result
    }

    fn reap_inner(&mut self, fired: bool) -> io::Result<Reaped> {
        let (Some(deadline), Some(pgid)) = (self.deadline, self.group) else {
            return Ok(Reaped {
                status: self.child.wait()?,
                killed_at: None,
            });
        };
        let mut killed = fired;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            let now = Instant::now();
            if now >= deadline.at {
                kill_group(pgid);
                killed = true;
                break self.child.wait()?;
            }
            std::thread::sleep((deadline.at - now).min(REAP_POLL));
        };
        Ok(Reaped {
            status,
            killed_at: killed.then_some(deadline),
        })
    }
}

/// Kills a process group at a deadline unless stopped first. It is stopped before the group
/// leader is reaped: until then the leader's pid, and so the group id, cannot be reused.
struct Watchdog {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<bool>>,
}

impl Watchdog {
    fn arm(pgid: i32, at: Instant) -> io::Result<Self> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let watched = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(format!("deadline-{pgid}"))
            .spawn(move || {
                let (lock, wake) = &*watched;
                let mut stopped = lock.lock().unwrap_or_else(PoisonError::into_inner);
                loop {
                    if *stopped {
                        return false;
                    }
                    let now = Instant::now();
                    if now >= at {
                        kill_group(pgid);
                        return true;
                    }
                    stopped = wake
                        .wait_timeout(stopped, at - now)
                        .map(|(guard, _)| guard)
                        .unwrap_or_else(|poisoned| poisoned.into_inner().0);
                }
            })?;
        Ok(Watchdog {
            stop,
            thread: Some(thread),
        })
    }

    fn stop(&self) {
        let (lock, wake) = &*self.stop;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        wake.notify_all();
    }

    /// Stop the watchdog and say whether it had already fired.
    fn disarm(mut self) -> bool {
        self.stop();
        self.thread
            .take()
            .is_some_and(|thread| thread.join().unwrap_or(false))
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use crate::deadline::{Bound, Deadline, RunCeiling, Supervised};
    use crate::duration::TaskTimeout;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn timeout(s: &str) -> TaskTimeout {
        s.parse().unwrap()
    }

    fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    #[test]
    fn the_earlier_of_the_task_limit_and_the_run_ceiling_sets_the_deadline() {
        let now = Instant::now();
        let long_run = RunCeiling::starting(now, Duration::from_secs(3600));
        let short_run = RunCeiling::starting(now, Duration::from_secs(30));

        let own = Deadline::for_attempt(now, Some(timeout("90s")), long_run).unwrap();
        assert_eq!(own.bound, Bound::Task(timeout("90s")));
        assert_eq!(own.at, now + Duration::from_secs(90));

        let run = Deadline::for_attempt(now, Some(timeout("90s")), short_run).unwrap();
        assert_eq!(run.bound, Bound::Run(Duration::from_secs(30)));
        assert_eq!(run.at, now + Duration::from_secs(30));

        let undeclared = Deadline::for_attempt(now, None, long_run).unwrap();
        assert_eq!(undeclared.bound, Bound::Run(Duration::from_secs(3600)));

        let unbounded_run = Deadline::for_attempt(now, Some(timeout("90s")), None).unwrap();
        assert_eq!(unbounded_run.bound, Bound::Task(timeout("90s")));

        assert_eq!(Deadline::for_attempt(now, None, None), None);
    }

    #[test]
    fn a_deadline_note_names_the_limit_that_set_it() {
        let now = Instant::now();
        let own = Deadline::for_attempt(now, Some(timeout("10m")), None).unwrap();
        assert_eq!(own.note(), "timed out: the task ran past its 10m limit");
        let run = Deadline::for_attempt(
            now,
            None,
            RunCeiling::starting(now, Duration::from_secs(90)),
        )
        .unwrap();
        assert_eq!(
            run.note(),
            "timed out: the run reached its 90s wall-clock ceiling"
        );
    }

    /// The grandchild holds stdout open, so a kill that reached only the shell would leave the
    /// reader blocked for the grandchild's whole sleep.
    #[test]
    fn a_deadline_kills_the_whole_group_and_unblocks_its_reader() {
        let started = Instant::now();
        let deadline = Deadline::for_attempt(started, Some(timeout("0.5s")), None);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo $!; wait")
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        let mut child = Supervised::spawn(&mut cmd, deadline).unwrap();
        let mut out = String::new();
        child
            .child()
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let reaped = child.wait().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the reader outlived the deadline"
        );
        assert_eq!(reaped.killed_at, deadline);
        let grandchild: i32 = out.trim().parse().unwrap();
        let settle = Instant::now();
        while alive(grandchild) && settle.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !alive(grandchild),
            "the grandchild {grandchild} survived its group's kill"
        );
    }

    #[test]
    fn a_child_that_finishes_in_time_is_not_killed() {
        let deadline = Deadline::for_attempt(Instant::now(), Some(timeout("30s")), None);
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 3");
        let reaped = Supervised::spawn(&mut cmd, deadline)
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(reaped.status.code(), Some(3));
        assert_eq!(reaped.killed_at, None);
    }

    /// A child that closes its output early and then hangs is caught by the reap, not the reader.
    #[test]
    fn a_child_that_hangs_after_closing_its_output_is_killed_at_the_reap() {
        let started = Instant::now();
        let deadline = Deadline::for_attempt(started, Some(timeout("0.5s")), None);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("exec >&- 2>&-; sleep 30")
            .stdout(Stdio::piped());
        let mut child = Supervised::spawn(&mut cmd, deadline).unwrap();
        let mut out = String::new();
        let _ = child
            .child()
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out);
        let reaped = child.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(reaped.killed_at, deadline);
    }

    fn group_of(pid: u32) -> String {
        let out = Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn without_a_deadline_the_child_shares_the_callers_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("ps -o pgid= -p $$")
            .stdout(Stdio::piped());
        let mut child = Supervised::spawn(&mut cmd, None).unwrap();
        let mut out = String::new();
        child
            .child()
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let reaped = child.wait().unwrap();
        assert!(reaped.status.success());
        assert_eq!(reaped.killed_at, None);
        assert_eq!(out.trim(), group_of(std::process::id()));
    }
}
