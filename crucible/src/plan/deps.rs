//! What a pack pins from outside itself: Starlark libraries (`[deps]`) and tool binaries
//! (`[tools]`) in the manifest, `crucible.lock` beside it, and a cache of what was fetched, keyed
//! by digest.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use base64::Engine;
use crucible_contract::pack_tree::{PackTree, TreeDigest, TreeError, walk_dir};
use serde::{Deserialize, Serialize};

use crate::errors::FileError;

pub const MANIFEST_FILE: &str = "crucible.toml";
pub const LOCK_FILE: &str = "crucible.lock";
/// Overrides the cache directory.
pub const CACHE_ENV: &str = "CRUCIBLE_DEPS_DIR";

const LOCK_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 64;
const TREE_PREFIX: &str = "tree1:";
const ALLOWED_PROTOCOLS: &str = "file:https:ssh";
const PLATFORM_PLACEHOLDER: &str = "{platform}";
const MAX_TOOL_BYTES: u64 = 256 * 1024 * 1024;
const TOOLS_DIR: &str = "tools";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "dependency name {name:?} must start with a lowercase letter and hold only lowercase \
         letters, digits, `-` and `_`, at most {MAX_NAME_BYTES} bytes"
    )]
    InvalidName { name: String },
    #[error("dependency \"{name}\": {field} {value:?} may not be empty or start with `-`")]
    InvalidSpec {
        name: DepName,
        field: &'static str,
        value: String,
    },
    #[error(transparent)]
    File(#[from] FileError),
    #[error("parsing {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("{} has lock version {found}; this build reads version {LOCK_VERSION}", path.display())]
    LockVersion { path: PathBuf, found: u32 },
    #[error("{} lists dependency \"{name}\" twice", path.display())]
    DuplicateLock { path: PathBuf, name: DepName },
    #[error("no cache directory: set {CACHE_ENV}, XDG_CACHE_HOME or HOME")]
    NoCacheDir,
    #[error("running git: {0}")]
    GitSpawn(#[source] std::io::Error),
    #[error("dependency \"{name}\": {operation} failed: {stderr}")]
    Git {
        name: DepName,
        operation: &'static str,
        stderr: String,
    },
    #[error("dependency \"{name}\": {error}")]
    Tree {
        name: DepName,
        #[source]
        error: TreeError,
    },
    #[error("dependency \"{name}\" holds no files at {rev}")]
    Empty { name: DepName, rev: String },
    #[error(
        "dependency \"{name}\" at {commit} has tree {found}, but {LOCK_FILE} pins {locked}; run \
         `crucible deps lock` if the change is intended"
    )]
    DigestMismatch {
        name: DepName,
        commit: String,
        locked: TreeDigest,
        found: TreeDigest,
    },
    #[error("{} lists tool \"{name}\" for {platform} twice", path.display())]
    DuplicateTool {
        path: PathBuf,
        name: DepName,
        platform: Platform,
    },
    #[error(
        "unknown platform {raw:?}; expected one of linux_amd64, linux_arm64, darwin_amd64, darwin_arm64"
    )]
    UnknownPlatform { raw: String },
    #[error("tool \"{name}\": downloading {url}: {message}")]
    Download {
        name: DepName,
        url: String,
        message: String,
    },
    #[error("tool \"{name}\": {url} is larger than {MAX_TOOL_BYTES} bytes")]
    ToolTooLarge { name: DepName, url: String },
    #[error("tool \"{name}\": no platform it names has a download")]
    NoPlatform { name: DepName },
    #[error(
        "tool \"{name}\" for {platform} is {found}, but {LOCK_FILE} pins {locked}; run `crucible \
         deps lock` if the change is intended"
    )]
    ToolDigestMismatch {
        name: DepName,
        platform: Platform,
        locked: String,
        found: String,
    },
    #[error("tool \"{name}\": the archive at {url} has no file {member:?}")]
    NoMember {
        name: DepName,
        url: String,
        member: String,
    },
    #[error("tool \"{name}\": reading the archive at {url}: {message}")]
    Archive {
        name: DepName,
        url: String,
        message: String,
    },
    #[error("{LOCK_FILE} pins tool \"{name}\" for no {platform} download")]
    ToolNotPinned { name: DepName, platform: Platform },
    #[error("fetching tools from the controller needs {var} in the environment")]
    NoController { var: &'static str },
    #[error("tool \"{name}\" ({sha256}) is not in the cache; run `crucible deps fetch`")]
    ToolNotFetched { name: DepName, sha256: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// The name a pack gives a dependency, as written in `[deps.<name>]` and `@<name>//path`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DepName(String);

impl DepName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DepName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for DepName {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self> {
        let mut chars = name.chars();
        let starts = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let rest =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if starts && rest && name.len() <= MAX_NAME_BYTES {
            Ok(DepName(name.to_owned()))
        } else {
            Err(Error::InvalidName {
                name: name.to_owned(),
            })
        }
    }
}

impl TryFrom<String> for DepName {
    type Error = Error;

    fn try_from(name: String) -> Result<Self> {
        name.parse()
    }
}

impl From<DepName> for String {
    fn from(name: DepName) -> String {
        name.0
    }
}

/// Which tree a path is resolved in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Root {
    Pack,
    Dep(DepName),
}

/// Split `@name//path` into its dependency and the path under it. `None` when `raw` does not
/// start with `@`.
pub fn split(raw: &str) -> Option<Result<(DepName, &str)>> {
    let rest = raw.strip_prefix('@')?;
    let (name, path) = rest.split_once("//").unwrap_or((rest, ""));
    Some(name.parse().map(|name| (name, path)))
}

/// One `[deps.<name>]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepSpec {
    /// A git URL over https, ssh or file.
    pub git: String,
    /// A branch, tag or commit.
    pub rev: String,
}

