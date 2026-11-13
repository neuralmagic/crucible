//! Preparing a manifest's workspace: the setup command or clone, and the toolbox copied into it.

use crate::args::Paths;
use crate::errors::FileError;
use crate::manifest::{self, GitRef};
use crucible_vcs::cli::{GitError, git};
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
    #[error("git clone {src} -> {} failed", .dest.display())]
    GitCloneFailed {
        src: String,
        dest: PathBuf,
        #[source]
        source: GitError,
    },
    #[error("fetching {git_ref} from {src} failed: it names no branch, tag or commit there")]
    GitFetchFailed {
        src: String,
        git_ref: GitRef,
        #[source]
        source: GitError,
    },
    #[error("checking out {git_ref} in {} failed", .dest.display())]
    GitCheckoutFailed {
        git_ref: GitRef,
        dest: PathBuf,
        #[source]
        source: GitError,
    },
    #[error("in {}", .dir.display())]
    Git {
        dir: PathBuf,
        #[source]
        source: GitError,
    },
    #[error("reading the packs of {}", .dir.display())]
    ReadPacks {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
    clone_repo(src, m.repo.git_ref.as_ref(), m.repo.filter, workspace)
}

/// `git clone <src> <dest>`, then `git_ref` fetched and checked out detached. A clone whose
/// ref cannot be checked out is removed, so no later run mistakes it for one at that ref.
pub(crate) fn clone_repo(
    src: &str,
    git_ref: Option<&GitRef>,
    filter: Option<crate::manifest::CloneFilter>,
    dest: &Path,
) -> Result<(), WorkspaceError> {
    let filter_arg = filter.map(|f| format!("--filter={}", f.as_str()));
    let mut args: Vec<&std::ffi::OsStr> = vec!["clone".as_ref(), "--quiet".as_ref()];
    args.extend(filter_arg.as_deref().map(std::ffi::OsStr::new));
    args.extend([src.as_ref(), dest.as_os_str()]);
    git(None, args)
        .run()
        .map_err(|source| WorkspaceError::GitCloneFailed {
            src: src.to_owned(),
            dest: dest.to_path_buf(),
            source,
        })?;
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
    let held = r.is_commit()
        && git(dest, ["cat-file", "-e", &format!("{r}^{{commit}}")])
            .succeeds()
            .map_err(in_dir(dest))?;
    let target = if held {
        r.as_str()
    } else {
        git(dest, ["fetch", "--quiet", "origin", r.as_str()])
            .run()
            .map_err(|source| WorkspaceError::GitFetchFailed {
                src: src.to_owned(),
                git_ref: r.clone(),
                source,
            })?;
        "FETCH_HEAD"
    };
    git(dest, ["checkout", "--quiet", "--detach", target])
        .run()
        .map_err(|source| WorkspaceError::GitCheckoutFailed {
            git_ref: r.clone(),
            dest: dest.to_path_buf(),
            source,
        })
}

/// A `map_err` argument naming the repository a git failed in.
fn in_dir(dir: &Path) -> impl Fn(GitError) -> WorkspaceError {
    let dir = dir.to_path_buf();
    move |source| WorkspaceError::Git {
        dir: dir.clone(),
        source,
    }
}

/// The remote a partial clone fetches missing objects from; `None` for a full clone.
fn promisor_remote(workspace: &Path) -> Result<Option<String>, WorkspaceError> {
    let remotes = git(workspace, ["remote"])
        .read()
        .map_err(in_dir(workspace))?;
    for remote in remotes.lines() {
        let key = format!("remote.{remote}.promisor");
        let promisor = git(
            workspace,
            [
                "config",
                "--get",
                "--type",
                "bool",
                "--default",
                "false",
                &key,
            ],
        )
        .read()
        .map_err(in_dir(workspace))?;
        if promisor.trim() == "true" {
            return Ok(Some(remote.to_owned()));
        }
    }
    Ok(None)
}

