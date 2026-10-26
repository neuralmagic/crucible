#![allow(clippy::disallowed_macros)]

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use flate2::Compression;
use flate2::write::GzEncoder;
use ignore::WalkBuilder;

// gzipped for streaming over exec stdin (the interactive path)
pub fn tarball(ctx: &Path) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&tar_bytes(ctx)?).context("gzipping tar")?;
    enc.finish().context("finalizing gzip")
}

// honors .dockerignore; .git/ and target/ always skipped because nobody
// wants to ship a mac-arch target dir to an amd64 builder. uncompressed so
// detach mode can use one sha256 as both blob digest and diff_id.
pub fn tar_bytes(ctx: &Path) -> Result<Vec<u8>> {
    if !ctx.is_dir() {
        return Err(anyhow!("context {} is not a directory", ctx.display()));
    }
    let mut walk = WalkBuilder::new(ctx);
    walk.standard_filters(false)
        .hidden(false)
        .add_custom_ignore_filename(".dockerignore");
    walk.filter_entry(|entry| {
        entry.depth() != 1 || (entry.file_name() != ".git" && entry.file_name() != "target")
    });

    let mut tar = tar::Builder::new(Vec::new());
    tar.follow_symlinks(false);

    for entry in walk.build() {
        let entry = entry.context("walking build context")?;
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(ctx)
            .with_context(|| format!("relativizing {}", path.display()))?;
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir {
            tar.append_dir(rel, path)
                .with_context(|| format!("archiving dir {}", rel.display()))?;
        } else {
            tar.append_path_with_name(path, rel)
                .with_context(|| format!("archiving {}", rel.display()))?;
        }
    }

    tar.into_inner().context("finalizing tar")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use flate2::read::GzDecoder;

    use crate::context::tarball;

    fn entries(data: &[u8]) -> BTreeSet<String> {
        let mut archive = tar::Archive::new(GzDecoder::new(data));
        archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn skips_git_target_and_dockerignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Dockerfile"));
        touch(&root.join("src/main.rs"));
        touch(&root.join(".git/HEAD"));
        touch(&root.join("target/debug/foo"));
        touch(&root.join("secrets.env"));
        // nested dirs named target are NOT the cargo dir, keep them
        touch(&root.join("src/target/keep.rs"));
        fs::write(root.join(".dockerignore"), "secrets.env\n").unwrap();

        let names = entries(&tarball(root).unwrap());
        assert!(names.contains("Dockerfile"), "{names:?}");
        assert!(names.contains("src/main.rs"), "{names:?}");
        assert!(names.contains("src/target/keep.rs"), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with(".git")), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("target")), "{names:?}");
        assert!(!names.contains("secrets.env"), "{names:?}");
    }

    #[test]
    fn rejects_non_directory_context() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        touch(&file);
        assert!(tarball(&file).is_err());
    }
}