pub type DepSpecs = BTreeMap<DepName, DepSpec>;

/// An operating system and CPU a tool binary runs on, spelled `linux_amd64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Platform {
    LinuxAmd64,
    LinuxArm64,
    DarwinAmd64,
    DarwinArm64,
}

impl Platform {
    const ALL: [Platform; 4] = [
        Platform::LinuxAmd64,
        Platform::LinuxArm64,
        Platform::DarwinAmd64,
        Platform::DarwinArm64,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::LinuxAmd64 => "linux_amd64",
            Platform::LinuxArm64 => "linux_arm64",
            Platform::DarwinAmd64 => "darwin_amd64",
            Platform::DarwinArm64 => "darwin_arm64",
        }
    }

    /// The platform this process runs on, or `None` on one no tool is pinned for.
    pub fn host() -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some(Platform::LinuxAmd64),
            ("linux", "aarch64") => Some(Platform::LinuxArm64),
            ("macos", "x86_64") => Some(Platform::DarwinAmd64),
            ("macos", "aarch64") => Some(Platform::DarwinArm64),
            _ => None,
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for Platform {
    type Error = Error;

    fn try_from(raw: String) -> Result<Self> {
        Platform::ALL
            .into_iter()
            .find(|platform| platform.as_str() == raw)
            .ok_or(Error::UnknownPlatform { raw })
    }
}

impl From<Platform> for String {
    fn from(platform: Platform) -> String {
        platform.as_str().to_owned()
    }
}

/// One `[tools.<name>]` table: a binary command tasks find on `PATH` under that name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    /// Where the binary, or a `.tar.gz` holding it, downloads from. `{platform}` is replaced
    /// with each platform's name.
    pub url: String,
    /// The file inside the archive, by path or by file name. Defaults to the tool's name.
    #[serde(default)]
    pub member: Option<String>,
    /// The platforms to pin. Defaults to both Linux platforms and this machine's, leaving out
    /// any the URL has no download for.
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

pub type ToolSpecs = BTreeMap<DepName, ToolSpec>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedTool {
    pub name: DepName,
    pub platform: Platform,
    pub url: String,
    /// The `sha256:<hex>` of the downloaded bytes.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedDep {
    pub name: DepName,
    pub git: String,
    pub rev: String,
    pub commit: String,
    pub tree: TreeDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    version: u32,
    #[serde(default, rename = "dep")]
    pub deps: Vec<LockedDep>,
    #[serde(default, rename = "tool", skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<LockedTool>,
}

impl Lock {
    /// The pack's lock, or `None` when the pack has no `crucible.lock`.
    pub fn read(pack_dir: &Path) -> Result<Option<Self>> {
        let path = pack_dir.join(LOCK_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(FileError::at("reading lock file", &path)(error).into()),
        };
        Self::parse(&text, &path).map(Some)
    }

    /// Parse the text of a lock file read from `path`.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let lock: Lock = toml::from_str(text).map_err(|error| Error::Parse {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if lock.version != LOCK_VERSION {
            return Err(Error::LockVersion {
                path,
                found: lock.version,
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for dep in &lock.deps {
            if !seen.insert(&dep.name) {
                return Err(Error::DuplicateLock {
                    path,
                    name: dep.name.clone(),
                });
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for tool in &lock.tools {
            if !seen.insert((&tool.name, tool.platform)) {
                return Err(Error::DuplicateTool {
                    path,
                    name: tool.name.clone(),
                    platform: tool.platform,
                });
            }
        }
        Ok(lock)
    }

    fn write(&self, pack_dir: &Path) -> Result<()> {
        let path = pack_dir.join(LOCK_FILE);
        let text = toml::to_string(self).map_err(|error| Error::Parse {
            path: path.clone(),
            message: error.to_string(),
        })?;
        std::fs::write(&path, text).map_err(FileError::at("writing lock file", &path))?;
        Ok(())
    }
}

/// The `[deps]` and `[tools]` tables of the pack's manifest. A pack with no manifest has
/// neither.
pub fn read_specs(pack_dir: &Path) -> Result<(DepSpecs, ToolSpecs)> {
    #[derive(Deserialize)]
    struct OnlyDeps {
        #[serde(default)]
        deps: DepSpecs,
        #[serde(default)]
        tools: ToolSpecs,
    }
    let path = pack_dir.join(MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((DepSpecs::new(), ToolSpecs::new()));
        }
        Err(error) => return Err(FileError::at("reading manifest", &path)(error).into()),
    };
    let manifest: OnlyDeps = toml::from_str(&text).map_err(|error| Error::Parse {
        path,
        message: error.to_string(),
    })?;
    Ok((manifest.deps, manifest.tools))
}

/// Fetched trees on disk, one directory per tree digest.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Cache { root: root.into() }
    }

