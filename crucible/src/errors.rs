//! Error building blocks shared by the typed error enums.

use std::path::{Path, PathBuf};

/// Flatten an error and its `source()` chain onto one line, anyhow's `{:#}` shape.
///
/// Typed errors keep their causes as real sources rather than pre-formatting them into
/// Display, so a `{}` prints only the outermost layer. Anything that renders an error into
/// a log line, a report field, or a test assertion goes through here instead.
pub fn report(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut cause = error.source();
    while let Some(current) = cause {
        out.push_str(": ");
        out.push_str(&current.to_string());
        cause = current.source();
    }
    out
}

/// A filesystem failure with the operation and path that caused it. Typed enums embed it
/// as a `#[from]` variant so a module error can say "reading prompt file X" over an
/// `io::Error` source, without every enum growing its own IO arm.
#[derive(Debug, thiserror::Error)]
#[error("{doing} {}", .path.display())]
pub struct FileError {
    doing: &'static str,
    path: PathBuf,
    #[source]
    cause: std::io::Error,
}

impl FileError {
    /// A `map_err` argument: `fs::read(&p).map_err(FileError::at("reading prompt file", &p))?`.
    pub fn at(doing: &'static str, path: impl AsRef<Path>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.as_ref().to_path_buf();
        move |cause| FileError { doing, path, cause }
    }
}

// Equality ignores the io::Error, which has none: tests compare the operation and path.
impl PartialEq for FileError {
    fn eq(&self, other: &Self) -> bool {
        self.doing == other.doing
            && self.path == other.path
            && self.cause.kind() == other.cause.kind()
    }
}
