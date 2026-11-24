//! A `[[workspace.inject]]` copy: one file the pack places in the workspace, at setup and again
//! before each scored measure.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("inject destination {} is a directory", .path.display())]
pub struct DestIsDir {
    path: PathBuf,
}

/// Copy one inject `src` -> `dst`, creating parent dirs. Used at setup (all injects) and before each
/// scored measure (frozen injects), so the frozen judge the gate runs is always the baked one.
pub fn apply(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating inject dir {}", parent.display()))?;
    }
    // Replace the entry: copying through an agent-created symlink could escape `dst`.
    let dst_meta = std::fs::symlink_metadata(dst);
    let same_file = matches!(
        (std::fs::canonicalize(src), std::fs::canonicalize(dst)),
        (Ok(src), Ok(dst)) if src == dst
    );
    if dst_meta
        .as_ref()
        .is_ok_and(|meta| !meta.file_type().is_symlink())
        && same_file
    {
        return Ok(());
    }
    match dst_meta {
        Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => {
            return Err(DestIsDir {
                path: dst.to_path_buf(),
            }
            .into());
        }
        Ok(_) => std::fs::remove_file(dst)
            .with_context(|| format!("removing old inject destination {}", dst.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("inspecting inject destination {}", dst.display()));
        }
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("inject {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::inject::apply;

    #[test]
    fn copies_and_creates_parents() {
        let tmp = std::env::temp_dir().join(format!("inject-test-{}", std::process::id()));
        let src = tmp.join("src.txt");
        let dst = tmp.join("nested/deeper/dst.txt");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&src, b"frozen-judge").unwrap();
        apply(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"frozen-judge");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn replaces_a_destination_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("inject-symlink-test-{}", std::process::id()));
        let src = tmp.join("src.txt");
        let outside = tmp.join("outside.txt");
        let dst = tmp.join("dst.txt");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&src, b"frozen-judge").unwrap();
        std::fs::write(&outside, b"do-not-touch").unwrap();
        symlink(&outside, &dst).unwrap();

        apply(&src, &dst).unwrap();

        assert!(
            !std::fs::symlink_metadata(&dst)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"frozen-judge");
        assert_eq!(std::fs::read(&outside).unwrap(), b"do-not-touch");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
