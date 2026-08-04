//! Append-only NDJSON ledger mechanics, shared by every durable record the engine keeps
//! (the run's admission ledger, the broker's step ledger).
//!
//! Domain semantics stay with the caller: this owns only the file. An append takes an
//! exclusive `flock` so a second writer (another thread, or another process holding the
//! same path) can never interleave a half line; a fold skips the blank or torn tail a
//! killed writer leaves behind; a file that cannot be read at all is moved aside rather
//! than wedging the caller.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How hard an append tries before it returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Durability {
    /// Write + flush. A power loss can still lose the tail, so the caller must treat a
    /// lost record as never-written (safe re-delivery), never as data loss.
    Flush,
    /// Write + flush + `sync_data`: the record is on the platter before `append` returns.
    Fsync,
}

/// What to do with an existing file at open time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Open {
    /// Discard it: a fresh run must not inherit the last run's records.
    Truncate,
    /// Keep it and fold it into the caller's in-memory state.
    Fold,
    /// Keep it without reading it: the caller folds on its own schedule (see
    /// [`Ledger::append_only`]).
    Keep,
}

/// What folding an existing ledger file produced.
#[derive(Debug)]
pub struct Folded<T> {
    pub records: Vec<T>,
    /// Blank or torn lines skipped (the tail a killed writer leaves).
    pub skipped: usize,
    /// Where an unreadable file was moved so the caller could start clean.
    pub quarantined: Option<PathBuf>,
}

impl<T> Default for Folded<T> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            skipped: 0,
            quarantined: None,
        }
    }
}

/// An open append-only NDJSON file.
pub struct Ledger {
    file: File,
    path: PathBuf,
    durability: Durability,
}

impl Ledger {
    /// Fold the file that is already there (per `mode`), then open it for appends.
    /// `decode` returns `None` for a line that isn't a whole record, which is skipped and
    /// counted instead of failing the open: the last line of a killed writer's file is
    /// routinely half-written.
    pub fn open<T>(
        path: &Path,
        mode: Open,
        durability: Durability,
        decode: impl Fn(&str) -> Option<T>,
    ) -> io::Result<(Self, Folded<T>)> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let folded = match mode {
            Open::Truncate => {
                // Truncate through a separate handle: `append` + `truncate` is not a legal
                // OpenOptions combination.
                if path.exists() {
                    File::create(path)?;
                }
                Folded::default()
            }
            Open::Fold => fold(path, decode),
            Open::Keep => Folded::default(),
        };
        // `read` is for the torn-tail heal below only; every write goes through `append`.
        let mut file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(path)?;
        heal_torn_tail(&mut file)?;
        Ok((
            Self {
                file,
                path: path.to_path_buf(),
                durability,
            },
            folded,
        ))
    }

    /// Open for appends without reading what is already there, for a caller that folds the
    /// file separately (or only ever writes).
    pub fn append_only(path: &Path, durability: Durability) -> io::Result<Self> {
        let (ledger, _) = Self::open(path, Open::Keep, durability, |_| None::<()>)?;
        Ok(ledger)
    }

    /// Append one record. `line` must not contain a newline (it is one NDJSON record);
    /// the terminator is added here so a caller can never write a record without one.
    pub fn append(&mut self, line: &str) -> io::Result<()> {
        let _guard = FlockGuard::acquire(&self.file)?;
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(line);
        payload.push('\n');
        self.file.write_all(payload.as_bytes())?;
        self.file.flush()?;
        if self.durability == Durability::Fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Terminate a half-written last line. The torn record itself is unrecoverable (it is
/// skipped by every fold), but without this the NEXT append would be glued onto it and
/// lost too, turning one dead record into two.
fn heal_torn_tail(file: &mut File) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        let _guard = FlockGuard::acquire(file)?;
        file.write_all(b"\n")?;
        file.flush()?;
    }
    Ok(())
}

/// Read every whole record out of `path`. A file that cannot be opened or read through is
/// quarantined (moved to `<name>.corrupt-<unix-secs>`) and folds as empty: a ledger we
/// cannot read is not one we can trust to be complete, and wedging the caller over it
/// would be worse than starting clean with the evidence kept on disk.
pub fn fold<T>(path: &Path, decode: impl Fn(&str) -> Option<T>) -> Folded<T> {
    if !path.exists() {
        return Folded::default();
    }
    let Ok(file) = File::open(path) else {
        return quarantined(path);
    };
    let mut records = Vec::new();
    let mut skipped = 0usize;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            return quarantined(path);
        };
        match decode(&line) {
            Some(record) => records.push(record),
            None => skipped += 1,
        }
    }
    Folded {
        records,
        skipped,
        quarantined: None,
    }
}

