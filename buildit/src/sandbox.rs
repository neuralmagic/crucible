#![allow(clippy::disallowed_macros)]

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

pub const EXCLUDED: [&str; 6] = [".git", ".mcp.json", ".claude", ".buildit", ".kube", ".jira"];

pub const RESULTS_DIR: &str = ".buildit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxName(String);

/// A Kubernetes label value: at most 63 characters of alphanumerics, `-`, `_`, `.`, alphanumeric
/// at both ends.
pub fn is_label_value(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    raw.len() <= 63
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl SandboxName {
    pub fn parse(raw: &str) -> Result<Self> {
        if !is_label_value(raw) {
            bail!("invalid sandbox name {raw:?}");
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelPath(String);

impl RelPath {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.chars().any(char::is_control) {
            bail!("path {raw:?} must not contain control characters");
        }
        let mut parts = Vec::new();
        for comp in Path::new(raw).components() {
            match comp {
                Component::Normal(part) => {
                    let part = part
                        .to_str()
                        .ok_or_else(|| anyhow!("path {raw:?} is not utf-8"))?;
                    if EXCLUDED.contains(&part) {
                        bail!("path {raw:?} goes through {part}, which is never shipped");
                    }
                    parts.push(part);
                }
                Component::CurDir => {}
                Component::ParentDir => bail!("path {raw:?} must not contain `..`"),
                Component::RootDir | Component::Prefix(_) => bail!("path {raw:?} must be relative"),
            }
        }
        if parts.is_empty() {
            bail!("path {raw:?} must name a subdirectory, not the root");
        }
        Ok(Self(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDir(String);

impl SandboxDir {
    pub fn parse(raw: &str) -> Result<Self> {
        let rest = raw.trim_end_matches('/');
        let ok = rest.starts_with('/')
            && rest[1..].split('/').all(|p| !matches!(p, "" | "." | ".."))
            && !raw.chars().any(|c| c.is_whitespace() || c.is_control());
        if !ok {
            bail!("invalid sandbox workdir {raw:?}: must be an absolute, normal path");
        }
        Ok(Self(rest.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspace {
    Openshell { workdir: SandboxDir },
    // a local directory standing in for the sandbox, for tests and dev
    Local { dir: PathBuf },
}

impl Workspace {
    pub fn fetch_context(&self, name: &SandboxName, ctx: &RelPath, dest: &Path) -> Result<()> {
        fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
        match self {
            Workspace::Openshell { workdir } => openshell(&[
                "sandbox",
                "download",
                name.as_str(),
                &format!("{}/{}", workdir.0, ctx.as_str()),
                &dest.to_string_lossy(),
            ])?,
            Workspace::Local { dir } => {
                let mut at = dir.clone();
                for part in ctx.as_str().split('/') {
                    at.push(part);
                    if fs::symlink_metadata(&at)
                        .with_context(|| format!("reading {}", ctx.as_str()))?
                        .is_symlink()
                    {
                        bail!("the context {} goes through a symlink", ctx.as_str());
                    }
                }
                copy_tree(&at, dest)?;
            }
        }
        sanitize(dest)
    }

    // `results` is a directory named for the build id; it lands at .buildit/<id>
    pub fn publish(&self, name: &SandboxName, results: &Path) -> Result<()> {
        match self {
            Workspace::Openshell { workdir } => openshell(&[
                "sandbox",
                "upload",
                "--no-git-ignore",
                name.as_str(),
                &results.to_string_lossy(),
                &format!("{}/{RESULTS_DIR}/", workdir.0),
            ]),
            Workspace::Local { dir } => {
                let id = results
                    .file_name()
                    .ok_or_else(|| anyhow!("results dir {} has no name", results.display()))?;
                let dest = dir.join(RESULTS_DIR).join(id);
                for d in [dir.join(RESULTS_DIR), dest.clone()] {
                    match fs::symlink_metadata(&d) {
                        Ok(m) if m.is_dir() => {}
                        Ok(_) => bail!("{} is not a directory", d.display()),
                        Err(_) => fs::create_dir(&d)
                            .with_context(|| format!("creating {}", d.display()))?,
                    }
                }
                copy_tree(results, &dest)
            }
        }
    }
}

fn openshell(args: &[&str]) -> Result<()> {
    let out = Command::new("openshell")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running openshell (is it on PATH?)")?;
    if !out.status.success() {
        bail!(
            "openshell {} failed ({}): {}",
            args[..2].join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// copies files, dirs and links; never writes through a link already in `dest`
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry.with_context(|| format!("reading {}", src.display()))?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        let ft = entry.file_type()?;
        let existing = fs::symlink_metadata(&to).ok();
        if ft.is_dir() {
            match existing {
                Some(m) if m.is_dir() => {}
                Some(_) => bail!("{} is not a directory", to.display()),
                None => {
                    fs::create_dir(&to).with_context(|| format!("creating {}", to.display()))?
                }
            }
            copy_tree(&from, &to)?;
            continue;
        }
        if existing.is_some() {
            fs::remove_file(&to).with_context(|| format!("replacing {}", to.display()))?;
        }
        if ft.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)
                .with_context(|| format!("copying link {}", to.display()))?;
        } else if ft.is_file() {
            fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

// drops excluded names, special files, and links that resolve outside `root`
pub fn sanitize(root: &Path) -> Result<()> {
    let real_root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    sanitize_in(&real_root, root)
}

fn sanitize_in(real_root: &Path, dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let ft = entry.file_type()?;
        let excluded = entry
            .file_name()
            .to_str()
            .is_some_and(|n| EXCLUDED.contains(&n));
        if ft.is_dir() && !excluded {
            sanitize_in(real_root, &path)?;
        } else if ft.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        } else if excluded
            || (ft.is_symlink()
                && !path
                    .canonicalize()
                    .is_ok_and(|real| real.starts_with(real_root)))
            || !(ft.is_file() || ft.is_symlink())
        {
            tracing::info!("dropping {} from the build context", path.display());
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use crate::sandbox::{RelPath, SandboxDir, SandboxName, Workspace};

    fn touch(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn names_and_paths_stay_inside_their_bounds() {
        assert_eq!(RelPath::parse("./svc/api/").unwrap().as_str(), "svc/api");
        for bad in [
            "",
            ".",
            "/",
            "/abs",
            "..",
            "svc/../..",
            ".git",
            "a/.claude/x",
            "out/a\nb",
            "out\t",
        ] {
            assert!(RelPath::parse(bad).is_err(), "{bad:?}");
        }
        assert!(SandboxName::parse("ci-1_a.b").is_ok());
        for bad in ["", "-x", "--gateway=x", "a b", "a/b", &"x".repeat(64)] {
            assert!(SandboxName::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(SandboxDir::parse("/sandbox/w/").unwrap().0, "/sandbox/w");
        for bad in ["rel", "/", "/a/../b", "/a//b", "/a b"] {
            assert!(SandboxDir::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn local_fetch_keeps_inner_links_and_drops_exclusions_and_outer_links() {
        let work = tempfile::tempdir().unwrap();
        let w = work.path();
        touch(&w.join("svc/Dockerfile"), "FROM x");
        touch(&w.join("svc/.git/config"), "x");
        touch(&w.join("svc/sub/.mcp.json"), "x");
        touch(&w.join("secret"), "x");
        symlink("Dockerfile", w.join("svc/inner")).unwrap();
        symlink("../secret", w.join("svc/up")).unwrap();
        symlink(w.join("secret"), w.join("svc/abs")).unwrap();
        symlink("up", w.join("svc/via-up")).unwrap();
        symlink("svc", w.join("linked")).unwrap();
        let ws = Workspace::Local {
            dir: w.to_path_buf(),
        };
        let name = SandboxName::parse("sb").unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("ctx");
        ws.fetch_context(&name, &RelPath::parse("svc").unwrap(), &dest)
            .unwrap();
        let mut names: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["Dockerfile", "inner", "sub"]);
        assert_eq!(
            fs::read_link(dest.join("inner")).unwrap(),
            Path::new("Dockerfile")
        );
        assert_eq!(fs::read_dir(dest.join("sub")).unwrap().count(), 0);

        let err = ws
            .fetch_context(&name, &RelPath::parse("linked").unwrap(), &dest)
            .unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn local_publish_never_writes_through_links() {
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim");
        fs::write(&victim, "original").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let results = stage.path().join("buildit-1");
        touch(&results.join("out/a.txt"), "new");
        let name = SandboxName::parse("sb").unwrap();

        for at in [".buildit", ".buildit/buildit-1", ".buildit/buildit-1/out"] {
            let work = tempfile::tempdir().unwrap();
            fs::create_dir_all(work.path().join(at).parent().unwrap()).unwrap();
            symlink(elsewhere.path(), work.path().join(at)).unwrap();
            let ws = Workspace::Local {
                dir: work.path().to_path_buf(),
            };
            assert!(ws.publish(&name, &results).is_err(), "{at}");
            assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 1, "{at}");
        }

        let work = tempfile::tempdir().unwrap();
        let planted = work.path().join(".buildit/buildit-1/out/a.txt");
        fs::create_dir_all(planted.parent().unwrap()).unwrap();
        symlink(&victim, &planted).unwrap();
        let ws = Workspace::Local {
            dir: work.path().to_path_buf(),
        };
        ws.publish(&name, &results).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");
        assert_eq!(fs::read_to_string(&planted).unwrap(), "new");
    }
}
