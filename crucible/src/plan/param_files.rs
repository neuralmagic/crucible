//! File parameter values on this engine's disk. A supplied value whose
//! staged path is not readable here is fetched from the controller that launched the run by its
//! digest, checked, and staged read-only under the run's state, beside the pack and never inside
//! it. Agent tasks then find every file parameter under [`input_path`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::plan::STAGED_INPUTS;
use crate::plan::call_run::Controller;
use crate::plan::starlark::params::ParamType;

/// Why a file parameter could not be put on this disk.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("file parameter {param:?}: {digest:?} is not a sha256 digest")]
    Digest { param: String, digest: String },
    #[error(
        "file parameter {param:?}: {} is not on this disk and no controller launched this run to \
         fetch it from",
        named.display()
    )]
    NoController { param: String, named: PathBuf },
    #[error("file parameter {param:?}: fetching {digest}: {detail}")]
    Fetch {
        param: String,
        digest: String,
        detail: String,
    },
    #[error("file parameter {param:?}: the controller sent {got}, not {digest}")]
    Mismatch {
        param: String,
        digest: String,
        got: String,
    },
    #[error("file parameter {param:?}: staging {}: {source}", path.display())]
    Stage {
        param: String,
        path: PathBuf,
        source: std::io::Error,
    },
}

pub const STAGE_DIR: &str = "param-files";

pub const INPUTS_DIR: &str = "params";

/// One file parameter, as this engine reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamFile {
    pub name: String,
    pub path: PathBuf,
}

/// Supplied values with every file parameter pointing at a file on this disk, and those files.
#[derive(Debug, Default)]
pub struct Localized {
    pub supplied: BTreeMap<String, String>,
    pub files: Vec<ParamFile>,
}

/// A staged file parameter value, as the controller supplies it.
#[derive(Deserialize)]
struct Staged {
    path: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    media_type: Option<String>,
}

impl Staged {
    fn read(raw: &str) -> Option<Self> {
        if raw.trim_start().starts_with('{') {
            crucible_contract::json::from_str(raw).ok()
        } else {
            Some(Staged {
                path: raw.to_owned(),
                digest: None,
                size: None,
                media_type: None,
            })
        }
    }
}

fn base_name(path: &Path) -> String {
    let name: String = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                true => c,
                false => '_',
            },
        )
        .collect();
    match name.trim_start_matches('.') {
        "" => "file".to_owned(),
        rest => rest.to_owned(),
    }
}

pub fn input_path(name: &str, path: &Path) -> String {
    format!("{STAGED_INPUTS}/{INPUTS_DIR}/{name}/{}", base_name(path))
}

fn digest_hex(digest: &str) -> Option<&str> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn localize(
    source: &Path,
    supplied: &BTreeMap<String, String>,
    state: &Path,
    controller: Option<&Controller>,
) -> Result<Localized, FetchError> {
    let mut localized = Localized {
        supplied: supplied.clone(),
        files: Vec::new(),
    };
    let Ok(text) = std::fs::read_to_string(source) else {
        return Ok(localized);
    };
    let Ok(declared) = crate::plan::starlark::read_params(&text, source) else {
        return Ok(localized);
    };
    for spec in declared.specs() {
        if spec.ty != ParamType::File {
            continue;
        }
        let Some(staged) = supplied.get(&spec.name).and_then(|raw| Staged::read(raw)) else {
            continue;
        };
        let here = Path::new(&staged.path);
        if here.is_absolute() && here.is_file() {
            localized.files.push(ParamFile {
                name: spec.name.clone(),
                path: here.to_path_buf(),
            });
            continue;
        }
        let Some(digest) = &staged.digest else {
            continue;
        };
        let path = fetch(&spec.name, state, here, digest, controller)?;
        let mut value = serde_json::json!({
            "path": path.to_string_lossy(),
            "digest": digest,
        });
        if let Some(size) = staged.size {
            value["size"] = serde_json::json!(size);
        }
        if let Some(media_type) = &staged.media_type {
            value["media_type"] = serde_json::json!(media_type);
        }
        localized
            .supplied
            .insert(spec.name.clone(), value.to_string());
        localized.files.push(ParamFile {
            name: spec.name.clone(),
            path,
        });
    }
    Ok(localized)
}

fn fetch(
    param: &str,
    state: &Path,
    named: &Path,
    digest: &str,
    controller: Option<&Controller>,
) -> Result<PathBuf, FetchError> {
    let hex = digest_hex(digest).ok_or_else(|| FetchError::Digest {
        param: param.to_owned(),
        digest: digest.to_owned(),
    })?;
    let dir = state.join(STAGE_DIR).join(hex);
    let staging = |path: &Path| {
        let path = path.to_path_buf();
        move |source| FetchError::Stage {
            param: param.to_owned(),
            path,
            source,
        }
    };
    let dir = std::path::absolute(&dir).map_err(staging(&dir))?;
    let path = dir.join(base_name(named));
    if let Ok(bytes) = std::fs::read(&path)
        && crucible_contract::artifact::content_digest(&bytes) == digest
    {
        return Ok(path);
    }
    let Some(controller) = controller else {
        return Err(FetchError::NoController {
            param: param.to_owned(),
            named: named.to_path_buf(),
        });
    };
    let bytes = controller
        .param_file(digest)
        .map_err(|detail| FetchError::Fetch {
            param: param.to_owned(),
            digest: digest.to_owned(),
            detail,
        })?;
    let got = crucible_contract::artifact::content_digest(&bytes);
    if got != digest {
        return Err(FetchError::Mismatch {
            param: param.to_owned(),
            digest: digest.to_owned(),
            got,
        });
    }
    std::fs::create_dir_all(&dir).map_err(staging(&dir))?;
    if path.exists() {
        writable(&path);
        std::fs::remove_file(&path).map_err(staging(&path))?;
    }
    std::fs::write(&path, &bytes).map_err(staging(&path))?;
    read_only(&path);
    Ok(path)
}

