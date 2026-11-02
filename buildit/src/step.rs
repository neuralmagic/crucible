//! A build or run step detached from the exec that starts it. The step runs under `setsid` in the
//! builder pod and keeps its state in files next to its log, so any broker process can observe,
//! cancel, or finish it:
//!
//! ```text
//! <log>            stage output, each stage headed by `$ argv`
//! <log>.paths      fetch_paths, one per line (run only)
//! <log>.pid        pid of the runner; a dead or zombie runner without an exit record is lost
//! <log>.started    creation time, written once the runner is up
//! <log>.stage      pid of the running stage's `timeout`
//! <log>.exit       `<code>[ timeout]` or `cancelled`; hard-linked into place, first writer wins
//! <log>.tar        fetch_paths tarball, present once the run's fetch succeeded
//! <log>.fetch-err  stderr of the fetch
//! <log>.claim      held by the broker publishing the tarball
//! <log>.published  JSON `Published`, the broker-side outcome of the fetch
//! ```
//!
//! A build also copies its exit record to the termination log and mirrors its log to the
//! container's stdout, so both outlive the pod's deadline.
#![allow(clippy::disallowed_macros)]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::logs::LogName;
use crate::pod::BuilderPod;
use crate::sandbox::RelPath;

pub const FETCH_LIMIT: u64 = 512 * 1024 * 1024;
pub const CONTAINER_STDOUT: &str = "/proc/1/fd/1";
pub const TERMINATION_LOG: &str = "/dev/termination-log";
const STATUS_LIMIT: u64 = 4 * 1024 * 1024;
const WAIT_CHUNK_S: u64 = 30;
const EXEC_SLACK: Duration = Duration::from_secs(30);
const CLAIM_STALE_MIN: u32 = 10;

pub(crate) const RUNNER: &str = r#"log=$1; mode=$2; shift 2; t=
stage() {
  secs=$1; shift
  [ -e "$log.exit" ] && return 130
  printf '$ %s\n' "$*" >> "$log"
  s=$(date +%s)
  timeout "$secs" "$@" >> "$log" 2>&1 < /dev/null &
  p=$!
  echo "$p" > "$log.stage"
  [ -e "$log.exit" ] && kill "$p" 2>/dev/null
  wait "$p"; c=$?; t=
  [ "$c" -ne 0 ] && [ $(($(date +%s) - s)) -ge "$secs" ] && t=" timeout"
  return "$c"
}
term=
case $mode in
build)
  secs=$1; term=$2; shift 2
  stage "$secs" "$@"; c=$?
  ;;
run)
  fs=$1; rs=$2; ctr=$3; img=$4; fetch=$5; shift 5
  stage "$fs" buildah from --pull-never --name "$ctr" "$img"; c=$?
  if [ "$c" -eq 0 ]; then
    stage "$rs" buildah run "$ctr" -- "$@"; c=$?
    if [ -s "$log.paths" ] && [ ! -e "$log.exit" ]; then
      buildah unshare sh -c "$fetch" sh "$ctr" "$log.paths" > "$log.tar.part" 2> "$log.fetch-err" \
        && mv "$log.tar.part" "$log.tar"
      rm -f "$log.tar.part"
    fi
    buildah rm "$ctr" > /dev/null 2>&1
  fi
  ;;
*) c=2 ;;
esac
printf '%s%s\n' "$c" "$t" > "$log.exit.$$" && ln "$log.exit.$$" "$log.exit" 2>/dev/null
rm -f "$log.exit.$$"
[ -n "$term" ] && cat "$log.exit" > "$term" 2>/dev/null
exit 0"#;

pub(crate) const LAUNCH: &str = r#"body=$1; log=$2; mirror=$3; shift 3
mkdir -p "${log%/*}" && : >> "$log" && cat > "$log.paths" || exit 1
setsid -w sh -c "$body" sh "$log" "$@" < /dev/null > /dev/null 2>&1 &
pid=$!
echo "$pid" > "$log.pid" || exit 1
if [ -n "$mirror" ]; then
  tail -n +1 -f --pid="$pid" "$log" < /dev/null 2> /dev/null > "$mirror" &
fi
date -u +%Y-%m-%dT%H:%M:%SZ > "$log.started""#;

const FETCH: &str = r#"mnt=$(buildah mount "$1") || exit 1; cd "$mnt" || exit 1; exec tar -cf - --ignore-failed-read --verbatim-files-from --no-unquote -T "$2""#;

pub(crate) const STATUS: &str = r#"log=$1
[ -e "$log.started" ] || exit 3
cat "$log.started"
for f in "$log.published" "$log.exit" "$log"; do
  [ -e "$f" ] && { date -u -r "$f" +%Y-%m-%dT%H:%M:%SZ; break; }
