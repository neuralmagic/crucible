//! Preparing a manifest's workspace: the setup command or clone, and the toolbox copied into it.

use crate::args::Paths;
use crate::errors::FileError;
use crate::manifest::{self, GitRef};
use std::path::{Path, PathBuf};

/// What preparing a workspace can fail on: the setup command, the clone, the toolbox copy.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspaceError {
    #[error("running setup_cmd: {cmd}")]
    SpawnSetupCmd {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("setup_cmd failed ({status}): {cmd}")]
    SetupCmdFailed {
        cmd: String,
        status: std::process::ExitStatus,
    },
    #[error("creating workspace {}", .dir.display())]
    CreateWorkspace {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("running git")]
    SpawnGit(#[source] std::io::Error),
    #[error("git clone {src} -> {} failed", .dest.display())]
    GitCloneFailed { src: String, dest: PathBuf },
    #[error("fetching {git_ref} from {src} failed: it names no branch, tag or commit there")]
    GitFetchFailed { src: String, git_ref: GitRef },
    #[error("checking out {git_ref} in {} failed", .dest.display())]
    GitCheckoutFailed { git_ref: GitRef, dest: PathBuf },
    #[error("reading the refs of {}", .dest.display())]
    ReadRefs {
        dest: PathBuf,
        #[source]
        source: git2::Error,
    },
    #[error(
        "[agent].toolbox_exclude names `{name}`, but no such skill dir exists under {}",
        .skills.display()
    )]
    UnknownToolboxExclude { name: String, skills: PathBuf },
    #[error("failed to copy skill {}", .skill.display())]
    CopySkill {
        skill: PathBuf,
        #[source]
        source: fs_extra::error::Error,
    },
    #[error(transparent)]
    File(#[from] FileError),
}

/// Prepare the manifest's workspace: run `setup_cmd` (cwd = manifest dir, the one command that
/// runs there since it *creates* the workspace), else `git clone` `[repo]`, else an empty dir
/// for a repo-less playbook. Injects land after this, and `ensure_repo` commits the baseline.
pub(crate) fn manifest_setup(
    m: &manifest::Manifest,
    manifest_dir: &Path,
    workspace: &Path,
) -> Result<(), WorkspaceError> {
    if let Some(cmd) = &m.workspace.setup_cmd {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(manifest_dir)
            .status()
            .map_err(|source| WorkspaceError::SpawnSetupCmd {
                cmd: cmd.clone(),
                source,
            })?;
        if !status.success() {
            return Err(WorkspaceError::SetupCmdFailed {
                cmd: cmd.clone(),
                status,
            });
        }
        return Ok(());
    }
    let Some(src) = m.repo.source() else {
        return std::fs::create_dir_all(workspace).map_err(|source| {
            WorkspaceError::CreateWorkspace {
                dir: workspace.to_path_buf(),
                source,
            }
        });
    };
    clone_repo(src, m.repo.git_ref.as_ref(), workspace)
}

/// `git clone <src> <dest>`, then `git_ref` fetched and checked out detached. A clone whose
/// ref cannot be checked out is removed, so no later run mistakes it for one at that ref.
pub(crate) fn clone_repo(
    src: &str,
    git_ref: Option<&GitRef>,
    dest: &Path,
) -> Result<(), WorkspaceError> {
    let dest_arg = dest.to_string_lossy();
    if !git(None, &["clone", "--quiet", src, &dest_arg])? {
        return Err(WorkspaceError::GitCloneFailed {
            src: src.to_owned(),
            dest: dest.to_path_buf(),
        });
    }
    let Some(r) = git_ref else {
        return Ok(());
    };
    let checked_out = checkout(src, dest, r);
    if checked_out.is_err()
        && let Err(e) = std::fs::remove_dir_all(dest)
    {
        tracing::warn!(dest = %dest.display(), error = %e, "removing a clone whose ref failed");
    }
    checked_out
}