pub fn materialize(root: &Path, files: &[ParamFile]) -> Result<(), String> {
    for file in files {
        let to = root.join(input_path(&file.name, &file.path));
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("staging {}: {e}", to.display()))?;
        }
        if to.exists() {
            writable(&to);
            let _ = std::fs::remove_file(&to);
        }
        std::fs::copy(&file.path, &to).map_err(|e| format!("staging {}: {e}", to.display()))?;
        read_only(&to);
    }
    Ok(())
}

fn read_only(path: &Path) {
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

fn writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    #[cfg(not(unix))]
    {
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::param_files::{ParamFile, STAGE_DIR, input_path, localize, materialize};
    use std::collections::BTreeMap;
    use std::path::Path;

    const SOURCE: &str = r#"params = {
    "notes": {"type": "file", "required": True, "max_bytes": 1024},
    "config": {"type": "json", "required": True},
}
t = command(name = "t", run = "true")
workflow(type = "playbook", tasks = [t])
"#;

    fn source(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("workflow.star");
        std::fs::write(&path, SOURCE).unwrap();
        path
    }

    #[test]
    fn an_input_path_names_the_parameter_and_a_plain_file_name() {
        assert_eq!(
            input_path("notes", Path::new("/s/param-files/ab/notes.md")),
            "inputs/params/notes/notes.md"
        );
        assert_eq!(
            input_path("notes", Path::new("/s/../.hidden name")),
            "inputs/params/notes/hidden_name"
        );
        assert_eq!(
            input_path("notes", Path::new("/")),
            "inputs/params/notes/file"
        );
    }

    #[test]
    fn a_readable_file_is_kept_and_a_json_parameter_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.md");
        std::fs::write(&file, "hello").unwrap();
        let raw = serde_json::json!({"path": file, "digest": "sha256:00"}).to_string();
        let supplied = BTreeMap::from([
            ("notes".to_owned(), raw.clone()),
            (
                "config".to_owned(),
                r#"{"path": "/nope", "digest": "x"}"#.to_owned(),
            ),
        ]);
        let localized = localize(&source(dir.path()), &supplied, dir.path(), None).unwrap();
        assert_eq!(localized.supplied, supplied);
        assert_eq!(
            localized.files,
            [ParamFile {
                name: "notes".into(),
                path: file
            }]
        );
    }

    #[test]
    fn a_file_off_this_disk_is_reused_when_staged_and_refused_without_a_controller() {
        let dir = tempfile::tempdir().unwrap();
        let digest = crucible_contract::artifact::content_digest(b"hello");
        let raw = serde_json::json!({
            "path": "/controller/scratch/notes.md",
            "digest": digest,
            "size": 5,
            "media_type": "text/markdown",
        })
        .to_string();
        let supplied = BTreeMap::from([("notes".to_owned(), raw)]);
        let error = localize(&source(dir.path()), &supplied, dir.path(), None).unwrap_err();
        assert!(format!("{error:#}").contains("no controller"), "{error:#}");

        let staged = dir
            .path()
            .join(STAGE_DIR)
            .join(digest.trim_start_matches("sha256:"))
            .join("notes.md");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, "hello").unwrap();
        let localized = localize(&source(dir.path()), &supplied, dir.path(), None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&localized.supplied["notes"]).unwrap();
        assert_eq!(value["path"], staged.to_string_lossy().as_ref());
        assert_eq!(value["digest"], digest);
        assert_eq!(value["size"], 5);
        assert_eq!(value["media_type"], "text/markdown");
        assert_eq!(localized.files[0].path, staged);

        std::fs::write(&staged, "tampered").unwrap();
        let error = localize(&source(dir.path()), &supplied, dir.path(), None).unwrap_err();
        assert!(format!("{error:#}").contains("no controller"), "{error:#}");
    }

    #[test]
    fn a_digest_that_is_not_sha256_hex_is_refused_before_any_path_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let raw =
            serde_json::json!({"path": "/x/notes.md", "digest": "sha256:../../etc"}).to_string();
        let supplied = BTreeMap::from([("notes".to_owned(), raw)]);
        let error = localize(&source(dir.path()), &supplied, dir.path(), None).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a sha256 digest"),
            "{error:#}"
        );
        assert!(!dir.path().join(STAGE_DIR).exists());
    }

    #[test]
    fn materialized_parameters_are_read_only_and_replace_an_earlier_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.md");
        std::fs::write(&file, "one").unwrap();
        let files = [ParamFile {
            name: "notes".into(),
            path: file.clone(),
        }];
        let root = dir.path().join("root");
        materialize(&root, &files).unwrap();
        std::fs::write(&file, "two").unwrap();
        materialize(&root, &files).unwrap();
        let staged = root.join("inputs/params/notes/notes.md");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "two");
        assert!(std::fs::metadata(&staged).unwrap().permissions().readonly());
    }
}