done
if [ -s "$log.exit" ]; then
  echo "exit $(head -n 1 "$log.exit")"
else
  pid=$(cat "$log.pid" 2>/dev/null)
  case "$pid" in ""|*[!0-9]*) stat="" ;; *) stat=$(cat "/proc/$pid/stat" 2>/dev/null) ;; esac
  stat=${stat##*) }
  case "${stat%% *}" in
    ""|Z|X) if [ -s "$log.exit" ]; then echo "exit $(head -n 1 "$log.exit")"; else echo lost; fi ;;
    *) echo running ;;
  esac
fi
if [ -e "$log.tar" ]; then echo tar; elif [ -e "$log.fetch-err" ]; then echo err; else echo -; fi
if [ -s "$log.published" ]; then cat "$log.published"; echo; else echo -; fi"#;

const WAIT: &str =
    r#"n=$2; while [ ! -s "$1.exit" ] && [ "$n" -gt 0 ]; do sleep 1; n=$((n - 1)); done"#;

pub(crate) const CANCEL: &str = r#"log=$1
printf 'cancelled\n' > "$log.exit.c$$" || exit 1
if ln "$log.exit.c$$" "$log.exit" 2>/dev/null; then
  rm -f "$log.exit.c$$"
  p=$(cat "$log.stage" 2>/dev/null) && kill "$p" 2>/dev/null
  echo cancelled
else
  rm -f "$log.exit.c$$"
  echo ended
fi"#;

const CLAIM: &str = r#"find "$1.claim" -maxdepth 0 -mmin "+$2" -exec rm -f {} + 2>/dev/null
[ -s "$1.published" ] && exit 1
set -C; true > "$1.claim" 2>/dev/null"#;

const RECORD: &str = r#"cat > "$1.published.part" && mv "$1.published.part" "$1.published" && rm -f "$1.tar" "$1.fetch-err""#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Exited { code: i32, timed_out: bool },
    Cancelled,
    // the runner died without an exit record
    Lost,
    // the pod hit its deadline without an exit record
    Expired,
}

impl State {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "running" => Ok(Self::Running),
            "lost" => Ok(Self::Lost),
            other => other
                .strip_prefix("exit ")
                .and_then(Self::record)
                .ok_or_else(|| anyhow!("unexpected step state {other:?}")),
        }
    }

    // `<exit code>[ timeout]` or `cancelled`, as the runner and CANCEL write it
    pub fn record(raw: &str) -> Option<Self> {
        let (code, timed_out) = match raw.split_whitespace().collect::<Vec<_>>()[..] {
            ["cancelled"] => return Some(Self::Cancelled),
            [code] => (code, false),
            [code, "timeout"] => (code, true),
            _ => return None,
        };
        Some(Self::Exited {
            code: code.parse().ok()?,
            timed_out,
        })
    }

    pub fn status(self) -> Status {
        match self {
            Self::Running => Status::Running,
            Self::Exited { code: 0, .. } => Status::Succeeded,
            Self::Cancelled => Status::Cancelled,
            Self::Exited { .. } | Self::Lost | Self::Expired => Status::Failed,
        }
    }

    pub fn exit(self) -> Option<i32> {
        match self {
            Self::Exited { code, .. } => Some(code),
            Self::Running | Self::Cancelled | Self::Lost | Self::Expired => None,
        }
    }

    pub fn timed_out(self) -> bool {
        match self {
            Self::Exited { timed_out, .. } => timed_out,
            Self::Expired => true,
            Self::Running | Self::Cancelled | Self::Lost => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Nothing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Published {
    Fetched(Vec<String>),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub created: String,
    pub updated: String,
    pub state: State,
    pub fetch: Fetch,
    pub published: Option<Published>,
}

impl Snapshot {
    fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        let mut next = |what: &str| {
            lines
                .next()
                .ok_or_else(|| anyhow!("step status has no {what} line: {text:?}"))
        };
        let created = next("created")?.to_string();
        let updated = next("updated")?.to_string();
        let state = State::parse(next("state")?)?;
        let fetch = match next("fetch")? {
            "-" => Fetch::Nothing,
            "tar" => Fetch::Ready,
            "err" => Fetch::Failed,
            raw => bail!("bad step fetch state {raw:?}"),
        };
        let published = match next("published")? {
            "-" => None,
            raw => Some(serde_json::from_str(raw).context("reading the published record")?),
        };
        Ok(Self {
            created,
            updated,
            state,
            fetch,
            published,
        })
    }
}

fn sh(script: &str, args: impl IntoIterator<Item = String>) -> Vec<String> {
    ["sh", "-c", script, "sh"]
        .map(String::from)
        .into_iter()
        .chain(args)
        .collect()
}

pub fn build_args(limit: Duration, cmd: &[String]) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        limit.as_secs().to_string(),
        TERMINATION_LOG.to_string(),
    ];
    args.extend_from_slice(cmd);
    args
}

