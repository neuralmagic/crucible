//! The format a workload's result files take in the collector container's log: one JSON record
//! per line naming each file, then a gzipped tar of them as base64 lines, then a record with its
//! digest. The collector writes it with [`write`]; `workload_results` reads it with [`Reader`].

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

/// The most files one directory's listing names.
pub const MAX_FILES: usize = 4096;

/// Archive bytes per base64 line.
const LINE_BYTES: usize = 3060;

/// What a tar adds around each file, at most: its header, its padding, and a long name's blocks.
const TAR_OVERHEAD: u64 = 8192;

/// Room in a collector's log for what is not archive: its records and line ends.
const LOG_OVERHEAD: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "results", rename_all = "snake_case")]
pub enum Record {
    File {
        path: String,
        bytes: u64,
    },
    /// A file the archive leaves out.
    Unread {
        path: String,
        error: String,
    },
    /// The archive follows, as base64 lines.
    Archive {
        files: usize,
        bytes: u64,
    },
    /// No archive follows.
    Withheld {
        reason: String,
    },
    /// The archive is complete; `sha256` and `bytes` are of the gzipped tar.
    End {
        sha256: String,
        bytes: u64,
    },
    /// The archive broke off.
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unread {
    pub path: String,
    pub error: String,
}

/// Whether `path` names a file below the directory it is relative to.
pub fn confined(path: &str) -> bool {
    let mut components = Path::new(path).components().peekable();
    components.peek().is_some() && components.all(|c| matches!(c, Component::Normal(_)))
}

struct Found {
    path: String,
    full: PathBuf,
    bytes: u64,
}

#[derive(Default)]
struct Walk {
    files: Vec<Found>,
    unread: Vec<Unread>,
    overflowed: bool,
}

impl Walk {
    fn full(&mut self) -> bool {
        if self.files.len() + self.unread.len() >= MAX_FILES {
            self.overflowed = true;
        }
        self.overflowed
    }

    fn unread(&mut self, path: String, error: impl ToString) {
        self.unread.push(Unread {
            path,
            error: error.to_string(),
        });
    }

    fn visit(&mut self, dir: &Path, prefix: &str) {
        let shown = || match prefix {
            "" => ".".to_string(),
            p => p.to_string(),
        };
        let mut entries: Vec<fs::DirEntry> = match fs::read_dir(dir) {
            Ok(entries) => entries.filter_map(Result::ok).collect(),
            Err(e) => return self.unread(shown(), e),
        };
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if self.full() {
                return;
            }
            let name = entry.file_name();
            let path = match prefix {
                "" => name.to_string_lossy().into_owned(),
                p => format!("{p}/{}", name.to_string_lossy()),
            };
            if name.to_str().is_none() {
                self.unread(path, "its name is not UTF-8");
                continue;
            }
            let full = entry.path();
            match fs::symlink_metadata(&full) {
                Err(e) => self.unread(path, e),
                Ok(meta) if meta.is_dir() => self.visit(&full, &path),
                Ok(meta) if meta.is_file() => match File::open(&full) {
                    Ok(_) => self.files.push(Found {
                        path,
                        full,
                        bytes: meta.len(),
                    }),
                    Err(e) => self.unread(path, e),
                },
                Ok(_) => self.unread(path, "not a regular file"),
            }
        }
    }
}

