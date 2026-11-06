//! A build's step runner, the main process of its build pod (`jobs::build_job`). It keeps the
//! build's state in files next to its log:
//!
//! ```text
//! <log>            stage output, each stage headed by `$ argv`
//! <log>.stage      pid of the running stage's `timeout`
//! <log>.exit       `<code>[ timeout]`, hard-linked into place, first writer wins
//! ```
//!
//! A build also stamps each `STEP`, `COMMIT` and `CACHE` line of its log, and its end, into a
//! steps file: `<unix ns> <line>`, then `<unix ns> END <exit record>` once the runner has exited.
//!
//! A build with cluster caches restores their snapshots before `bud` and, when `bud` succeeds and
//! the build may write to the shared cache, saves them after it ([`RESTORE`], [`SAVE`]).
#![allow(clippy::disallowed_macros)]

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const FETCH_LIMIT: u64 = 512 * 1024 * 1024;
/// Where the runner writes its exit record and log path when the build pod passes no other file.
pub const TERMINATION_LOG: &str = "/tmp/buildit-runner.term";

/// Arguments: the log, the build's timeout, the termination file, the steps file, the snapshot
/// plan, the shared cache's URL, the save header file, [`RESTORE`], [`SAVE`], then `bud`'s argv.
pub(crate) const RUNNER: &str = r#"log=$1; shift; t=
stage() {
  secs=$1; shift
  printf '$ %s\n' "$*" >> "$log"
  s=$(date +%s)
  timeout "$secs" "$@" >> "$log" 2>&1 < /dev/null
  c=$?; t=
  [ "$c" -ne 0 ] && [ $(($(date +%s) - s)) -ge "$secs" ] && t=" timeout"
  return "$c"
}
secs=$1; term=$2; steps=$3; plan=$4; url=$5; auth=$6; restore=$7; save=$8; shift 8
sleep 2147483647 & hold=$!
{
  tail -n +1 -f -s 0.1 --pid=$hold "$log" 2> /dev/null \
    | grep --line-buffered -E '^(STEP |COMMIT |CACHE )' \
    | while IFS= read -r l; do printf '%s %s\n' "$(date +%s%N)" "$l"; done
  n=50; while [ ! -s "$log.exit" ] && [ "$n" -gt 0 ]; do sleep 0.1; n=$((n - 1)); done
  printf '%s END %s\n' "$(date +%s%N)" "$(head -n 1 "$log.exit" 2> /dev/null)"
} >> "$steps" &
if [ -n "$plan" ]; then
  printf '%s\n' "$restore" > "$log.restore"
  stage 900 sh "$log.restore" "$plan" "$url"
fi
stage "$secs" "$@"; c=$?; bt=$t
if [ "$c" -eq 0 ] && [ -n "$plan" ] && [ -s "$auth" ]; then
  printf '%s\n' "$save" > "$log.save"
  stage 900 sh "$log.save" "$plan" "$url" "$auth"
fi
t=$bt
kill "$hold" 2> /dev/null; wait "$hold" 2> /dev/null
printf '%s%s\n' "$c" "$t" > "$log.exit"
[ -n "$term" ] && { cat "$log.exit"; printf '%s\n' "$log"; } > "$term" 2>/dev/null
exit 0"#;

/// Restores each cluster cache in a build's snapshot plan, `<name> <key> <dir> <use>` per line,
/// whose directory is empty: the snapshot under its key, else the newest one under its name.
/// Arguments: the plan and the shared cache's URL.
pub(crate) const RESTORE: &str = r#"plan=$1; url=$2
while read -r name key dir use; do
  echo "CACHE restore $name"
  if [ -n "$(ls -A "$dir" 2> /dev/null)" ]; then
    echo "cache $name: kept in this pod"
    continue
  fi
  newest=$(curl -fsS --max-time 30 "$url/dirs/$name/newest" 2> /dev/null)
  got=
  for k in "$key" "$newest"; do
    case $k in "" | *[!0-9a-f]*) continue ;; esac
    rm -rf "$dir.part" "$dir.snap"
    if curl -fsS -o "$dir.snap" "$url/dirs/$name/$k" 2> /dev/null && mkdir -p "$dir.part" \
      && tar --zstd -xf "$dir.snap" -C "$dir.part"; then
      rm -rf "$dir" && mv "$dir.part" "$dir" && got=$k
    fi
    rm -rf "$dir.part" "$dir.snap"
    [ -n "$got" ] && break
  done
  case $got in
    "") echo "cache $name: no snapshot" ;;
    "$key") echo "cache $name: restored $key" ;;
    *) echo "cache $name: restored the newest snapshot, $got" ;;
  esac
done < "$plan""#;

/// Saves each cluster cache in a build's snapshot plan marked `save` under its key, unless the
/// key has a snapshot already or the cache is empty, and points its name's newest at it.
/// Arguments: the plan, the shared cache's URL, and a file holding the `Authorization` header.
pub(crate) const SAVE: &str = r#"plan=$1; url=$2; auth=$3
while read -r name key dir use; do
  [ "$use" = save ] || continue
  echo "CACHE save $name"
  if curl -fsS -o /dev/null -I "$url/dirs/$name/$key" 2> /dev/null; then
    echo "cache $name: $key is saved already"
    continue
  fi
  if [ -z "$(ls -A "$dir" 2> /dev/null)" ]; then
    echo "cache $name: empty, not saved"
    continue
  fi
  rm -f "$dir.snap"
  if tar --zstd -cf "$dir.snap" -C "$dir" . \
    && curl -fsS -o /dev/null -H @"$auth" -T "$dir.snap" "$url/dirs/$name/$key" \
    && printf '%s' "$key" | curl -fsS -o /dev/null -H @"$auth" -T - "$url/dirs/$name/newest"; then
    echo "cache $name: saved $key, $(($(wc -c < "$dir.snap") / 1048576)) MiB"
  else
    echo "cache $name: not saved"
  fi
  rm -f "$dir.snap"
