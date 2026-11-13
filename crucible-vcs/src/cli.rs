//! The `git` binary, for what libgit2 does not do: clones and fetches over the system's TLS and
//! credential setup, partial clones, worktrees and patches.
//!
//! ```text
//!   git(dir, args) ─┬─ read()      stdout as text; a failure carries git's stderr
//!                   ├─ run()       output left on the caller's stdout and stderr
//!                   └─ succeeds()  whether git exited zero
//! ```

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("running git (is it on PATH?)")]
    Spawn(#[source] std::io::Error),
    #[error("git {subcommand} failed ({status}){}", detail(.stderr))]
    Failed {
        subcommand: String,
        status: std::process::ExitStatus,
        /// What git wrote to stderr, when it was captured.
        stderr: String,
    },
}

fn detail(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

enum Stdin {
    Null,
    Bytes(Vec<u8>),
    File(std::fs::File),
}

/// One git invocation, built by [`git`].
pub struct Git {
    dir: Option<PathBuf>,
    args: Vec<OsString>,
    stdin: Stdin,
}

/// `git <args>`, run in `dir` when one is given. A failure names the subcommand alone, since an
/// argument may be a URL carrying a credential.
pub fn git<'a, D, I, A>(dir: D, args: I) -> Git
where
    D: Into<Option<&'a Path>>,
    I: IntoIterator<Item = A>,
    A: AsRef<OsStr>,
{
    Git {
        dir: dir.into().map(Path::to_path_buf),
        args: args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect(),
        stdin: Stdin::Null,
    }
}

impl Git {
    /// Feed `bytes` to git's stdin.
    pub fn stdin_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Stdin::Bytes(bytes.into());
        self
    }

    /// Give git `file` as its stdin.
    pub fn stdin_file(mut self, file: std::fs::File) -> Self {
        self.stdin = Stdin::File(file);
        self
    }

    /// The first argument that is neither an option nor the value of `-c` or `-C`.
    fn subcommand(&self) -> String {
        let mut args = self.args.iter().map(|arg| arg.to_string_lossy());
        while let Some(arg) = args.next() {
            if arg == "-c" || arg == "-C" {
                args.next();
            } else if !arg.starts_with('-') {
                return arg.into_owned();
            }
        }
        String::new()
    }

    fn expression(self) -> duct::Expression {
        let mut args: Vec<OsString> = Vec::with_capacity(self.args.len() + 2);
        if let Some(dir) = self.dir {
            args.push("-C".into());
            args.push(dir.into());
        }
        args.extend(self.args);
        let expression = duct::cmd("git", args).unchecked();
        match self.stdin {
            Stdin::Null => expression.stdin_null(),
            Stdin::Bytes(bytes) => expression.stdin_bytes(bytes),
            Stdin::File(file) => expression.stdin_file(file),
        }
    }

    /// Run git and return what it wrote to stdout. A failure carries what it wrote to stderr.
    pub fn read(self) -> Result<String, GitError> {
        let subcommand = self.subcommand();
        let out = self
            .expression()
            .stdout_capture()
            .stderr_capture()
            .run()
            .map_err(GitError::Spawn)?;
        if !out.status.success() {
            return Err(GitError::Failed {
                subcommand,
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run git with its output on this process's stdout and stderr. A failure carries the exit
    /// status alone.
    pub fn run(self) -> Result<(), GitError> {
        let subcommand = self.subcommand();
        let out = self.expression().run().map_err(GitError::Spawn)?;
        if !out.status.success() {
            return Err(GitError::Failed {
                subcommand,
                status: out.status,
                stderr: String::new(),
            });
        }
        Ok(())
    }

    /// Whether git exited zero, with its output discarded.
    pub fn succeeds(self) -> Result<bool, GitError> {
        let out = self
            .expression()
            .stdout_null()
            .stderr_null()
            .run()
            .map_err(GitError::Spawn)?;
        Ok(out.status.success())
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{GitError, git};
    use std::path::Path;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct Repo(std::path::PathBuf);
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn repo() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "crucible-git-cli-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git(dir.as_path(), ["init", "-q"]).run().unwrap();
        Repo(dir)
    }

    #[test]
    fn read_returns_stdout_and_a_failure_carries_stderr_and_the_subcommand() {
        let repo = repo();
        let inside = git(repo.0.as_path(), ["rev-parse", "--is-inside-work-tree"]);
        assert_eq!(inside.read().unwrap(), "true\n");

        let err = git(
            repo.0.as_path(),
            [
                "-c",
                "core.quotePath=false",
                "rev-parse",
                "--verify",
                "nope",
            ],
        )
        .read()
        .unwrap_err();
        let GitError::Failed {
            subcommand,
            status,
            stderr,
        } = &err
        else {
            panic!("{err:?}");
        };
        assert_eq!(subcommand, "rev-parse");
        assert!(!status.success());
        assert!(stderr.contains("Needed a single revision"), "{stderr}");
        assert!(
            err.to_string().starts_with("git rev-parse failed ("),
            "{err}"
        );
    }

    #[test]
    fn a_failure_never_names_an_argument() {
        let repo = repo();
        let secret = "https://user:hunter2@example.invalid/r.git";
        let err = git(repo.0.as_path(), ["-C", ".", "frobnicate", secret])
            .run()
            .unwrap_err();
        assert!(matches!(&err, GitError::Failed { subcommand, stderr, .. }
            if subcommand == "frobnicate" && stderr.is_empty()));
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn succeeds_reports_the_exit_status_without_failing() {
        let repo = repo();
        assert!(
            git(repo.0.as_path(), ["rev-parse", "--git-dir"])
                .succeeds()
                .unwrap()
        );
        assert!(
            !git(repo.0.as_path(), ["rev-parse", "--verify", "nope"])
                .succeeds()
                .unwrap()
        );
        assert!(
            !git(Path::new("/nonexistent"), ["status"])
                .succeeds()
                .unwrap()
        );
    }

    #[test]
    fn stdin_carries_more_than_a_pipe_buffers_while_stdout_is_read() {
        let repo = repo();
        let ids: Vec<String> = (0..20_000).map(|i| format!("{i:040x}")).collect();
        let listed = git(repo.0.as_path(), ["cat-file", "--batch-check"])
            .stdin_bytes(ids.join("\n"))
            .read()
            .unwrap();
        assert_eq!(listed.lines().count(), ids.len());
        assert!(listed.lines().all(|line| line.ends_with(" missing")));

        let path = repo.0.join("ids");
        std::fs::write(&path, ids.join("\n")).unwrap();
        let from_file = git(repo.0.as_path(), ["cat-file", "--batch-check"])
            .stdin_file(std::fs::File::open(&path).unwrap())
            .read()
            .unwrap();
        assert_eq!(from_file, listed);
    }

    #[test]
    fn no_directory_runs_git_where_the_process_is() {
        assert!(
            git(None, ["--version"])
                .read()
                .unwrap()
                .starts_with("git version")
        );
    }
}