    pub fn from_env() -> Result<Self> {
        let set = |key: &str| std::env::var_os(key).filter(|value| !value.is_empty());
        if let Some(dir) = set(CACHE_ENV) {
            return Ok(Cache::at(dir));
        }
        if let Some(dir) = set("XDG_CACHE_HOME") {
            return Ok(Cache::at(PathBuf::from(dir).join("crucible/deps")));
        }
        match set("HOME") {
            Some(home) => Ok(Cache::at(PathBuf::from(home).join(".cache/crucible/deps"))),
            None => Err(Error::NoCacheDir),
        }
    }

    /// The cache beside a pack directory: `<pack>.deps` next to `<pack>`. It is read before the
    /// user's cache, so whoever writes a pack to disk can put its dependencies with it.
    pub fn beside(pack_dir: &Path) -> Option<Self> {
        let pack_dir = std::fs::canonicalize(pack_dir).ok()?;
        let mut name = pack_dir.file_name()?.to_owned();
        name.push(".deps");
        Some(Cache::at(pack_dir.parent()?.join(name)))
    }

    pub fn dir(&self, tree: &TreeDigest) -> PathBuf {
        let digest = tree.as_str();
        self.root
            .join(digest.strip_prefix(TREE_PREFIX).unwrap_or(digest))
    }

    pub fn has(&self, tree: &TreeDigest) -> bool {
        self.dir(tree).is_dir()
    }

    /// The directory holding the one binary of the tool whose download hashes to `sha256`.
    pub fn tool_dir(&self, sha256: &str) -> PathBuf {
        self.root
            .join(TOOLS_DIR)
            .join(sha256.strip_prefix("sha256:").unwrap_or(sha256))
    }

    pub fn has_tool(&self, tool: &LockedTool) -> bool {
        self.tool_dir(&tool.sha256)
            .join(tool.name.as_str())
            .is_file()
    }

    /// Write `binary` as the executable `name` of the tool pinned at `sha256`, unless it is
    /// already there.
    pub fn put_tool(&self, sha256: &str, name: &DepName, binary: &[u8]) -> Result<()> {
        let dest = self.tool_dir(sha256);
        if dest.join(name.as_str()).is_file() {
            return Ok(());
        }
        let tools = self.root.join(TOOLS_DIR);
        std::fs::create_dir_all(&tools).map_err(FileError::at("creating tool cache", &tools))?;
        let staged = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&tools)
            .map_err(FileError::at("creating staging directory", &tools))?;
        let file = staged.path().join(name.as_str());
        std::fs::write(&file, binary).map_err(FileError::at("writing tool", &file))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                .map_err(FileError::at("marking tool executable", &file))?;
        }
        let staged = staged.keep();
        if let Err(error) = std::fs::rename(&staged, &dest) {
            let _ = std::fs::remove_dir_all(&staged);
            if !dest.is_dir() {
                return Err(FileError::at("moving tool into the cache", &dest)(error).into());
            }
        }
        Ok(())
    }

    /// Write `tree` under its digest unless it is already there.
    pub fn put(&self, tree: &PackTree) -> Result<()> {
        let dest = self.dir(&tree.digest());
        if dest.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)
            .map_err(FileError::at("creating dependency cache", &self.root))?;
        let staged = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&self.root)
            .map_err(FileError::at("creating staging directory", &self.root))?;
        for (path, bytes) in tree.files() {
            let file = staged.path().join(path.as_str());
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(FileError::at("creating dependency directory", parent))?;
            }
            std::fs::write(&file, bytes)
                .map_err(FileError::at("writing dependency file", &file))?;
        }
        let staged = staged.keep();
        if let Err(error) = std::fs::rename(&staged, &dest) {
            let _ = std::fs::remove_dir_all(&staged);
            if !dest.is_dir() {
                return Err(FileError::at("moving dependency into the cache", &dest)(error).into());
            }
        }
        Ok(())
    }
}

/// How git authenticates a fetch.
#[derive(Debug, Clone)]
pub enum Auth {
    /// Whatever the machine's git already has: credential helpers and SSH agents.
    Ambient,
    /// A token sent only to `https://<host>/` URLs, such as a GitHub App installation token.
    Token { host: String, token: String },
}

