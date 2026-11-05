//! Names the controller, the loop, and the per-run `crucible-git` server share.

/// The URL sandboxes reach `crucible-git` at unless `[git] url` names another, and the audience of
/// the tokens it accepts.
pub const URL: &str = "http://host.openshell.internal:8853/mcp";

/// The port in [`URL`], which `crucible-git` listens on in the loop pod.
pub const PORT: u16 = 8853;

/// Where the controller mounts the run's GitHub credential in the `crucible-git` container,
/// and nowhere else in the pod.
pub const TOKEN_DIR: &str = "/var/run/crucible-git-token";

/// The token's file name in [`TOKEN_DIR`].
pub const TOKEN_FILE_NAME: &str = "token";

/// The directory the loop container and `crucible-git` share: the loop writes [`BOUNDS_FILE`]
/// there at setup, and `crucible-git` reads it.
pub const SHARED_DIR: &str = "/run/crucible-git";

/// Where `crucible-git` writes the run CA and the bundle tasks trust, readable by the loop.
pub const CA_DIR: &str = "/run/crucible-git-ca";

/// The run CA's certificate in [`CA_DIR`], for runtimes that add one CA to their own roots.
pub const CA_FILE_NAME: &str = "ca.pem";

/// The system's roots plus the run CA in [`CA_DIR`], for runtimes that take one bundle.
pub const BUNDLE_FILE_NAME: &str = "bundle.pem";

/// GitHub's hostnames, which the run pod maps to loopback, where `crucible-git` serves them over
/// TLS at [`TLS_BIND`].
pub const GITHUB_HOSTS: [&str; 4] = [
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "codeload.github.com",
];

/// Where `crucible-git` serves [`GITHUB_HOSTS`] in the run pod.
pub const TLS_BIND: &str = "127.0.0.1:443";

/// The environment a task needs to reach GitHub through `crucible-git` with stock tools: trust in
/// the run CA for the common runtimes, and the placeholder token `gh` sends.
pub fn task_env() -> Vec<(&'static str, String)> {
    let bundle = format!("{CA_DIR}/{BUNDLE_FILE_NAME}");
    vec![
        ("SSL_CERT_FILE", bundle.clone()),
        ("GIT_SSL_CAINFO", bundle.clone()),
        ("CURL_CA_BUNDLE", bundle.clone()),
        ("REQUESTS_CA_BUNDLE", bundle),
        ("NODE_EXTRA_CA_CERTS", format!("{CA_DIR}/{CA_FILE_NAME}")),
        ("GH_TOKEN", PLACEHOLDER_TOKEN.to_string()),
    ]
}

/// The token tasks hold in `GH_TOKEN`: `crucible-git` replaces it with the run's credential, so it
/// is worthless outside the pod.
pub const PLACEHOLDER_TOKEN: &str = "crucible-git";

/// The run's resolved output bounds and bound parameters, as [`Bounds`].
pub const BOUNDS_FILE: &str = "/run/crucible-git/bounds.json";

/// What the loop hands `crucible-git` about the run: the `[outputs]` table resolved for this run
/// and the parameter values an open target binds to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    pub outputs: crate::outputs::ResolvedOutputs,
    pub params: std::collections::BTreeMap<String, String>,
}
