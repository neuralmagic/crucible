//! A command's output while it runs: each line echoed as `[label] line`, recorded in batches
//! for the session log, and the last stdout line and stderr tail kept for the result.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::deadline::{Deadline, Supervised};
use crucible_contract::event::RawStream;
use crucible_contract::session::SessionEvent;

/// Non-empty stderr lines a command keeps, newest last.
pub const STDERR_LINES: usize = 40;

/// How a command ended: on its own, or killed at its deadline.
pub enum Finished {
    Exited(Captured),
    Killed(Deadline),
}

/// What an exited command left behind: its status, its last non-empty stdout line, and the last
/// [`STDERR_LINES`] non-empty stderr lines, oldest first.
pub struct Captured {
    pub status: std::process::ExitStatus,
    pub result_line: Option<String>,
    pub stderr_lines: Vec<String>,
}

impl Captured {
    pub fn stderr(&self) -> String {
        self.stderr_lines.join("\n")
    }
}

/// What a command printed, as the session log records it.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// Lines from one stream, in arrival order.
    Lines {
        stream: RawStream,
        lines: Vec<String>,
    },
    /// Lines past [`ECHO_BYTES`], counted and not recorded.
    Dropped(u64),
}

impl Output {
    pub fn into_event(self, task: String, attempt: u32) -> SessionEvent {
        match self {
            Output::Lines { stream, lines } => SessionEvent::TaskOutput {
                task,
                attempt,
                stream,
                lines,
            },
            Output::Dropped(lines) => SessionEvent::TaskOutputDropped {
                task,
                attempt,
                lines,
            },
        }
    }
}

/// Longest stdout line kept as the result. Past it the line is clipped, so it fails as JSON.
const RESULT_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Longest stderr line kept.
const STDERR_LINE_BYTES: usize = 4096;
/// Longest line echoed or recorded; the rest is replaced by `…`.
const ECHO_LINE_BYTES: usize = 2048;
/// Bytes echoed per command, prefixes included, before the rest is counted instead.
const ECHO_BYTES: usize = 1024 * 1024;
/// A recorded batch is flushed once it holds this many lines or is this old.
const BATCH_LINES: usize = 100;
const BATCH_AGE: Duration = Duration::from_millis(250);
/// Lines a pipe reader may queue ahead of the echo.
const LINE_QUEUE: usize = 1024;

/// Run `cmd` under an optional deadline. Stdin is closed. Each non-blank output line is echoed to
/// `sink` as `[label] line` as it arrives and handed to `record` in batches; both stop at the
/// same cap. Each pipe is read on its own thread so a command filling one cannot wedge the
/// other, and the echo runs on the calling thread.
pub fn run_echoed(
    cmd: &mut Command,
    deadline: Option<Deadline>,
    label: &str,
    sink: impl Write,
    record: impl FnMut(Output),
) -> std::io::Result<Finished> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Supervised::spawn(cmd, deadline)?;
    let stdout = child.child().stdout.take();
    let stderr = child.child().stderr.take();
    let mut echo = Echo::new(label, sink, record);
    let (stdout_tx, lines) = mpsc::sync_channel(LINE_QUEUE);
    let stderr_tx = stdout_tx.clone();
    let (result_line, stderr_lines) = std::thread::scope(|scope| {
        let stderr_thread = scope.spawn(move || {
            let mut tail = VecDeque::with_capacity(STDERR_LINES);
            each_line(stderr, STDERR_LINE_BYTES, |line| {
                queue(&stderr_tx, RawStream::Stderr, line);
                let line = String::from_utf8_lossy(line);
                if !line.trim().is_empty() {
                    if tail.len() == STDERR_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line.into_owned());
                }
            });
            Vec::from(tail)
        });
        let stdout_thread = scope.spawn(move || {
            let mut result_line = None;
            each_line(stdout, RESULT_LINE_BYTES, |line| {
                queue(&stdout_tx, RawStream::Stdout, line);
                if !line.trim_ascii().is_empty() {
                    result_line = Some(String::from_utf8_lossy(line).into_owned());
                }
            });
            result_line
        });
        echo.pump(&lines);
        (
            stdout_thread.join().unwrap_or_default(),
            stderr_thread.join().unwrap_or_default(),
        )
    });
    echo.finish();
    let reaped = child.wait()?;
    if let Some(deadline) = reaped.killed_at {
        return Ok(Finished::Killed(deadline));
    }
    Ok(Finished::Exited(Captured {
        status: reaped.status,
        result_line,
        stderr_lines,
    }))
}