impl Auth {
    /// The token to send to `url`: only over https, to the token's host or its `api.` host.
    fn bearer(&self, url: &str) -> Option<&str> {
        let Auth::Token { host, token } = self else {
            return None;
        };
        let rest = url.strip_prefix("https://")?;
        let authority = rest.split('/').next()?;
        (authority == host || authority.strip_prefix("api.") == Some(host.as_str()))
            .then_some(token.as_str())
    }

    fn env(&self, url: &str) -> Vec<(&'static str, String)> {
        let Auth::Token { host, token } = self else {
            return Vec::new();
        };
        if !url.starts_with(&format!("https://{host}/")) {
            return Vec::new();
        }
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        vec![
            ("GIT_CONFIG_COUNT", "1".to_owned()),
            ("GIT_CONFIG_KEY_0", "http.extraHeader".to_owned()),
            (
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Basic {basic}"),
            ),
        ]
    }
}

/// A locked dependency and where its tree is on this machine.
#[derive(Debug, Clone)]
pub struct Located {
    pub tree: TreeDigest,
    /// The canonical cache directory, or `None` when the tree has not been fetched.
    pub dir: Option<PathBuf>,
}

/// Every dependency the pack's lock lists, looked up beside the pack and then in the user's
/// cache. A pack with no lock has none.
pub fn locate(pack_dir: &Path) -> Result<BTreeMap<DepName, Located>> {
    let Some(lock) = Lock::read(pack_dir)? else {
        return Ok(BTreeMap::new());
    };
    let caches = [Cache::beside(pack_dir), Cache::from_env().ok()];
    Ok(lock
        .deps
        .into_iter()
        .map(|dep| {
            let dir = caches
                .iter()
                .flatten()
                .map(|cache| cache.dir(&dep.tree))
                .find(|dir| dir.is_dir())
                .and_then(|dir| std::fs::canonicalize(dir).ok());
            (
                dep.name,
                Located {
                    tree: dep.tree,
                    dir,
                },
            )
        })
        .collect())
}

/// Resolve every `[deps]` entry to a commit and a tree digest, cache the trees, and write
/// `crucible.lock`.
pub fn lock(pack_dir: &Path, cache: &Cache, auth: &Auth) -> Result<Lock> {
    let mut deps = Vec::new();
    let (dep_specs, tool_specs) = read_specs(pack_dir)?;
    for (name, spec) in dep_specs {
        let fetched = fetch(&name, &spec.git, &spec.rev, cache, auth, None)?;
        deps.push(LockedDep {
            name,
            git: spec.git,
            rev: spec.rev,
            commit: fetched.commit,
            tree: fetched.tree.digest(),
        });
    }
    let mut tools = Vec::new();
    for (name, spec) in tool_specs {
        tools.extend(lock_tool(&name, &spec, cache, auth)?);
    }
    let lock = Lock {
        version: LOCK_VERSION,
        deps,
        tools,
    };
    lock.write(pack_dir)?;
    Ok(lock)
}

/// Download `spec` for each of its platforms and pin what came back.
fn lock_tool(
    name: &DepName,
    spec: &ToolSpec,
    cache: &Cache,
    auth: &Auth,
) -> Result<Vec<LockedTool>> {
    let explicit = spec.platforms.is_some();
    let platforms = spec.platforms.clone().unwrap_or_else(|| {
        let mut platforms = vec![Platform::LinuxAmd64, Platform::LinuxArm64];
        platforms.extend(Platform::host().filter(|host| !platforms.contains(host)));
        platforms
    });
    let mut locked = Vec::new();
    for platform in platforms {
        let url = spec.url.replace(PLATFORM_PLACEHOLDER, platform.as_str());
        let bytes = match download(name, &url, auth.bearer(&url))? {
            Some(bytes) => bytes,
            None if explicit => {
                return Err(Error::Download {
                    name: name.clone(),
                    url,
                    message: "not found".to_owned(),
                });
            }
            None => continue,
        };
        let tool = LockedTool {
            name: name.clone(),
            platform,
            url,
            sha256: crucible_contract::content_digest(&bytes),
            member: spec.member.clone(),
        };
        if Platform::host() == Some(platform) {
            install(&tool, &bytes, cache)?;
        }
        locked.push(tool);
        if !spec.url.contains(PLATFORM_PLACEHOLDER) && !explicit {
            break;
        }
    }
    if locked.is_empty() {
        return Err(Error::NoPlatform { name: name.clone() });
    }
    Ok(locked)
}