/// Base64 lines of what is written, with its digest and length.
struct Encoded<W: Write> {
    out: W,
    pending: Vec<u8>,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> Encoded<W> {
    fn new(out: W) -> Self {
        Self {
            out,
            pending: Vec::new(),
            hash: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(mut self) -> io::Result<(String, u64)> {
        if !self.pending.is_empty() {
            writeln!(self.out, "{}", STANDARD.encode(&self.pending))?;
        }
        Ok((hex::encode(self.hash.finalize()), self.bytes))
    }
}

impl<W: Write> Write for Encoded<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hash.update(buf);
        self.bytes += buf.len() as u64;
        self.pending.extend_from_slice(buf);
        let (lines, rest) = self.pending.as_chunks::<LINE_BYTES>();
        for line in lines {
            writeln!(self.out, "{}", STANDARD.encode(line))?;
        }
        self.pending = rest.to_vec();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

fn record(out: &mut impl Write, record: &Record) -> io::Result<()> {
    serde_json::to_writer(&mut *out, record)?;
    out.write_all(b"\n")
}

fn archive(files: &[Found], out: &mut impl Write) -> io::Result<(String, u64)> {
    let mut encoded = Encoded::new(out);
    let mut tar = tar::Builder::new(GzEncoder::new(&mut encoded, Compression::default()));
    for found in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(found.bytes);
        header.set_mode(0o644);
        header.set_mtime(0);
        tar.append_data(
            &mut header,
            &found.path,
            File::open(&found.full)?.take(found.bytes),
        )?;
    }
    tar.into_inner()?.finish()?;
    encoded.finish()
}

/// Writes the files under `dir` to `out`: every file's record, then their archive when they
/// total at most `limit` bytes. An archive that breaks off is an error, after its record.
pub fn write(dir: &Path, limit: u64, out: &mut impl Write) -> io::Result<()> {
    let mut walk = Walk::default();
    walk.visit(dir, "");
    for found in &walk.files {
        record(
            out,
            &Record::File {
                path: found.path.clone(),
                bytes: found.bytes,
            },
        )?;
    }
    for unread in &walk.unread {
        record(
            out,
            &Record::Unread {
                path: unread.path.clone(),
                error: unread.error.clone(),
            },
        )?;
    }
    let total: u64 = walk.files.iter().map(|f| f.bytes).sum();
    let withheld = match (walk.overflowed, total > limit) {
        (true, _) => Some(format!("the directory holds more than {MAX_FILES} files")),
        (false, true) => Some(format!(
            "its files total {total} bytes, over the {limit} this server collects"
        )),
        (false, false) => None,
    };
    if let Some(reason) = withheld {
        return record(out, &Record::Withheld { reason });
    }
    record(
        out,
        &Record::Archive {
            files: walk.files.len(),
            bytes: total,
        },
    )?;
    match archive(&walk.files, out) {
        Ok((sha256, bytes)) => record(out, &Record::End { sha256, bytes }),
        Err(e) => {
            record(
                out,
                &Record::Failed {
                    error: e.to_string(),
                },
            )?;
            Err(e)
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Damaged {
    #[error("the collector's log ends before its archive does")]
    Truncated,
    #[error("a line of the archive is not base64")]
    Encoding,
    #[error("the archive is larger than its files and the {limit} byte limit allow")]
    Oversize { limit: u64 },
    #[error("the archive does not match the digest the collector wrote")]
    Digest,
    #[error("the archive does not unpack: {0}")]
    Unpack(String),
    #[error("the archive names {0:?}, which is no file below the results directory")]
    Path(String),
}

/// What the collector says became of the directory's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The log says nothing yet.
    Listing,
    /// The archive is being read.
    Archive,
    Withheld(String),
    Failed(String),
    Complete,
}

/// The bytes of collector log `workload_results` reads for a directory of `limit` bytes, at most.
pub fn log_cap(limit: u64) -> u64 {
    (limit + limit / 64) / 3 * 4 + LOG_OVERHEAD
}

/// Reads a collector's log a line at a time.
#[derive(Debug)]
pub struct Reader {
    pub files: Vec<Entry>,
    pub unread: Vec<Unread>,
    pub state: State,
    limit: u64,
    archive: Vec<u8>,
}

impl Reader {
    pub fn new(limit: u64) -> Self {
        Self {
            files: Vec::new(),
            unread: Vec::new(),
            state: State::Listing,
            limit,
            archive: Vec::new(),
        }
    }

    /// Whether every file the collector found is in `files` and `unread`.
    pub fn listed(&self) -> bool {
        self.state != State::Listing
    }

    /// The most bytes the listed files' tar unpacks to.
    fn tar_cap(&self) -> u64 {
        self.limit + (self.files.len() as u64 + 1) * TAR_OVERHEAD
    }

    /// The most bytes the listed files' gzipped tar takes.
    fn archive_cap(&self) -> u64 {
        self.tar_cap() + self.limit / 64 + 65536
    }

    /// Whether the log holds nothing more to read.
    pub fn ended(&self) -> bool {
        !matches!(self.state, State::Listing | State::Archive)
    }

    pub fn line(&mut self, line: &str) -> Result<(), Damaged> {
        let line = line.trim_end();
        if self.ended() || line.is_empty() {
            return Ok(());
        }
        if line.starts_with('{') {
            let Ok(record) = serde_json::from_str::<Record>(line) else {
                return Ok(());
            };
            return self.record(record);
        }
        if self.state != State::Archive {
            return Ok(());
        }
        let bytes = STANDARD.decode(line).map_err(|_| Damaged::Encoding)?;
        self.archive.extend_from_slice(&bytes);
        if self.archive.len() as u64 > self.archive_cap() {
            return Err(Damaged::Oversize { limit: self.limit });
        }
        Ok(())
    }

    fn record(&mut self, record: Record) -> Result<(), Damaged> {
        match (record, &self.state) {
            (Record::File { path, bytes }, State::Listing) if self.files.len() < MAX_FILES => {
                self.files.push(Entry { path, bytes })
            }
            (Record::Unread { path, error }, State::Listing) if self.unread.len() < MAX_FILES => {
                self.unread.push(Unread { path, error })
            }
            (Record::Archive { .. }, State::Listing) => self.state = State::Archive,
            (Record::Withheld { reason }, State::Listing) => self.state = State::Withheld(reason),
            (Record::Failed { error }, State::Archive) => self.state = State::Failed(error),
            (Record::End { sha256, bytes }, State::Archive) => {
                let matches = bytes == self.archive.len() as u64
                    && sha256 == hex::encode(Sha256::digest(&self.archive));
                if !matches {
                    return Err(Damaged::Digest);
                }
                self.state = State::Complete;
            }
            _ => {}
        }
        Ok(())
    }

    /// Every archived file's contents, once the log has been read to its end.
    pub fn unpack(&self) -> Result<Vec<(String, Vec<u8>)>, Damaged> {
        if self.state != State::Complete {
            return Err(Damaged::Truncated);
        }
        let unpack = |e: io::Error| Damaged::Unpack(e.to_string());
        let mut tar =
            tar::Archive::new(GzDecoder::new(self.archive.as_slice()).take(self.tar_cap()));
        let mut out = Vec::new();
        for entry in tar.entries().map_err(unpack)? {
            let mut entry = entry.map_err(unpack)?;
            let path = entry
                .path()
                .map_err(unpack)?
                .to_str()
                .unwrap_or_default()
                .to_string();
            if !entry.header().entry_type().is_file() || !confined(&path) {
                return Err(Damaged::Path(path));
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(unpack)?;
            out.push((path, data));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::results::*;
    use std::os::unix::fs::PermissionsExt;

    fn collected(dir: &Path, limit: u64) -> String {
        let mut out = Vec::new();
        write(dir, limit, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn read(log: &str, limit: u64) -> Result<Reader, Damaged> {
        let mut reader = Reader::new(limit);
        for line in log.lines() {
            reader.line(line)?;
        }
        Ok(reader)
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn a_directory_round_trips_through_the_log_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let binary = noise(200_000);
        fs::create_dir_all(dir.path().join("runs/a b")).unwrap();
        fs::write(dir.path().join("result.json"), br#"{"p50": 1.5}"#).unwrap();
        fs::write(dir.path().join("runs/a b/trace.bin"), &binary).unwrap();
        fs::write(dir.path().join("runs/empty"), b"").unwrap();
        let long = format!("{}/deep.txt", "d".repeat(120));
        fs::create_dir_all(dir.path().join("d".repeat(120))).unwrap();
        fs::write(dir.path().join(&long), b"deep").unwrap();

        let log = collected(dir.path(), 1 << 20);
        assert!(log.lines().all(|l| l.len() <= LINE_BYTES / 3 * 4));
        let reader = read(&log, 1 << 20).unwrap();
        assert_eq!(reader.state, State::Complete);
        assert_eq!(
            reader.files,
            [
                (long.as_str(), 4),
                ("result.json", 12),
                ("runs/a b/trace.bin", 200_000),
                ("runs/empty", 0),
            ]
            .map(|(path, bytes)| Entry {
                path: path.to_string(),
                bytes
            })
        );
        assert!(reader.unread.is_empty());
        let files = reader.unpack().unwrap();
        assert_eq!(
            files,
            [
                (long, b"deep".to_vec()),
                ("result.json".to_string(), br#"{"p50": 1.5}"#.to_vec()),
                ("runs/a b/trace.bin".to_string(), binary),
                ("runs/empty".to_string(), Vec::new()),
            ]
        );
    }

    #[test]
    fn files_over_the_limit_are_listed_and_not_archived() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), noise(5000)).unwrap();
        fs::write(dir.path().join("small"), b"x").unwrap();
        let log = collected(dir.path(), 5000);
        let reader = read(&log, 5000).unwrap();
        assert_eq!(
            reader.state,
            State::Withheld(
                "its files total 5001 bytes, over the 5000 this server collects".into()
            )
        );
        assert_eq!(reader.files.len(), 2);
        assert_eq!(reader.unpack(), Err(Damaged::Truncated));
        assert_eq!(log.lines().count(), 3);

        let fits = read(&collected(dir.path(), 5001), 5001).unwrap();
        assert_eq!(fits.state, State::Complete);
    }

    #[test]
    fn what_is_not_a_readable_regular_file_is_named_and_left_out() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("kept"), b"k").unwrap();
        fs::write(dir.path().join("secret"), b"s").unwrap();
        fs::set_permissions(dir.path().join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("link")).unwrap();
        fs::create_dir(dir.path().join("closed")).unwrap();
        fs::set_permissions(dir.path().join("closed"), fs::Permissions::from_mode(0o000)).unwrap();

        let log = collected(dir.path(), 1 << 20);
        fs::set_permissions(dir.path().join("closed"), fs::Permissions::from_mode(0o755)).unwrap();
        let reader = read(&log, 1 << 20).unwrap();
        assert_eq!(
            reader.files,
            [Entry {
                path: "kept".into(),
                bytes: 1
            }]
        );
        let unread: Vec<(&str, &str)> = reader
            .unread
            .iter()
            .map(|u| (u.path.as_str(), u.error.as_str()))
            .collect();
        assert_eq!(unread.len(), 3, "{unread:?}");
        assert_eq!(unread[0].0, "closed");
        assert_eq!(unread[1], ("link", "not a regular file"));
        assert_eq!(unread[2].0, "secret");
        assert!(unread[2].1.contains("ermission denied"), "{unread:?}");
        assert_eq!(
            reader.unpack().unwrap(),
            [("kept".to_string(), b"k".to_vec())]
        );
    }

    #[test]
    fn a_missing_directory_is_one_unread_entry_and_an_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let reader = read(&collected(&dir.path().join("absent"), 10), 10).unwrap();
        assert_eq!(reader.state, State::Complete);
        assert_eq!(reader.unread.len(), 1);
        assert_eq!(reader.unread[0].path, ".");
        assert!(reader.unpack().unwrap().is_empty());
    }

    #[test]
    fn more_files_than_the_listing_holds_are_withheld() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=MAX_FILES {
            fs::write(dir.path().join(format!("f{i:05}")), b"").unwrap();
        }
        let reader = read(&collected(dir.path(), 1 << 20), 1 << 20).unwrap();
        assert_eq!(
            reader.state,
            State::Withheld("the directory holds more than 4096 files".into())
        );
        assert_eq!(reader.files.len(), MAX_FILES);
    }

    #[test]
    fn a_log_that_was_cut_or_altered_is_damaged() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), noise(10_000)).unwrap();
        let log = collected(dir.path(), 1 << 20);
        let lines: Vec<&str> = log.lines().collect();
        assert!(lines.len() >= 6, "{}", lines.len());

        let cut = read(&lines[..lines.len() - 1].join("\n"), 1 << 20).unwrap();
        assert_eq!(cut.state, State::Archive);
        assert!(!cut.ended() && cut.listed());
        assert_eq!(cut.unpack(), Err(Damaged::Truncated));

        let mut dropped = lines.clone();
        dropped.remove(3);
        assert_eq!(
            read(&dropped.join("\n"), 1 << 20).unwrap_err(),
            Damaged::Digest
        );

        let mut interleaved = lines.clone();
        interleaved.insert(3, "thread 'main' panicked");
        assert_eq!(
            read(&interleaved.join("\n"), 1 << 20).unwrap_err(),
            Damaged::Encoding
        );

        let payload = STANDARD.encode(vec![0u8; LINE_BYTES]);
        let mut flood = vec![lines[0], lines[1]];
        flood.extend(std::iter::repeat_n(payload.as_str(), 2000));
        assert_eq!(
            read(&flood.join("\n"), 16).unwrap_err(),
            Damaged::Oversize { limit: 16 }
        );
    }

    fn forged(entries: &[(&str, tar::EntryType)]) -> String {
        let mut out = Vec::new();
        record(&mut out, &Record::Archive { files: 1, bytes: 1 }).unwrap();
        let mut encoded = Encoded::new(&mut out);
        let mut tar = tar::Builder::new(GzEncoder::new(&mut encoded, Compression::default()));
        for (path, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(1);
            header.set_mode(0o644);
            let name = header.as_old_mut().name.as_mut_slice();
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            tar.append(&header, &b"x"[..]).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let (sha256, bytes) = encoded.finish().unwrap();
        record(&mut out, &Record::End { sha256, bytes }).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn an_archive_naming_a_path_outside_the_directory_is_refused() {
        for (path, kind) in [
            ("../escape", tar::EntryType::Regular),
            ("/etc/passwd", tar::EntryType::Regular),
            ("a/../../b", tar::EntryType::Regular),
            ("link", tar::EntryType::Symlink),
        ] {
            let reader = read(&forged(&[(path, kind)]), 1 << 20).unwrap();
            assert_eq!(reader.state, State::Complete, "{path}");
            assert_eq!(
                reader.unpack(),
                Err(Damaged::Path(path.to_string())),
                "{path}"
            );
        }
        let fine = read(&forged(&[("a/b", tar::EntryType::Regular)]), 1 << 20).unwrap();
        assert_eq!(fine.unpack().unwrap(), [("a/b".to_string(), b"x".to_vec())]);
    }

    #[test]
    fn only_paths_below_the_directory_are_confined() {
        for good in ["a", "a/b.json", "a b/c"] {
            assert!(confined(good), "{good}");
        }
        for bad in ["", "/a", "..", "a/../b", "./a", "a/.."] {
            assert!(!confined(bad), "{bad}");
        }
    }

    #[test]
    fn records_are_one_tagged_json_object_per_line() {
        let mut out = Vec::new();
        record(
            &mut out,
            &Record::File {
                path: "a\nb".into(),
                bytes: 3,
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"results\":\"file\",\"path\":\"a\\nb\",\"bytes\":3}\n"
        );
    }
}