/// Call `f` with each line read from `pipe`, without its `\n` or `\r\n`, clipped to `max` bytes.
fn each_line(pipe: Option<impl Read>, max: usize, mut f: impl FnMut(&[u8])) {
    let Some(pipe) = pipe else {
        return;
    };
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if chunk.is_empty() {
            break;
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let taken = newline.unwrap_or(chunk.len());
        let room = max.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..taken.min(room)]);
        reader.consume(newline.map_or(taken, |at| at + 1));
        if newline.is_some() {
            f(line.strip_suffix(b"\r").unwrap_or(&line));
            line.clear();
        }
    }
    if !line.is_empty() {
        f(line.strip_suffix(b"\r").unwrap_or(&line));
    }
}

/// One output line on its way to the echo, already clipped to [`ECHO_LINE_BYTES`].
struct Line {
    stream: RawStream,
    text: String,
}

fn queue(lines: &mpsc::SyncSender<Line>, stream: RawStream, line: &[u8]) {
    if line.trim_ascii().is_empty() {
        return;
    }
    let clipped = &line[..line.len().min(ECHO_LINE_BYTES)];
    let mut text = String::from_utf8_lossy(clipped).into_owned();
    if clipped.len() < line.len() {
        text.push('…');
    }
    let _ = lines.send(Line { stream, text });
}

/// A command's output echo and its recorded batches, capped at [`ECHO_BYTES`].
struct Echo<'a, W, R> {
    label: &'a str,
    sink: W,
    record: R,
    shown: usize,
    hidden: u64,
    batch: Option<Batch>,
}

struct Batch {
    stream: RawStream,
    lines: Vec<String>,
    since: Instant,
}

impl<'a, W: Write, R: FnMut(Output)> Echo<'a, W, R> {
    fn new(label: &'a str, sink: W, record: R) -> Self {
        Echo {
            label,
            sink,
            record,
            shown: 0,
            hidden: 0,
            batch: None,
        }
    }