fn quarantined<T>(path: &Path) -> Folded<T> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ledger".to_string());
    let aside = path.with_file_name(format!("{name}.corrupt-{secs}"));
    let moved = std::fs::rename(path, &aside).is_ok();
    Folded {
        records: Vec::new(),
        skipped: 0,
        quarantined: moved.then_some(aside),
    }
}

/// An exclusive `flock` held for the length of one write. Holds the raw descriptor rather
/// than the `&File` so the caller can still write through the handle while locked.
struct FlockGuard {
    fd: std::os::fd::RawFd,
}

impl FlockGuard {
    fn acquire(file: &File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        // SAFETY: the caller keeps `file` (and therefore `fd`) alive past the guard.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        // SAFETY: same descriptor we locked; an unlock failure leaves the lock to be
        // released by close, so there is nothing to report.
        unsafe { libc::flock(self.fd, libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(name: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("forge-ndjson-{}-{}-{name}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ledger.jsonl")
    }

    fn ident(line: &str) -> Option<String> {
        let t = line.trim();
        (!t.is_empty() && t.ends_with(';')).then(|| t.trim_end_matches(';').to_string())
    }

    #[test]
    fn appends_survive_a_reopen_in_order() {
        let path = tmp("reopen");
        let (mut l, folded) =
            Ledger::open(&path, Open::Fold, Durability::Fsync, ident).expect("open");
        assert!(folded.records.is_empty());
        l.append("a;").expect("a");
        l.append("b;").expect("b");
        drop(l);

        let (_l, folded) = Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert_eq!(folded.records, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(folded.skipped, 0);
        assert!(folded.quarantined.is_none());
    }

    #[test]
    fn truncate_starts_empty_but_append_still_works() {
        let path = tmp("truncate");
        std::fs::write(&path, "a;\nb;\n").expect("seed");
        let (mut l, folded) =
            Ledger::open(&path, Open::Truncate, Durability::Flush, ident).expect("open");
        assert!(folded.records.is_empty());
        l.append("c;").expect("c");
        drop(l);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "c;\n");
    }

    #[test]
    fn a_torn_tail_is_skipped_and_counted() {
        let path = tmp("torn");
        // The last line was half-written when the process died: no terminator, no record.
        std::fs::write(&path, "a;\nb;\nc-half").expect("seed");
        let (mut l, folded) =
            Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert_eq!(folded.records, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(folded.skipped, 1);
        // The open healed the missing terminator, so the next record lands whole instead
        // of being glued onto the dead one.
        l.append("d;").expect("d");
        drop(l);
        let (_l, folded) = Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert_eq!(
            folded.records,
            vec!["a".to_string(), "b".to_string(), "d".to_string()]
        );
        assert_eq!(folded.skipped, 1, "only the torn line is lost");
    }

    #[test]
    fn an_unreadable_file_is_quarantined_and_folds_empty() {
        let path = tmp("quarantine");
        // A directory where the ledger should be: opening it succeeds, reading it doesn't.
        std::fs::create_dir_all(&path).expect("dir");
        let (mut l, folded) =
            Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert!(folded.records.is_empty());
        let aside = folded.quarantined.expect("moved aside");
        assert!(aside.is_dir(), "the evidence is kept, not deleted");
        l.append("a;").expect("append after quarantine");
        drop(l);
        let (_l, folded) = Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert_eq!(folded.records, vec!["a".to_string()]);
    }

    #[test]
    fn append_only_keeps_what_is_there_without_reading_it() {
        let path = tmp("append-only");
        std::fs::write(&path, "a;\n").expect("seed");
        let mut l = Ledger::append_only(&path, Durability::Flush).expect("open");
        l.append("b;").expect("b");
        drop(l);
        let (_l, folded) = Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert_eq!(folded.records, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn concurrent_appenders_never_interleave_a_line() {
        let path = tmp("flock");
        let threads: Vec<_> = (0..4u32)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let (mut l, _) =
                        Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
                    for i in 0..50 {
                        l.append(&format!("{}{};", "x".repeat(200), t * 100 + i))
                            .expect("append");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("join");
        }
        let (_l, folded) = Ledger::open(&path, Open::Fold, Durability::Flush, ident).expect("open");
        assert_eq!(folded.records.len(), 200);
        assert_eq!(folded.skipped, 0, "every line is whole");
    }
}