pub fn run_args(
    from_limit: Duration,
    run_limit: Duration,
    ctr: &str,
    image: &str,
    cmd: &[String],
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        from_limit.as_secs().to_string(),
        run_limit.as_secs().to_string(),
        ctr.to_string(),
        image.to_string(),
        FETCH.to_string(),
    ];
    args.extend_from_slice(cmd);
    args
}

// `mirror` copies the log to that path as it grows, e.g. the container's stdout
pub async fn launch(
    pod: &BuilderPod,
    log: LogName,
    mirror: Option<&str>,
    paths: &[RelPath],
    args: Vec<String>,
) -> Result<()> {
    let head = [
        RUNNER.to_string(),
        log.path(),
        mirror.unwrap_or_default().to_string(),
    ];
    let argv = sh(LAUNCH, head.into_iter().chain(args));
    let stdin: String = paths.iter().map(|p| format!("{}\n", p.as_str())).collect();
    pod.exec_with_stdin(&argv, stdin.as_bytes())
        .await
        .with_context(|| format!("starting the {log} step"))
}

// None when the step was never started
pub async fn snapshot(pod: &BuilderPod, log: LogName) -> Result<Option<Snapshot>> {
    let out = pod
        .exec_output(&sh(STATUS, [log.path()]), STATUS_LIMIT)
        .await?;
    match out.code {
        Some(0) => Snapshot::parse(&String::from_utf8_lossy(&out.stdout)).map(Some),
        Some(3) => Ok(None),
        _ => bail!(
            "reading the {log} step state: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

// the snapshot once the step stops running or `limit` passes, whichever is first
pub async fn wait(pod: &BuilderPod, log: LogName, limit: Duration) -> Result<Snapshot> {
    let deadline = Instant::now() + limit;
    loop {
        let snap = snapshot(pod, log)
            .await?
            .ok_or_else(|| anyhow!("the {log} step was never started"))?;
        let left = deadline.saturating_duration_since(Instant::now());
        if snap.state != State::Running || left.is_zero() {
            return Ok(snap);
        }
        let secs = left.as_secs().clamp(1, WAIT_CHUNK_S);
        let argv = sh(WAIT, [log.path(), secs.to_string()]);
        let chunk = Duration::from_secs(secs) + EXEC_SLACK;
        if let Ok(waited) = tokio::time::timeout(chunk, pod.exec_capture(&argv)).await {
            waited?;
        }
    }
}

// true when this call ended the step, false when it had already ended
pub async fn cancel(pod: &BuilderPod, log: LogName) -> Result<bool> {
    let out = pod.exec_capture(&sh(CANCEL, [log.path()])).await?;
    Ok(out.trim() == "cancelled")
}

// true when this caller holds the claim and should publish the run's fetch
pub async fn claim(pod: &BuilderPod, log: LogName) -> Result<bool> {
    let argv = sh(CLAIM, [log.path(), CLAIM_STALE_MIN.to_string()]);
    Ok(pod.exec_output(&argv, 4096).await?.code == Some(0))
}

pub async fn record(pod: &BuilderPod, log: LogName, published: &Published) -> Result<()> {
    let body = serde_json::to_string(published).context("serializing the published record")?;
    pod.exec_with_stdin(&sh(RECORD, [log.path()]), body.as_bytes())
        .await
}

pub async fn fetched_tar(pod: &BuilderPod, log: LogName) -> Result<Vec<u8>> {
    let argv = ["cat", "--", &format!("{}.tar", log.path())].map(String::from);
    let out = pod.exec_output(&argv, FETCH_LIMIT).await?;
    match out.code {
        None => bail!("fetch_paths add up to more than {FETCH_LIMIT} bytes"),
        Some(0) => Ok(out.stdout),
        Some(_) => bail!(
            "reading fetched paths: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

pub async fn fetch_error(pod: &BuilderPod, log: LogName) -> Result<String> {
    let argv = ["cat", "--", &format!("{}.fetch-err", log.path())].map(String::from);
    let out = pod.exec_output(&argv, 64 * 1024).await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use crate::step::{Fetch, Published, Snapshot, State, Status};

    #[test]
    fn states_parse_from_the_status_script() {
        let exited = |code, timed_out| State::Exited { code, timed_out };
        let cases = [
            ("running\n", State::Running, Status::Running, None, false),
            (
                "exit 0\n",
                exited(0, false),
                Status::Succeeded,
                Some(0),
                false,
            ),
            ("exit 1", exited(1, false), Status::Failed, Some(1), false),
            (
                "exit 124\n",
                exited(124, false),
                Status::Failed,
                Some(124),
                false,
            ),
            (
                "exit 124 timeout\n",
                exited(124, true),
                Status::Failed,
                Some(124),
                true,
            ),
            (
                "exit 137 timeout",
                exited(137, true),
                Status::Failed,
                Some(137),
                true,
            ),
            (
                "exit cancelled\n",
                State::Cancelled,
                Status::Cancelled,
                None,
                false,
            ),
            ("lost\n", State::Lost, Status::Failed, None, false),
        ];
        for (raw, state, status, exit, timed_out) in cases {
            let parsed = State::parse(raw).unwrap();
            assert_eq!(parsed, state, "{raw:?}");
            assert_eq!(parsed.status(), status, "{raw:?}");
            assert_eq!(parsed.exit(), exit, "{raw:?}");
            assert_eq!(parsed.timed_out(), timed_out, "{raw:?}");
        }
        for bad in [
            "",
            "exit ",
            "exit x",
            "exit 1 2",
            "exit 1 timeout x",
            "exit timeout",
            "exit cancelled timeout",
            "cancelled",
            "Running",
            "done",
        ] {
            assert!(State::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(State::Expired.status(), Status::Failed);
        assert_eq!(State::Expired.exit(), None);
        assert!(State::Expired.timed_out());
        assert_eq!(
            serde_json::to_value([
                Status::Running,
                Status::Succeeded,
                Status::Failed,
                Status::Cancelled
            ])
            .unwrap(),
            serde_json::json!(["running", "succeeded", "failed", "cancelled"])
        );
    }

    #[test]
    fn snapshots_parse_every_state_the_status_script_prints() {
        let working =
            Snapshot::parse("2026-10-08T00:00:00Z\n2026-10-08T00:01:00Z\nrunning\n-\n-\n").unwrap();
        assert_eq!(
            working,
            Snapshot {
                created: "2026-10-08T00:00:00Z".to_string(),
                updated: "2026-10-08T00:01:00Z".to_string(),
                state: State::Running,
                fetch: Fetch::Nothing,
                published: None,
            }
        );
        let snap = |state: &str, fetch: &str, published: &str| {
            Snapshot::parse(&format!("c\nu\n{state}\n{fetch}\n{published}\n")).unwrap()
        };
        assert_eq!(snap("lost", "-", "-").state, State::Lost);
        assert_eq!(snap("exit 0", "tar", "-").fetch, Fetch::Ready);
        assert_eq!(snap("exit 0", "err", "-").fetch, Fetch::Failed);
        assert_eq!(
            snap("exit 0", "-", r#"{"fetched":["out/a.txt"]}"#).published,
            Some(Published::Fetched(vec!["out/a.txt".to_string()]))
        );
        assert_eq!(
            snap("exit 0", "err", r#"{"error":"fetching paths failed: x"}"#).published,
            Some(Published::Error("fetching paths failed: x".to_string()))
        );
        for bad in [
            "",
            "c\nu\nrunning\n-\n",
            "c\nu\nexit x\n-\n-\n",
            "c\nu\nrunning\nzip\n-\n",
            "c\nu\nrunning\n-\n{\n",
        ] {
            assert!(Snapshot::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn published_records_round_trip() {
        for p in [
            Published::Fetched(vec!["a".to_string(), "b/c".to_string()]),
            Published::Fetched(Vec::new()),
            Published::Error("multi\nline".to_string()),
        ] {
            let line = serde_json::to_string(&p).unwrap();
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(serde_json::from_str::<Published>(&line).unwrap(), p);
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }

    #[cfg(target_os = "linux")]
    fn read(p: &std::path::Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    fn ext(log: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}.{suffix}", log.display()))
    }

    #[cfg(target_os = "linux")]
    fn sh_script(script: &str, args: &[&std::path::Path]) -> std::process::Output {
        std::process::Command::new("sh")
            .args(["-c", script, "sh"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn launch(log: &std::path::Path, secs: &str, cmd: &[&str]) {
        use std::path::Path;
        let (mirror, term) = (ext(log, "mirror"), ext(log, "term"));
        let mut args = vec![Path::new(crate::step::RUNNER), log, &mirror];
        args.extend([Path::new("build"), Path::new(secs), &term]);
        args.extend(cmd.iter().map(Path::new));
        let started = std::time::Instant::now();
        let out = sh_script(crate::step::LAUNCH, &args);
        assert!(out.status.success(), "{out:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "launch held its stdout until the step ended"
        );
    }

    #[cfg(target_os = "linux")]
    fn status(log: &std::path::Path) -> Snapshot {
        let out = sh_script(crate::step::STATUS, &[log]);
        assert!(out.status.success(), "{out:?}");
        Snapshot::parse(&String::from_utf8(out.stdout).unwrap()).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn cancel(log: &std::path::Path) -> String {
        let out = sh_script(crate::step::CANCEL, &[log]);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn zombie_or_gone(pid: &str) -> bool {
        read(std::path::Path::new(&format!("/proc/{}/stat", pid.trim())))
            .rsplit_once(") ")
            .is_none_or(|(_, r)| r.starts_with('Z'))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn builds_record_exits_timeouts_and_mirror_their_log() {
        let dir = tempfile::tempdir().unwrap();
        let run = |name: &str, secs: &str, cmd: &[&str]| {
            let log = dir.path().join(name).join("logs/build.log");
            launch(&log, secs, cmd);
            wait_for(name, || !read(&ext(&log, "term")).is_empty());
            let log_text = read(&log);
            wait_for(name, || read(&ext(&log, "mirror")) == log_text);
            assert_eq!(read(&ext(&log, "term")), read(&ext(&log, "exit")), "{name}");
            (status(&log).state, log_text)
        };

        let (state, log) = run("ok", "30", &["sh", "-c", "echo hi; exit 3"]);
        assert_eq!(
            state,
            State::Exited {
                code: 3,
                timed_out: false
            }
        );
        assert_eq!(log, "$ sh -c echo hi; exit 3\nhi\n");
        let (state, _) = run("own-124", "30", &["sh", "-c", "exit 124"]);
        assert_eq!(
            state,
            State::Exited {
                code: 124,
                timed_out: false
            }
        );
        let (state, _) = run("slow", "1", &["sleep", "30"]);
        assert_eq!(
            state,
            State::Exited {
                code: 124,
                timed_out: true
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancel_wins_once_and_kills_the_stage() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("build.log");
        launch(&log, "60", &["sleep", "57"]);
        wait_for("the stage", || !read(&ext(&log, "stage")).is_empty());
        assert_eq!(status(&log).state, State::Running);
        assert_eq!(cancel(&log), "cancelled\n");
        wait_for("the termination record", || {
            !read(&ext(&log, "term")).is_empty()
        });
        assert_eq!(read(&ext(&log, "term")), "cancelled\n");
        assert_eq!(status(&log).state, State::Cancelled);
        assert_eq!(cancel(&log), "ended\n");
        let stage = read(&ext(&log, "stage"));
        wait_for("the stage to die", || zombie_or_gone(&stage));
        assert_eq!(status(&log).state, State::Cancelled);

        let done = dir.path().join("done.log");
        launch(&done, "60", &["true"]);
        wait_for("the exit", || !read(&ext(&done, "exit")).is_empty());
        assert_eq!(cancel(&done), "ended\n");
        assert_eq!(read(&ext(&done, "exit")), "0\n");
        assert_eq!(
            status(&done).state,
            State::Exited {
                code: 0,
                timed_out: false
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_reports_dead_and_zombie_runners_as_lost() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("build.log");
        std::fs::write(&log, "").unwrap();
        assert_eq!(
            sh_script(crate::step::STATUS, &[&log]).status.code(),
            Some(3)
        );
        std::fs::write(ext(&log, "started"), "2026-10-08T00:00:00Z\n").unwrap();
        assert_eq!(status(&log).state, State::Lost);
        std::fs::write(ext(&log, "pid"), "not-a-pid\n").unwrap();
        assert_eq!(status(&log).state, State::Lost);

        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(ext(&log, "pid"), format!("{}\n", sleeper.id())).unwrap();
        assert_eq!(status(&log).state, State::Running);
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();

        let mut zombie = std::process::Command::new("true").spawn().unwrap();
        let pid = zombie.id().to_string();
        wait_for("a zombie", || zombie_or_gone(&pid));
        std::fs::write(ext(&log, "pid"), format!("{pid}\n")).unwrap();
        assert_eq!(status(&log).state, State::Lost);

        std::fs::write(ext(&log, "exit"), "124 timeout\n").unwrap();
        assert_eq!(
            status(&log).state,
            State::Exited {
                code: 124,
                timed_out: true
            }
        );
        zombie.wait().unwrap();
    }
}