    /// Take lines until both readers hang up, flushing a batch once it is [`BATCH_AGE`] old.
    fn pump(&mut self, lines: &mpsc::Receiver<Line>) {
        loop {
            let received = match &self.batch {
                Some(batch) => lines.recv_timeout(BATCH_AGE.saturating_sub(batch.since.elapsed())),
                None => lines
                    .recv()
                    .map_err(|mpsc::RecvError| mpsc::RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(line) => self.line(line),
                Err(mpsc::RecvTimeoutError::Timeout) => self.flush(),
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn line(&mut self, Line { stream, text }: Line) {
        if self.shown >= ECHO_BYTES {
            self.hidden += 1;
            return;
        }
        let echoed = format!("[{}] {text}\n", self.label);
        self.shown += echoed.len();
        self.emit(&echoed);
        if self
            .batch
            .as_ref()
            .is_some_and(|batch| batch.stream != stream)
        {
            self.flush();
        }
        let batch = self.batch.get_or_insert_with(|| Batch {
            stream,
            lines: Vec::new(),
            since: Instant::now(),
        });
        batch.lines.push(text);
        if batch.lines.len() >= BATCH_LINES || batch.since.elapsed() >= BATCH_AGE {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if let Some(Batch { stream, lines, .. }) = self.batch.take() {
            (self.record)(Output::Lines { stream, lines });
        }
    }

    fn finish(mut self) {
        self.flush();
        if self.hidden > 0 {
            let notice = format!("[{}] … {} more lines not shown\n", self.label, self.hidden);
            self.emit(&notice);
            (self.record)(Output::Dropped(self.hidden));
        }
    }

    fn emit(&mut self, text: &str) {
        let _ = self.sink.write_all(text.as_bytes());
        let _ = self.sink.flush();
    }
}

/// The task's JSON result, where its last non-empty stdout line carries one.
pub fn json_line(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line.trim()).ok()
}

#[cfg(test)]
mod tests {
    use crate::deadline::Deadline;
    use crate::plan::output::*;

    /// A sink that stamps each write, to show lines reach it while the command still runs.
    #[derive(Default)]
    struct Stamped(Vec<(std::time::Instant, String)>);
    impl std::io::Write for &mut Stamped {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push((
                std::time::Instant::now(),
                String::from_utf8_lossy(buf).into_owned(),
            ));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn echoed(
        script: &str,
        label: &str,
        deadline: Option<Deadline>,
    ) -> (Finished, String, Vec<Output>) {
        let mut sink = Vec::new();
        let mut recorded = Vec::new();
        let finished = run_echoed(
            std::process::Command::new("sh").arg("-c").arg(script),
            deadline,
            label,
            &mut sink,
            |output| recorded.push(output),
        )
        .unwrap();
        (finished, String::from_utf8(sink).unwrap(), recorded)
    }

    fn stdout(lines: &[&str]) -> Output {
        Output::Lines {
            stream: RawStream::Stdout,
            lines: lines.iter().map(|line| line.to_string()).collect(),
        }
    }

    fn stderr(lines: &[&str]) -> Output {
        Output::Lines {
            stream: RawStream::Stderr,
            lines: lines.iter().map(|line| line.to_string()).collect(),
        }
    }

    fn exited(finished: Finished) -> Captured {
        match finished {
            Finished::Exited(captured) => captured,
            Finished::Killed(deadline) => panic!("killed at {}", deadline.note()),
        }
    }

    fn lines_of<'a>(echo: &'a str, stream: &str) -> Vec<&'a str> {
        echo.lines().filter(|line| line.contains(stream)).collect()
    }

    #[test]
    fn output_lines_echo_prefixed_with_the_instance_name_and_in_order() {
        let (finished, echo, _) = echoed(
            "echo out1; echo err1 >&2; echo; echo out2; echo '   '; echo err2 >&2; \
             printf 'out3\\r\\n'; printf 'err3' >&2; echo '{\"score\": 3}'",
            "task[3]",
            None,
        );
        let captured = exited(finished);
        assert!(captured.status.success());
        assert_eq!(
            lines_of(&echo, "out"),
            ["[task[3]] out1", "[task[3]] out2", "[task[3]] out3"]
        );
        assert_eq!(
            lines_of(&echo, "err"),
            ["[task[3]] err1", "[task[3]] err2", "[task[3]] err3"]
        );
        assert_eq!(lines_of(&echo, "score"), [r#"[task[3]] {"score": 3}"#]);
        assert_eq!(echo.lines().count(), 7, "blank lines were echoed: {echo}");
        assert_eq!(captured.result_line.as_deref(), Some(r#"{"score": 3}"#));
        assert_eq!(captured.stderr_lines, ["err1", "err2", "err3"]);
    }

    #[test]
    fn a_line_reaches_the_sink_while_the_command_still_runs() {
        let mut sink = Stamped::default();
        let started = std::time::Instant::now();
        let finished = run_echoed(
            std::process::Command::new("sh")
                .arg("-c")
                .arg("echo early; sleep 1; echo '{}'"),
            None,
            "slow",
            &mut sink,
            |_| {},
        )
        .unwrap();
        let ended = std::time::Instant::now();
        assert!(exited(finished).status.success());
        let (at, line) = &sink.0[0];
        assert_eq!(line, "[slow] early\n");
        assert!(
            ended.duration_since(*at) >= std::time::Duration::from_millis(500),
            "the line waited for the exit: echoed {:?} after the start, exit at {:?}",
            at.duration_since(started),
            ended.duration_since(started)
        );
    }

    #[test]
    fn a_flood_on_both_pipes_neither_wedges_nor_echoes_past_the_cap() {
        let lines = 200_000;
        let (finished, echo, recorded) = echoed(
            &format!(
                "yes err | head -n {lines} >&2; yes noise | head -n {lines}; \
                 yes err | head -n {lines} >&2; echo '{{\"ok\": 1}}'"
            ),
            "flood",
            None,
        );
        let captured = exited(finished);
        assert!(captured.status.success());
        assert_eq!(captured.result_line.as_deref(), Some(r#"{"ok": 1}"#));
        assert_eq!(captured.stderr_lines.len(), STDERR_LINES);
        assert!(
            echo.len() <= ECHO_BYTES + ECHO_LINE_BYTES + 64,
            "echoed {} bytes",
            echo.len()
        );
        let notice = echo.lines().last().unwrap();
        let shown = echo.lines().count() - 1;
        let hidden = 3 * lines + 1 - shown;
        assert_eq!(notice, format!("[flood] … {hidden} more lines not shown"));
        let (last, batches) = recorded.split_last().unwrap();
        assert_eq!(last, &Output::Dropped(u64::try_from(hidden).unwrap()));
        let echoed: Vec<String> = echo
            .lines()
            .take(shown)
            .map(|line| line.strip_prefix("[flood] ").unwrap().to_string())
            .collect();
        let batched: Vec<String> = batches
            .iter()
            .flat_map(|batch| match batch {
                Output::Lines { lines, .. } => {
                    assert!(lines.len() <= BATCH_LINES);
                    lines.clone()
                }
                Output::Dropped(_) => panic!("a drop count before the last batch"),
            })
            .collect();
        assert_eq!(batched, echoed, "the record and the echo disagree");
    }

    #[test]
    fn a_long_line_echoes_clipped_and_still_parses_as_the_result() {
        let (finished, echo, recorded) = echoed(
            r#"head -c 100000 /dev/zero | tr '\0' x; echo; printf '{"blob": "%s"}\n' "$(head -c 50000 /dev/zero | tr '\0' y)""#,
            "long",
            None,
        );
        let captured = exited(finished);
        let echoed: Vec<&str> = echo.lines().collect();
        assert_eq!(echoed.len(), 2);
        for line in echoed {
            assert_eq!(line.len(), "[long] ".len() + ECHO_LINE_BYTES + "…".len());
            assert!(line.ends_with('…'));
        }
        let result = json_line(captured.result_line.as_deref().unwrap()).unwrap();
        assert_eq!(result["blob"].as_str().unwrap().len(), 50_000);
        let [Output::Lines { lines, .. }] = recorded.as_slice() else {
            panic!("{recorded:?}");
        };
        assert_eq!(
            lines
                .iter()
                .map(|line| format!("[long] {line}"))
                .collect::<Vec<_>>(),
            echo.lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_command_killed_at_its_deadline_reports_the_kill_after_echoing_what_it_printed() {
        let deadline =
            Deadline::for_attempt(std::time::Instant::now(), Some("1s".parse().unwrap()), None);
        let (finished, echo, recorded) = echoed("echo started; sleep 30", "hang", deadline);
        assert!(matches!(finished, Finished::Killed(_)));
        assert_eq!(echo, "[hang] started\n");
        assert_eq!(recorded, [stdout(&["started"])]);
    }

    #[test]
    fn recorded_output_batches_by_count_and_by_stream_in_arrival_order() {
        let (finished, _, recorded) = echoed(
            "seq 1 250; sleep 0.4; echo e1 >&2; echo e2 >&2; sleep 0.4; echo last",
            "batch",
            None,
        );
        assert!(exited(finished).status.success());
        let numbers = |from: u32, to: u32| (from..=to).map(|n| n.to_string()).collect::<Vec<_>>();
        let lines = |stream, lines| Output::Lines { stream, lines };
        assert_eq!(
            recorded,
            [
                lines(RawStream::Stdout, numbers(1, 100)),
                lines(RawStream::Stdout, numbers(101, 200)),
                lines(RawStream::Stdout, numbers(201, 250)),
                stderr(&["e1", "e2"]),
                stdout(&["last"]),
            ]
        );
    }

    #[test]
    fn a_partial_batch_is_recorded_once_it_is_old_not_at_exit() {
        let started = std::time::Instant::now();
        let mut recorded = Vec::new();
        let finished = run_echoed(
            std::process::Command::new("sh")
                .arg("-c")
                .arg("echo early; sleep 1.5; echo late"),
            None,
            "aged",
            std::io::sink(),
            |output| recorded.push((started.elapsed(), output)),
        )
        .unwrap();
        let ended = started.elapsed();
        assert!(exited(finished).status.success());
        let [(early_at, early), (_, late)] = recorded.as_slice() else {
            panic!("{recorded:?}");
        };
        assert_eq!(early, &stdout(&["early"]));
        assert_eq!(late, &stdout(&["late"]));
        assert!(
            *early_at >= BATCH_AGE && ended - *early_at >= std::time::Duration::from_millis(750),
            "the first batch was recorded at {early_at:?}, the command ended at {ended:?}"
        );
    }
}