/// The bytes at `url`, or `None` when the server has nothing there.
fn download(name: &DepName, url: &str, bearer: Option<&str>) -> Result<Option<Vec<u8>>> {
    let failed = |message: String| Error::Download {
        name: name.clone(),
        url: url.to_owned(),
        message,
    };
    let (name, url, bearer) = (name.clone(), url.to_owned(), bearer.map(str::to_owned));
    let target = url.clone();
    let fetched = std::thread::spawn(move || -> std::result::Result<_, String> {
        let url = target;
        use std::io::Read;
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|error| error.to_string())?;
        let mut request = client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/octet-stream");
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().map_err(|error| error.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        response
            .take(MAX_TOOL_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(Some(bytes))
    })
    .join()
    .map_err(|_| failed("the download thread panicked".to_owned()))?
    .map_err(failed)?;
    match fetched {
        Some(bytes) if bytes.len() as u64 > MAX_TOOL_BYTES => {
            Err(Error::ToolTooLarge { name, url })
        }
        fetched => Ok(fetched),
    }
}

/// Take `tool`'s binary out of its download and write it to `cache`.
fn install(tool: &LockedTool, bytes: &[u8], cache: &Cache) -> Result<()> {
    let path = tool.url.split(['?', '#']).next().unwrap_or(&tool.url);
    let binary = if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        let member = tool.member.as_deref().unwrap_or(tool.name.as_str());
        unpack_member(tool, bytes, member)?
    } else {
        bytes.to_vec()
    };
    cache.put_tool(&tool.sha256, &tool.name, &binary)
}

fn unpack_member(tool: &LockedTool, bytes: &[u8], member: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let archive = |message: String| Error::Archive {
        name: tool.name.clone(),
        url: tool.url.clone(),
        message,
    };
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in tar.entries().map_err(|e| archive(e.to_string()))? {
        let mut entry = entry.map_err(|e| archive(e.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| archive(e.to_string()))?
            .into_owned();
        let named = path == Path::new(member)
            || path
                .strip_prefix(".")
                .is_ok_and(|rest| rest == Path::new(member))
            || path.file_name().is_some_and(|file| file == member);
        if named {
            let mut binary = Vec::new();
            entry
                .by_ref()
                .take(MAX_TOOL_BYTES + 1)
                .read_to_end(&mut binary)
                .map_err(|e| archive(e.to_string()))?;
            if binary.len() as u64 > MAX_TOOL_BYTES {
                return Err(Error::ToolTooLarge {
                    name: tool.name.clone(),
                    url: tool.url.clone(),
                });
            }
            return Ok(binary);
        }
    }
    Err(Error::NoMember {
        name: tool.name.clone(),
        url: tool.url.clone(),
        member: member.to_owned(),
    })
}

/// `bytes` when they are the download `tool` pins.
fn verified(tool: &LockedTool, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let found = crucible_contract::content_digest(&bytes);
    if found != tool.sha256 {
        return Err(Error::ToolDigestMismatch {
            name: tool.name.clone(),
            platform: tool.platform,
            locked: tool.sha256.clone(),
            found,
        });
    }
    Ok(bytes)
}

/// The download `tool` pins, refused when its digest differs from the lock.
pub fn download_tool(tool: &LockedTool, auth: &Auth) -> Result<Vec<u8>> {
    let bytes = download(&tool.name, &tool.url, auth.bearer(&tool.url))?.ok_or_else(|| {
        Error::Download {
            name: tool.name.clone(),
            url: tool.url.clone(),
            message: "not found".to_owned(),
        }
    })?;
    verified(tool, bytes)
}

/// Fetch one locked tool into `cache`, refusing a download whose digest differs from the lock.
pub fn fetch_tool(tool: &LockedTool, cache: &Cache, auth: &Auth) -> Result<()> {
    install(tool, &download_tool(tool, auth)?, cache)
}

/// A controller that serves a run the downloads its pack's lock pins.
#[derive(Debug, Clone)]
pub struct Controller {
    pub url: String,
    pub token: String,
}

impl Controller {
    /// The controller and pod token a run pod is given.
    pub fn from_env() -> Result<Self> {
        let var = |var: &'static str| {
            std::env::var(var)
                .ok()
                .filter(|value| !value.is_empty())
                .ok_or(Error::NoController { var })
        };
        Ok(Controller {
            url: var(crucible_contract::ENV_CONTROLLER_URL)?,
            token: var(crucible_contract::ENV_POD_TOKEN)?,
        })
    }

    fn tool_url(&self, tool: &LockedTool) -> String {
        format!(
            "{}{}/{}",
            self.url.trim_end_matches('/'),
            crucible_contract::TOOLS_PATH,
            tool.sha256.strip_prefix("sha256:").unwrap_or(&tool.sha256)
        )
    }
}

/// Fetch every locked tool for this machine that the cache lacks from `controller`, and refuse
/// one whose digest differs from the lock. Returns the names fetched.
pub fn fetch_tools_from(
    pack_dir: &Path,
    cache: &Cache,
    controller: &Controller,
) -> Result<Vec<DepName>> {
    let Some(lock) = Lock::read(pack_dir)? else {
        return Ok(Vec::new());
    };
    let mut fetched = Vec::new();
    for tool in lock.tools {
        if Platform::host() != Some(tool.platform) || cache.has_tool(&tool) {
            continue;
        }
        let url = controller.tool_url(&tool);
        let bytes = download(&tool.name, &url, Some(&controller.token))?.ok_or_else(|| {
            Error::Download {
                name: tool.name.clone(),
                url,
                message: "the controller does not hold it; save the pack again".to_owned(),
            }
        })?;
        install(&tool, &verified(&tool, bytes)?, cache)?;
        fetched.push(tool.name);
    }
    Ok(fetched)
}

