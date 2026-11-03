//! `crucible migrate`: move a pack to a newer edition. Prints the diff, the changes, and what
//! needs review; writes the pack only when asked.

use crate::manifest::edition::Edition;
use crate::manifest::migrate::{PackFiles, migrate};
use anyhow::{Context, Result};
use std::path::Path;

pub(crate) fn run(manifest: &Path, to: Option<i64>, write: bool) -> Result<()> {
    let to = match to {
        Some(edition) => Edition::try_from(edition)?,
        None => Edition::LATEST,
    };
    let pack = PackFiles::read(manifest)?;
    let migration = migrate(&pack, to)?;
    if migration.is_noop() {
        println!(
            "[crucible migrate] {} is edition {}: nothing to do",
            manifest.display(),
            migration.from
        );
        return Ok(());
    }
    let diffs = migration.diffs(&pack);
    for diff in &diffs {
        print!("{}", diff.unified);
    }
    for change in &migration.changes {
        println!("[crucible migrate] change: {change}");
    }
    for finding in &migration.needs_review {
        println!("[crucible migrate] review: {finding}");
    }
    if !write {
        println!("[crucible migrate] dry run: pass --write to apply");
        return Ok(());
    }
    let root = crate::manifest::manifest_dir(manifest);
    for diff in &diffs {
        let text = migration
            .files
            .files()
            .get(&diff.path)
            .with_context(|| format!("the migration dropped {}", diff.path))?;
        let target = root.join(&diff.path);
        std::fs::write(&target, text)
            .with_context(|| format!("writing the migrated {}", target.display()))?;
        println!("[crucible migrate] wrote {}", target.display());
    }
    Ok(())
}