/// The blobs a partial clone has fetched from origin, by id, sorted: those its checkout needed
/// and those anything since read. None for a full clone, which holds every blob from the start.
pub(crate) fn fetched_blobs(workspace: &Path) -> Result<Vec<String>, WorkspaceError> {
    if promisor_remote(workspace)?.is_none() {
        return Ok(Vec::new());
    }
    let packs = git(
        workspace,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects/pack",
        ],
    )
    .read()
    .map_err(in_dir(workspace))?;
    let packs = PathBuf::from(packs.trim_end_matches('\n'));
    let read = |source| WorkspaceError::ReadPacks {
        dir: packs.clone(),
        source,
    };
    let mut fetched = Vec::new();
    for entry in std::fs::read_dir(&packs).map_err(read)? {
        let marker = entry.map_err(read)?.path();
        if marker.extension().is_none_or(|ext| ext != "promisor") {
            continue;
        }
        let index = std::fs::File::open(marker.with_extension("idx")).map_err(read)?;
        let listed = git(workspace, ["show-index"])
            .stdin_file(index)
            .read()
            .map_err(in_dir(workspace))?;
        fetched.extend(
            listed
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_owned),
        );
    }
    let typed = git(
        workspace,
        ["cat-file", "--batch-check=%(objecttype) %(objectname)"],
    )
    .stdin_bytes(fetched.join("\n"))
    .read()
    .map_err(in_dir(workspace))?;
    let mut blobs: Vec<String> = typed
        .lines()
        .filter_map(|line| line.strip_prefix("blob "))
        .map(str::to_owned)
        .collect();
    blobs.sort();
    blobs.dedup();
    Ok(blobs)
}