/// The directories to put on `PATH` so the pack's tools resolve by name on this machine, looked
/// up beside the pack and then in the user's cache. A pack with no lock has none.
pub fn tool_dirs(pack_dir: &Path) -> Result<Vec<PathBuf>> {
    let Some(lock) = Lock::read(pack_dir)? else {
        return Ok(Vec::new());
    };
    let names: std::collections::BTreeSet<&DepName> =
        lock.tools.iter().map(|tool| &tool.name).collect();
    let caches = [Cache::beside(pack_dir), Cache::from_env().ok()];
    let mut dirs = Vec::new();
    for name in names {
        let pinned = Platform::host().and_then(|host| {
            lock.tools
                .iter()
                .find(|tool| &tool.name == name && tool.platform == host)
        });
        let Some(tool) = pinned else {
            return Err(Error::ToolNotPinned {
                name: name.clone(),
                platform: Platform::host().unwrap_or(Platform::LinuxAmd64),
            });
        };
        let dir = caches
            .iter()
            .flatten()
            .map(|cache| cache.tool_dir(&tool.sha256))
            .find(|dir| dir.join(name.as_str()).is_file())
            .ok_or_else(|| Error::ToolNotFetched {
                name: name.clone(),
                sha256: tool.sha256.clone(),
            })?;
        dirs.push(dir);
    }
    Ok(dirs)
}

/// Fetch every locked tree and every tool for this machine that the cache lacks, and refuse one
/// whose digest differs from the lock. Returns the names fetched.
pub fn fetch_locked(pack_dir: &Path, cache: &Cache, auth: &Auth) -> Result<Vec<DepName>> {
    let Some(lock) = Lock::read(pack_dir)? else {
        return Ok(Vec::new());
    };
    let mut fetched = Vec::new();
    for dep in lock.deps {
        if cache.has(&dep.tree) {
            continue;
        }
        fetch_dep(&dep, cache, auth)?;
        fetched.push(dep.name);
    }
    for tool in lock.tools {
        if Platform::host() != Some(tool.platform) || cache.has_tool(&tool) {
            continue;
        }
        fetch_tool(&tool, cache, auth)?;
        fetched.push(tool.name);
    }
    Ok(fetched)
}

/// Fetch one locked dependency at its locked commit into the cache and return its tree. A tree
/// whose digest differs from the lock is refused.
pub fn fetch_dep(dep: &LockedDep, cache: &Cache, auth: &Auth) -> Result<PackTree> {
    let fetched = fetch(
        &dep.name,
        &dep.git,
        &dep.commit,
        cache,
        auth,
        Some(&dep.tree),
    )?;
    Ok(fetched.tree)
}

struct Fetched {
    commit: String,
    tree: PackTree,
}