/// Fetch `r` from origin and check it out detached. A commit the clone already holds is checked
/// out without a fetch, since a server need not serve a commit by id.
fn checkout(src: &str, dest: &Path, r: &GitRef) -> Result<(), WorkspaceError> {
    let held = r.is_commit() && git(Some(dest), &["cat-file", "-e", &format!("{r}^{{commit}}")])?;
    let target = if held {
        r.as_str()
    } else {
        if !git(Some(dest), &["fetch", "--quiet", "origin", r.as_str()])? {
            return Err(WorkspaceError::GitFetchFailed {
                src: src.to_owned(),
                git_ref: r.clone(),
            });
        }
        "FETCH_HEAD"
    };
    if !git(Some(dest), &["checkout", "--quiet", "--detach", target])? {
        return Err(WorkspaceError::GitCheckoutFailed {
            git_ref: r.clone(),
            dest: dest.to_path_buf(),
        });
    }
    Ok(())
}

/// Run git, in `dir` when given; `Ok(false)` is a git that ran and failed.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<bool, WorkspaceError> {
    let mut command = std::process::Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    Ok(command
        .args(args)
        .status()
        .map_err(WorkspaceError::SpawnGit)?
        .success())
}

/// The branches and tags of a fresh clone whose history holds `commit`, by short name.
pub(crate) fn refs_containing(dest: &Path, commit: &GitRef) -> Result<Vec<String>, WorkspaceError> {
    let read = |source| WorkspaceError::ReadRefs {
        dest: dest.to_path_buf(),
        source,
    };
    let repo = git2::Repository::open(dest).map_err(read)?;
    let target = repo
        .revparse_single(commit.as_str())
        .and_then(|o| o.peel_to_commit())
        .map_err(read)?
        .id();
    let mut names = Vec::new();
    for reference in repo.references().map_err(read)? {
        let reference = reference.map_err(read)?;
        let Some(short) = reference.name().ok().and_then(|full| {
            full.strip_prefix("refs/remotes/origin/")
                .filter(|short| *short != "HEAD")
                .or_else(|| full.strip_prefix("refs/tags/"))
        }) else {
            continue;
        };
        let Ok(tip) = reference.peel_to_commit() else {
            continue;
        };
        if tip.id() == target || repo.graph_descendant_of(tip.id(), target).map_err(read)? {
            names.push(short.to_owned());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Copy every non-excluded skill under `p.skills` into the workspace's `skills_dir` (the
/// harness's discovery path, see [`crate::manifest::Harness::skills_dir`]).
/// `exclude` names setup-only skills (deployment config, workload capture) that the loop agent must
/// never see, see [`manifest::AgentCfg::toolbox_exclude`]. A name in `exclude` that doesn't
/// exist under the toolbox dir is a manifest bug (the exclusion is silently doing nothing), so
/// it's an error rather than a warning.
pub(crate) fn install_toolbox(
    p: &Paths,
    exclude: &[String],
    skills_dir: &str,
) -> Result<(), WorkspaceError> {
    let Some(src) = &p.skills else {
        return Ok(());
    };
    for name in exclude {
        if !src.join(name).is_dir() {
            return Err(WorkspaceError::UnknownToolboxExclude {
                name: name.clone(),
                skills: src.clone(),
            });
        }
    }
    let dst = p.workspace.join(skills_dir);
    std::fs::create_dir_all(&dst).map_err(FileError::at("creating the toolbox dir", &dst))?;
    let entries = std::fs::read_dir(src).map_err(FileError::at("reading the skills dir", src))?;
    for entry in entries {
        let skill = entry
            .map_err(FileError::at("reading the skills dir", src))?
            .path();
        if skill.is_dir() {
            let Some(name) = skill.file_name() else {
                continue;
            };
            if exclude.iter().any(|e| e.as_str() == name) {
                continue;
            }
            let _ = std::fs::remove_dir_all(dst.join(name));
            let opts = fs_extra::dir::CopyOptions::new().overwrite(true);
            fs_extra::dir::copy(&skill, &dst, &opts).map_err(|source| {
                WorkspaceError::CopySkill {
                    skill: skill.clone(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tempdir;
    use std::fs;

    fn at(raw: &str) -> GitRef {
        GitRef::parse(raw).expect("a ref")
    }

    #[test]
    fn a_ref_is_fetched_and_checked_out_detached_by_any_name() {
        let origin = crate::testing::git_origin("clone-refs");
        let src = origin.path.to_string_lossy().into_owned();
        let clone = |name: &str, r: Option<&str>| {
            let dest = origin.path.with_extension(name);
            let _ = fs::remove_dir_all(&dest);
            clone_repo(&src, r.map(at).as_ref(), &dest).map(|()| dest)
        };

        let plain = clone("plain", None).expect("clones");
        assert_eq!(
            crate::testing::checked_out(&plain),
            (origin.main.clone(), false)
        );
        for (name, r, expected) in [
            ("short", "feature/x", &origin.feature),
            ("full", "refs/heads/feature/x", &origin.feature),
            ("tag", "v1", &origin.main),
            ("commit", origin.feature.as_str(), &origin.feature),
            ("pull", "refs/pull/7/head", &origin.pull),
        ] {
            let dest = clone(name, Some(r)).unwrap_or_else(|e| panic!("{r}: {e:#}"));
            assert_eq!(
                crate::testing::checked_out(&dest),
                (expected.clone(), true),
                "{r}"
            );
        }

        let missing = origin.path.with_extension("missing");
        let err = clone("missing", Some("feature/nope")).expect_err("no such branch");
        assert!(
            matches!(err, WorkspaceError::GitFetchFailed { .. }),
            "{err:#}"
        );
        assert!(
            !missing.exists(),
            "a clone whose ref failed is removed, so no later run reuses it at the default branch"
        );
    }

    #[test]
    fn a_commit_is_held_by_the_branches_and_tags_whose_history_has_it() {
        let origin = crate::testing::git_origin("clone-contains");
        let dest = origin.path.with_extension("clone");
        clone_repo(&origin.path.to_string_lossy(), None, &dest).expect("clones");
        let held = |commit: &str| refs_containing(&dest, &at(commit)).expect("reads refs");
        assert_eq!(held(&origin.main), ["feature/x", "main", "v1"]);
        assert_eq!(held(&origin.feature), ["feature/x"]);
        assert!(
            held(&origin.dangling).is_empty(),
            "a commit the clone holds that no ref does"
        );
        assert!(
            held(&origin.pull).is_empty(),
            "a clone fetches no pull request refs, so a commit only one holds is held by nothing"
        );
    }

    /// A toolbox source dir with two skills; `install_toolbox` must copy only the one not named in
    /// `toolbox_exclude`, the setup-only skill never reaches the workspace `.claude/skills`.
    #[test]
    fn install_toolbox_skips_excluded_skills() {
        let dir = tempdir("toolbox-exclude");
        let skills = dir.join("skills");
        fs::create_dir_all(skills.join("rig-config")).unwrap();
        fs::write(skills.join("rig-config/SKILL.md"), "setup-only").unwrap();
        fs::create_dir_all(skills.join("bench")).unwrap();
        fs::write(skills.join("bench/SKILL.md"), "measure").unwrap();
        let workspace = dir.join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        let p = Paths::for_manifest(workspace.clone(), dir.join("state"), &dir, Some(skills));
        install_toolbox(&p, &["rig-config".to_string()], ".claude/skills")
            .expect("install_toolbox");

        assert!(
            !workspace.join(".claude/skills/rig-config").exists(),
            "excluded skill must not reach the workspace"
        );
        assert!(
            workspace.join(".claude/skills/bench/SKILL.md").exists(),
            "the non-excluded skill must still be copied"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A `toolbox_exclude` name that doesn't exist under the toolbox dir is a manifest bug (the
    /// exclusion is silently doing nothing), `install_toolbox` must refuse to proceed rather than
    /// quietly no-op.
    #[test]
    fn install_toolbox_errors_on_an_exclude_name_that_does_not_exist() {
        let dir = tempdir("toolbox-exclude-typo");
        let skills = dir.join("skills");
        fs::create_dir_all(skills.join("bench")).unwrap();
        let workspace = dir.join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        let p = Paths::for_manifest(workspace.clone(), dir.join("state"), &dir, Some(skills));
        let err =
            install_toolbox(&p, &["rig-cofnig-typo".to_string()], ".claude/skills").unwrap_err();
        assert!(err.to_string().contains("rig-cofnig-typo"));

        let _ = fs::remove_dir_all(&dir);
    }
}