done < "$plan""#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A step that has just started.
    pub fn running(at: &str) -> Self {
        Self {
            created: at.to_string(),
            updated: at.to_string(),
            state: State::Running,
            fetch: Fetch::Nothing,
            published: None,
        }
    }
}

/// Where a build's cluster cache snapshots are handled: the plan [`RESTORE`] and [`SAVE`] read,
/// the shared cache's URL, and the file holding the header a save sends, which exists only when
/// the build may save.
pub struct SnapshotFiles {
    pub plan: String,
    pub url: String,
    pub auth: String,
}

/// `steps` is the file the build stamps its steps into.
pub fn build_args(
    limit: Duration,
    steps: &str,
    snapshots: Option<&SnapshotFiles>,
    cmd: &[String],
) -> Vec<String> {
    let (plan, url, auth) = match snapshots {
        Some(s) => (s.plan.as_str(), s.url.as_str(), s.auth.as_str()),
        None => ("", "", ""),
    };
    let mut args = vec![
        limit.as_secs().to_string(),
        TERMINATION_LOG.to_string(),
        steps.to_string(),
        plan.to_string(),
        url.to_string(),
        auth.to_string(),
        RESTORE.to_string(),
        SAVE.to_string(),
    ];
    args.extend_from_slice(cmd);
    args
}

#[cfg(test)]
mod tests {
    use crate::step::{Published, State, Status};

    #[test]
    fn exit_records_read_as_states() {
        let exited = |code, timed_out| State::Exited { code, timed_out };
        let cases = [
            ("0", exited(0, false), Status::Succeeded, Some(0), false),
            ("1", exited(1, false), Status::Failed, Some(1), false),
            ("124", exited(124, false), Status::Failed, Some(124), false),
            (
                "124 timeout",
                exited(124, true),
                Status::Failed,
                Some(124),
                true,
            ),
            (
                "137 timeout",
                exited(137, true),
                Status::Failed,
                Some(137),
                true,
            ),
            (
                "cancelled",
                State::Cancelled,
                Status::Cancelled,
                None,
                false,
            ),
        ];
        for (raw, state, status, exit, timed_out) in cases {
            let parsed = State::record(raw).unwrap();
            assert_eq!(parsed, state, "{raw:?}");
            assert_eq!(parsed.status(), status, "{raw:?}");
            assert_eq!(parsed.exit(), exit, "{raw:?}");
            assert_eq!(parsed.timed_out(), timed_out, "{raw:?}");
        }
        for bad in [
            "",
            "x",
            "1 2",
            "1 timeout x",
            "timeout",
            "cancelled timeout",
        ] {
            assert_eq!(State::record(bad), None, "{bad:?}");
        }
        assert_eq!(State::Running.status(), Status::Running);
        assert_eq!(State::Lost.status(), Status::Failed);
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
    fn read(p: &std::path::Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    fn ext(log: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}.{suffix}", log.display()))
    }

    /// The runner as a build pod runs it, to its end.
    #[cfg(target_os = "linux")]
    fn build(log: &std::path::Path, secs: &str, cmd: &[&str]) {
        use std::path::Path;
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let (term, steps) = (ext(log, "term"), ext(log, "steps"));
        let mut args = vec![log, Path::new(secs), &term, &steps];
        args.extend(["", "", ""].map(Path::new));
        args.extend([crate::step::RESTORE, crate::step::SAVE].map(Path::new));
        args.extend(cmd.iter().map(Path::new));
        let out = std::process::Command::new("sh")
            .args(["-c", crate::step::RUNNER, "sh"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn builds_record_exits_and_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let run = |name: &str, secs: &str, cmd: &[&str]| {
            let log = dir.path().join(name).join("build.log");
            build(&log, secs, cmd);
            let record = format!("{}{}\n", read(&ext(&log, "exit")), log.display());
            assert_eq!(read(&ext(&log, "term")), record, "{name}");
            (
                State::record(read(&ext(&log, "exit")).trim()).unwrap(),
                read(&log),
            )
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
    fn a_build_stamps_its_steps_and_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/build.log");
        let script = "echo 'STEP 1/2: FROM base'; echo noise; sleep 1; \
                      echo 'STEP 2/2: RUN make'; echo 'COMMIT img'; exit 4";
        build(&log, "30", &["sh", "-c", script]);
        let steps = ext(&log, "steps");
        for _ in 0..50 {
            if read(&steps).lines().count() == 4 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let stamped: Vec<(u128, String)> = read(&steps)
            .lines()
            .map(|line| {
                let (at, text) = line.split_once(' ').unwrap();
                (at.parse().unwrap(), text.to_string())
            })
            .collect();
        let texts: Vec<&str> = stamped.iter().map(|(_, t)| t.as_str()).collect();
        assert!(texts.contains(&"STEP 1/2: FROM base"), "{texts:?}");
        assert!(texts.contains(&"STEP 2/2: RUN make"), "{texts:?}");
        assert!(texts.contains(&"COMMIT img"), "{texts:?}");
        assert!(texts.contains(&"END 4"), "{texts:?}");
        let at = |text: &str| stamped.iter().find(|(_, t)| t == text).unwrap().0;
        assert!(
            at("STEP 2/2: RUN make") - at("STEP 1/2: FROM base") >= 900_000_000,
            "{stamped:?}"
        );
        assert!(at("END 4") >= at("COMMIT img"), "{stamped:?}");
    }
}