fn fetch(
    name: &DepName,
    url: &str,
    rev: &str,
    cache: &Cache,
    auth: &Auth,
    locked: Option<&TreeDigest>,
) -> Result<Fetched> {
    for (field, value) in [("git", url), ("rev", rev)] {
        if value.trim().is_empty() || value.starts_with('-') {
            return Err(Error::InvalidSpec {
                name: name.clone(),
                field,
                value: value.to_owned(),
            });
        }
    }
    std::fs::create_dir_all(&cache.root)
        .map_err(FileError::at("creating dependency cache", &cache.root))?;
    let checkout = tempfile::Builder::new()
        .prefix(".fetch-")
        .tempdir_in(&cache.root)
        .map_err(FileError::at("creating fetch directory", &cache.root))?;
    let git = |operation: &'static str, args: &[&str]| -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(checkout.path())
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", ALLOWED_PROTOCOLS)
            .envs(auth.env(url))
            .output()
            .map_err(Error::GitSpawn)?;
        if !output.status.success() {
            return Err(Error::Git {
                name: name.clone(),
                operation,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    git("git init", &["init", "--quiet"])?;
    git(
        "git fetch",
        &["fetch", "--quiet", "--depth", "1", "--", url, rev],
    )?;
    let commit = git("git rev-parse", &["rev-parse", "FETCH_HEAD^{commit}"])?;
    git(
        "git checkout",
        &["checkout", "--quiet", "--detach", &commit],
    )?;

    let tree = walk_dir(checkout.path())
        .map_err(|error| Error::Tree {
            name: name.clone(),
            error,
        })?
        .tree;
    if tree.is_empty() {
        return Err(Error::Empty {
            name: name.clone(),
            rev: rev.to_owned(),
        });
    }
    let digest = tree.digest();
    if let Some(locked) = locked.filter(|locked| **locked != digest) {
        return Err(Error::DigestMismatch {
            name: name.clone(),
            commit,
            locked: locked.clone(),
            found: digest,
        });
    }

    cache.put(&tree)?;
    Ok(Fetched { commit, tree })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A library repo with a `v1` tag, and a later commit on the branch that changes it.
    fn library(root: &Path) -> (String, String) {
        let repo = root.join("library");
        std::fs::create_dir_all(repo.join("prompts")).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch", "main"]);
        std::fs::write(repo.join("gate.star"), "VERSION = 1\n").unwrap();
        std::fs::write(repo.join("prompts/review.md"), "Review it.\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "one"]);
        git(&repo, &["tag", "v1"]);
        let v1 = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("gate.star"), "VERSION = 2\n").unwrap();
        git(&repo, &["commit", "--quiet", "-am", "two"]);
        (format!("file://{}", repo.display()), v1)
    }

    fn pack(root: &Path, url: &str, rev: &str) -> PathBuf {
        let pack = root.join("pack");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(
            pack.join(MANIFEST_FILE),
            format!("[deps.gates]\ngit = \"{url}\"\nrev = \"{rev}\"\n"),
        )
        .unwrap();
        pack
    }

    #[test]
    fn lock_pins_the_commit_and_tree_and_caches_exactly_the_tree() {
        let root = tempfile::tempdir().unwrap();
        let (url, v1) = library(root.path());
        let pack = pack(root.path(), &url, "v1");
        let cache = Cache::at(root.path().join("cache"));

        let lock = lock(&pack, &cache, &Auth::Ambient).unwrap();
        let [dep] = lock.deps.as_slice() else {
            panic!("one dep: {lock:?}")
        };
        assert_eq!(dep.name.as_str(), "gates");
        assert_eq!(dep.rev, "v1");
        assert_eq!(dep.commit, v1);
        assert_eq!(Lock::read(&pack).unwrap(), Some(lock.clone()));

        let dir = cache.dir(&dep.tree);
        assert_eq!(
            std::fs::read_to_string(dir.join("gate.star")).unwrap(),
            "VERSION = 1\n"
        );
        assert!(!dir.join(".git").exists());
        let cached = walk_dir(&dir).unwrap();
        assert_eq!(cached.tree.digest(), dep.tree);
        assert!(cached.ignored.is_empty(), "{:?}", cached.ignored);
        let leftovers: Vec<_> = std::fs::read_dir(root.path().join("cache"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_branch_locks_to_its_tip_and_a_different_tree() {
        let root = tempfile::tempdir().unwrap();
        let (url, v1) = library(root.path());
        let cache = Cache::at(root.path().join("cache"));
        let tagged = lock(&pack(root.path(), &url, "v1"), &cache, &Auth::Ambient).unwrap();
        let branch = lock(&pack(root.path(), &url, "main"), &cache, &Auth::Ambient).unwrap();
        assert_ne!(branch.deps[0].commit, v1);
        assert_ne!(branch.deps[0].tree, tagged.deps[0].tree);
        assert_eq!(
            std::fs::read_to_string(cache.dir(&branch.deps[0].tree).join("gate.star")).unwrap(),
            "VERSION = 2\n"
        );
    }

    #[test]
    fn fetch_locked_fills_an_empty_cache_and_skips_a_full_one() {
        let root = tempfile::tempdir().unwrap();
        let (url, _) = library(root.path());
        let pack = pack(root.path(), &url, "v1");
        let lock = lock(&pack, &Cache::at(root.path().join("first")), &Auth::Ambient).unwrap();

        let cache = Cache::at(root.path().join("second"));
        let fetched = fetch_locked(&pack, &cache, &Auth::Ambient).unwrap();
        assert_eq!(fetched, [lock.deps[0].name.clone()]);
        assert_eq!(
            walk_dir(&cache.dir(&lock.deps[0].tree))
                .unwrap()
                .tree
                .digest(),
            lock.deps[0].tree
        );
        assert!(
            fetch_locked(&pack, &cache, &Auth::Ambient)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fetch_locked_refuses_a_tree_that_differs_from_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let (url, _) = library(root.path());
        let pack = pack(root.path(), &url, "v1");
        let cache = Cache::at(root.path().join("cache"));
        let locked = lock(&pack, &cache, &Auth::Ambient).unwrap();
        let other = lock(
            &self::pack(root.path(), &url, "main"),
            &cache,
            &Auth::Ambient,
        )
        .unwrap();

        let tampered = Lock {
            version: LOCK_VERSION,
            deps: vec![LockedDep {
                tree: other.deps[0].tree.clone(),
                ..locked.deps[0].clone()
            }],
            tools: Vec::new(),
        };
        tampered.write(&pack).unwrap();
        let empty = Cache::at(root.path().join("empty"));
        let error = fetch_locked(&pack, &empty, &Auth::Ambient).unwrap_err();
        assert!(
            matches!(&error, Error::DigestMismatch { locked: pinned, found, .. }
                if *pinned == other.deps[0].tree && *found == locked.deps[0].tree),
            "{error}"
        );
        assert!(!empty.dir(&other.deps[0].tree).exists());
    }

    #[test]
    fn a_missing_rev_and_an_option_shaped_value_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let (url, _) = library(root.path());
        let cache = Cache::at(root.path().join("cache"));
        let error = lock(&pack(root.path(), &url, "nope"), &cache, &Auth::Ambient).unwrap_err();
        assert!(
            matches!(
                error,
                Error::Git {
                    operation: "git fetch",
                    ..
                }
            ),
            "{error}"
        );
        for (url, rev) in [
            ("--upload-pack=touch /tmp/x", "v1"),
            (url.as_str(), "--all"),
        ] {
            let error = lock(&pack(root.path(), url, rev), &cache, &Auth::Ambient).unwrap_err();
            assert!(matches!(error, Error::InvalidSpec { .. }), "{error}");
        }
        let error = lock(
            &pack(root.path(), "ext::sh -c 'touch /tmp/x'", "v1"),
            &cache,
            &Auth::Ambient,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Git { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_repo_holding_a_symlink_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (url, _) = library(root.path());
        let repo = root.path().join("library");
        std::os::unix::fs::symlink("/etc/passwd", repo.join("link")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "link"]);
        let cache = Cache::at(root.path().join("cache"));
        let error = lock(&pack(root.path(), &url, "main"), &cache, &Auth::Ambient).unwrap_err();
        assert!(matches!(error, Error::Tree { .. }), "{error}");
    }

    #[test]
    fn a_token_goes_only_to_its_own_https_host() {
        let auth = Auth::Token {
            host: "github.com".to_owned(),
            token: "t0ken".to_owned(),
        };
        let env = auth.env("https://github.com/org/repo");
        let expected = base64::engine::general_purpose::STANDARD.encode("x-access-token:t0ken");
        assert_eq!(
            env.last().map(|(_, value)| value.as_str()),
            Some(format!("Authorization: Basic {expected}").as_str())
        );
        for url in [
            "https://github.com.evil.example/org/repo",
            "https://evil.example/github.com/",
            "http://github.com/org/repo",
            "ssh://git@github.com/org/repo",
        ] {
            assert!(auth.env(url).is_empty(), "{url}");
        }
        assert!(Auth::Ambient.env("https://github.com/org/repo").is_empty());
    }

    #[test]
    fn a_download_token_goes_only_to_its_host_and_its_api_host_over_https() {
        let auth = Auth::Token {
            host: "github.com".to_owned(),
            token: "t0ken".to_owned(),
        };
        for url in [
            "https://github.com/org/repo/releases/download/v1/tool.tar.gz",
            "https://api.github.com/repos/org/repo/releases/assets/1",
        ] {
            assert_eq!(auth.bearer(url), Some("t0ken"), "{url}");
        }
        for url in [
            "http://github.com/org/repo/releases/download/v1/tool",
            "https://github.com.evil.example/tool",
            "https://api.github.com.evil.example/tool",
            "https://evil.example/github.com/tool",
            "https://uploads.github.com/tool",
        ] {
            assert_eq!(auth.bearer(url), None, "{url}");
        }
        assert_eq!(Auth::Ambient.bearer("https://github.com/x"), None);
    }

    #[test]
    fn platforms_parse_by_their_one_spelling() {
        for platform in Platform::ALL {
            assert_eq!(
                Platform::try_from(platform.as_str().to_owned()).unwrap(),
                platform
            );
        }
        assert!(Platform::try_from("linux-amd64".to_owned()).is_err());
        assert!(Platform::try_from("LINUX_AMD64".to_owned()).is_err());
    }

    #[test]
    fn names_and_references_parse_strictly() {
        for name in ["gates", "a", "team-lib_2"] {
            assert!(name.parse::<DepName>().is_ok(), "{name}");
        }
        for name in ["", "Gates", "2x", "a/b", "a.b", "-a", &"a".repeat(65)] {
            assert!(name.parse::<DepName>().is_err(), "{name}");
        }
        assert!(split("lib/gate.star").is_none());
        let (name, path) = split("@gates//lib/gate.star").unwrap().unwrap();
        assert_eq!((name.as_str(), path), ("gates", "lib/gate.star"));
        assert_eq!(split("@gates").unwrap().unwrap().1, "");
        assert!(split("@Bad//x.star").unwrap().is_err());
    }

    #[test]
    fn a_lock_with_a_repeated_name_or_another_version_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let entry = "[[dep]]\nname = \"gates\"\ngit = \"file:///x\"\nrev = \"v1\"\ncommit = \"c\"\ntree = \"tree1:0000000000000000000000000000000000000000000000000000000000000000\"\n";
        std::fs::write(
            root.path().join(LOCK_FILE),
            format!("version = 1\n{entry}{entry}"),
        )
        .unwrap();
        assert!(matches!(
            Lock::read(root.path()).unwrap_err(),
            Error::DuplicateLock { .. }
        ));
        std::fs::write(root.path().join(LOCK_FILE), "version = 2\n").unwrap();
        assert!(matches!(
            Lock::read(root.path()).unwrap_err(),
            Error::LockVersion { found: 2, .. }
        ));
        assert_eq!(Lock::read(&root.path().join("absent")).unwrap(), None);
    }
}