/// Fetch those of `blobs` a partial clone does not hold from the remote it was cloned from. A
/// full clone holds every blob its remote serves, so there is nothing to fetch.
pub(crate) fn fetch_blobs(workspace: &Path, blobs: &[String]) -> Result<(), WorkspaceError> {
    if blobs.is_empty() {
        return Ok(());
    }
    let Some(remote) = promisor_remote(workspace)? else {
        return Ok(());
    };
    let held = git(
        workspace,
        [
            "cat-file",
            "--batch-all-objects",
            "--unordered",
            "--batch-check=%(objectname)",
        ],
    )
    .read()
    .map_err(in_dir(workspace))?;
    let held: std::collections::HashSet<&str> = held.lines().collect();
    let missing: Vec<&str> = blobs
        .iter()
        .map(String::as_str)
        .filter(|id| !held.contains(id))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    git(
        workspace,
        [
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
            "--stdin",
            &remote,
        ],
    )
    .stdin_bytes(missing.join("\n"))
    .read()
    .map_err(in_dir(workspace))?;
    Ok(())
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
/// harness's discovery path, see [`crate::knobs::Harness::skills_dir`]).
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
            clone_repo(&src, r.map(at).as_ref(), None, &dest).map(|()| dest)
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
    fn a_filtered_clone_is_partial_and_keeps_every_commit() {
        let origin = crate::testing::git_origin("clone-filter");
        let git = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git runs")
        };
        assert!(
            git(&origin.path, &["config", "uploadpack.allowFilter", "true"])
                .status
                .success()
        );
        let src = format!("file://{}", origin.path.display());
        let dest = origin.path.with_extension("partial");
        clone_repo(
            &src,
            Some(&at("feature/x")),
            Some(crate::manifest::CloneFilter::BlobNone),
            &dest,
        )
        .expect("clones");
        assert_eq!(
            crate::testing::checked_out(&dest),
            (origin.feature.clone(), true)
        );
        let filter = git(&dest, &["config", "remote.origin.partialclonefilter"]);
        assert_eq!(String::from_utf8_lossy(&filter.stdout).trim(), "blob:none");
        assert!(
            git(
                &dest,
                &["merge-base", "--is-ancestor", &origin.main, "HEAD"]
            )
            .status
            .success(),
            "the partial clone holds the history merge-base walks"
        );
    }

    #[test]
    fn a_partial_clone_lists_the_blobs_it_fetched_and_another_fetches_them() {
        let origin = crate::testing::git_origin("clone-fetched");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git runs");
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            )
        };
        for key in ["uploadpack.allowFilter", "uploadpack.allowAnySHA1InWant"] {
            assert!(git(&origin.path, &["config", key, "true"]).0);
        }
        let src = format!("file://{}", origin.path.display());
        let partial = |name: &str| {
            let dest = origin.path.with_extension(name);
            clone_repo(
                &src,
                None,
                Some(crate::manifest::CloneFilter::BlobNone),
                &dest,
            )
            .expect("clones");
            dest
        };
        let on_main = git(&origin.path, &["rev-parse", "main:a.txt"]).1;
        let on_feature = git(&origin.path, &["rev-parse", "feature/x:a.txt"]).1;
        let feature_blob = format!("{}:a.txt", origin.feature);
        let offline = |dir: &std::path::Path| {
            git(
                dir,
                &[
                    "-c",
                    "url./nonexistent/.insteadOf=file://",
                    "cat-file",
                    "-p",
                    &feature_blob,
                ],
            )
        };

        let first = partial("first");
        assert_eq!(
            fetched_blobs(&first).expect("lists"),
            std::slice::from_ref(&on_main)
        );
        assert_eq!(git(&first, &["cat-file", "-p", &feature_blob]).1, "feature");
        let mut both = vec![on_main.clone(), on_feature];
        both.sort();
        assert_eq!(fetched_blobs(&first).expect("lists"), both);

        let second = partial("second");
        assert!(!offline(&second).0, "a fresh clone lacks the blob");
        fetch_blobs(&second, &both).expect("fetches the one it lacks");
        assert_eq!(offline(&second), (true, "feature".to_owned()));
        assert_eq!(fetched_blobs(&second).expect("lists"), both);
        fetch_blobs(&second, &both).expect("nothing left to fetch");

        let unknown = vec!["0".repeat(40)];
        assert!(
            matches!(
                fetch_blobs(&second, &unknown),
                Err(WorkspaceError::Git { .. })
            ),
            "a blob origin does not serve fails the fetch"
        );

        let full = origin.path.with_extension("full");
        clone_repo(&src, None, None, &full).expect("clones");
        assert_eq!(fetched_blobs(&full).expect("lists"), Vec::<String>::new());
        fetch_blobs(&full, &both).expect("a full clone holds them already");
        for dir in [first, second, full] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn listing_and_fetching_carry_more_blobs_than_a_pipe_buffers() {
        let origin = crate::testing::git_origin("clone-many");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.email=c@l", "-c", "user.name=c"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        const FILES: usize = 4000;
        git(&origin.path, &["checkout", "-q", "-b", "many"]);
        fs::create_dir(origin.path.join("many")).expect("mkdir");
        for i in 0..FILES {
            fs::write(origin.path.join(format!("many/{i}.txt")), format!("{i}\n")).expect("write");
        }
        git(&origin.path, &["add", "-A"]);
        git(&origin.path, &["commit", "-q", "-m", "many"]);
        git(&origin.path, &["checkout", "-q", "main"]);
        for key in ["uploadpack.allowFilter", "uploadpack.allowAnySHA1InWant"] {
            git(&origin.path, &["config", key, "true"]);
        }
        let src = format!("file://{}", origin.path.display());
        let filter = Some(crate::manifest::CloneFilter::BlobNone);

        let wide = origin.path.with_extension("wide");
        clone_repo(&src, Some(&at("many")), filter, &wide).expect("clones");
        let blobs = fetched_blobs(&wide).expect("lists");
        assert_eq!(
            blobs.len(),
            FILES + 1,
            "every file of the checkout, a.txt included"
        );
        assert!(
            blobs.iter().map(|id| id.len() + 1).sum::<usize>() > 2 * 65536,
            "the list outgrows a pipe's buffer"
        );

        let narrow = origin.path.with_extension("narrow");
        clone_repo(&src, None, filter, &narrow).expect("clones");
        assert_eq!(fetched_blobs(&narrow).expect("lists").len(), 1);
        fetch_blobs(&narrow, &blobs).expect("fetches");
        assert_eq!(fetched_blobs(&narrow).expect("lists"), blobs);
        git(
            &narrow,
            &[
                "-c",
                "url./nonexistent/.insteadOf=file://",
                "checkout",
                "-q",
                "origin/many",
            ],
        );
        for dir in [wide, narrow] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_commit_is_held_by_the_branches_and_tags_whose_history_has_it() {
        let origin = crate::testing::git_origin("clone-contains");
        let dest = origin.path.with_extension("clone");
        clone_repo(&origin.path.to_string_lossy(), None, None, &dest).expect("clones");
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
