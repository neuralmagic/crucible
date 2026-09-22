//! Where the controller is, and the bearer every request carries.
//!
//! A `crk_` key minted in the SPA authenticates as the person who minted it. The controller's
//! static `CONTROLLER_API_TOKEN` authenticates as `anonymous`, or as the identity the deployment
//! pins to it. Nothing here names an identity: the controller resolves the key, and a caller who
//! wants to act as somebody mints a key as that somebody.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// What a key minted in the SPA starts with; the controller's `api_key::PREFIX`.
const API_KEY_PREFIX: &str = "crk_";

/// The credential a request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// A `crk_` key its owner minted in the SPA. The controller resolves it to that person, so
    /// mutations are booked to them, not to `anonymous`.
    ApiKey { token: String },
    /// The static bearer with no identity of its own: reads work, every admin/operator route 403s.
    Bearer { token: String },
    /// No credential at all (a controller running with `CONTROLLER_API_TOKEN` unset).
    None,
}

impl Auth {
    /// A blank token is no token; a `crk_` one is a minted key.
    pub fn from_token(token: Option<String>) -> Self {
        match token.filter(|t| !t.trim().is_empty()) {
            Some(token) if token.starts_with(API_KEY_PREFIX) => Auth::ApiKey { token },
            Some(token) => Auth::Bearer { token },
            None => Auth::None,
        }
    }

    /// The headers this credential contributes.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        match self {
            Auth::ApiKey { token } | Auth::Bearer { token } => {
                vec![("authorization", format!("Bearer {token}"))]
            }
            Auth::None => Vec::new(),
        }
    }

    /// A one-word label for `crux whoami`.
    pub fn mode(&self) -> &'static str {
        match self {
            Auth::ApiKey { .. } => "api key",
            Auth::Bearer { .. } => "bearer (anonymous)",
            Auth::None => "none",
        }
    }
}

/// Where to talk, and what to say we are.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL, no trailing slash.
    pub url: String,
    pub auth: Auth,
}

/// The flags every subcommand shares. Each mirrors an env var and a config-file key.
#[derive(Debug, clap::Args, Default)]
pub struct ConnectArgs {
    /// Config file. Defaults to `config.toml` under `$XDG_CONFIG_HOME/crux` (else
    /// `~/.config/crux`), which may be absent; a path given here must exist. A flag or env var
    /// wins over the file.
    #[arg(long, env = "CONTROLLER_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    /// Controller base URL: the API route.
    #[arg(long, env = "CONTROLLER_URL", global = true)]
    pub url: Option<String>,

    /// The bearer: a minted `crk_` key, which authenticates as the person who minted it, or the
    /// controller's static token, which authenticates as nobody.
    #[arg(
        long,
        env = "CONTROLLER_API_TOKEN",
        global = true,
        hide_env_values = true
    )]
    pub api_token: Option<String>,
}

/// `config.toml`: the settings that describe a deployment rather than one invocation. Every key
/// is optional. Unknown keys are refused, so a typo is an error and not a silently ignored line.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub url: Option<String>,
    pub api_token: Option<String>,
}

impl FileConfig {
    /// `config.toml` under [`config_dir`].
    fn default_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    /// Read `path`. The default location is allowed to be absent; an explicitly named file is not,
    /// because a typo in `--config` must not quietly become "no config".
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => (Self::default_path()?, false),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

impl ConnectArgs {
    /// Flags and env vars over the file.
    pub fn resolve(&self) -> Result<Config> {
        let file = FileConfig::load(self.config.as_deref())?;
        Ok(self.resolve_over(file))
    }

    fn resolve_over(&self, file: FileConfig) -> Config {
        Config {
            url: normalize_url(self.url.clone().or(file.url).unwrap_or_default()),
            auth: Auth::from_token(self.api_token.clone().or(file.api_token)),
        }
    }
}

/// Strip the trailing slash so `format!("{url}{path}")` never produces a double slash — some
/// ingress controllers 404 on `//api/issues`.
fn normalize_url(url: String) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// The config file's home: `$XDG_CONFIG_HOME/crux`, or `~/.config/crux` when `XDG_CONFIG_HOME`
/// is unset or empty.
fn config_dir() -> Result<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn config_dir_from(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("crux"));
    }
    let home = home.context("no HOME in the environment")?;
    Ok(PathBuf::from(home).join(".config").join("crux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> ConnectArgs {
        #[derive(clap::Parser)]
        struct Only {
            #[command(flatten)]
            connect: ConnectArgs,
        }
        let mut full = vec!["crux"];
        full.extend_from_slice(argv);
        <Only as clap::Parser>::try_parse_from(full)
            .expect("parses")
            .connect
    }

    #[test]
    fn the_config_dir_honors_xdg_config_home_and_falls_back_to_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            config_dir_from(xdg.map(Into::into), home.map(Into::into))
        };
        assert_eq!(
            dir(Some("/xdg"), Some("/home/u")).unwrap(),
            PathBuf::from("/xdg/crux")
        );
        assert_eq!(
            dir(None, Some("/home/u")).unwrap(),
            PathBuf::from("/home/u/.config/crux")
        );
        assert_eq!(
            dir(Some(""), Some("/home/u")).unwrap(),
            PathBuf::from("/home/u/.config/crux"),
            "an empty XDG_CONFIG_HOME is unset"
        );
        assert!(dir(None, None).is_err());
    }

    #[test]
    fn a_minted_key_is_named_as_one_and_not_as_anonymous() {
        let auth = Auth::from_token(Some("crk_id_secret".into()));
        assert_eq!(
            auth,
            Auth::ApiKey {
                token: "crk_id_secret".into()
            }
        );
        assert_eq!(
            auth.headers(),
            vec![("authorization", "Bearer crk_id_secret".to_string())]
        );
        assert_eq!(auth.mode(), "api key");
    }

    /// A bearer with no identity is the accident that costs an hour: reads work, every mutation
    /// 403s. Name it plainly instead of letting it look like a working session.
    #[test]
    fn the_static_bearer_is_anonymous() {
        let auth = Auth::from_token(Some("dev-local".into()));
        assert_eq!(auth.headers().len(), 1);
        assert_eq!(auth.mode(), "bearer (anonymous)");
    }

    #[test]
    fn no_credential_sends_no_headers() {
        assert_eq!(Auth::from_token(None), Auth::None);
        assert_eq!(Auth::from_token(Some("   ".into())), Auth::None);
        assert!(Auth::None.headers().is_empty());
        assert_eq!(Auth::None.mode(), "none");
    }

    #[test]
    fn urls_lose_their_trailing_slash() {
        assert_eq!(
            normalize_url("https://c.example.com/".into()),
            "https://c.example.com"
        );
        assert_eq!(
            normalize_url("  http://localhost:8870  ".into()),
            "http://localhost:8870"
        );
    }

    #[test]
    fn a_file_parses_its_keys_and_refuses_an_unknown_one() {
        let file: FileConfig = toml::from_str(
            r#"
            url = "https://c.example.com/"
            api_token = "crk_a_b"
            "#,
        )
        .expect("every key parses");
        assert_eq!(file.url.as_deref(), Some("https://c.example.com/"));
        assert_eq!(file.api_token.as_deref(), Some("crk_a_b"));

        let err = toml::from_str::<FileConfig>("controller_url = \"x\"\n")
            .expect_err("a misspelled key is an error, not a silently ignored line");
        assert!(err.to_string().contains("controller_url"), "{err}");
        let err = toml::from_str::<FileConfig>("kube_context = \"prod\"\n")
            .expect_err("a retired key is an error, so a stale file says so");
        assert!(err.to_string().contains("kube_context"), "{err}");
    }

    #[test]
    fn the_file_fills_in_what_the_command_line_did_not_say() {
        let file: FileConfig = toml::from_str(
            r#"
            url = "https://file.example.com/"
            api_token = "crk_file"
            "#,
        )
        .expect("parses");
        let cfg = args(&[]).resolve_over(file);
        assert_eq!(cfg.url, "https://file.example.com");
        assert_eq!(
            cfg.auth,
            Auth::ApiKey {
                token: "crk_file".into()
            }
        );
    }

    /// A shell that exports a token is authenticated as that token and nothing else, whatever the
    /// file says.
    #[test]
    fn the_command_line_wins_over_the_file() {
        let file: FileConfig = toml::from_str(
            r#"
            url = "https://file.example.com"
            api_token = "crk_file"
            "#,
        )
        .expect("parses");
        let cfg = args(&[
            "--url",
            "https://flag.example.com/",
            "--api-token",
            "crk_flag",
        ])
        .resolve_over(file);
        assert_eq!(cfg.url, "https://flag.example.com");
        assert_eq!(
            cfg.auth,
            Auth::ApiKey {
                token: "crk_flag".into()
            }
        );
    }

    #[test]
    fn with_nothing_at_all_the_url_is_empty_and_the_auth_is_none() {
        let cfg = args(&[]).resolve_over(FileConfig::default());
        assert_eq!(cfg.url, "");
        assert_eq!(cfg.auth, Auth::None);
    }

    #[test]
    fn a_named_config_file_must_exist_but_the_default_one_need_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let err = FileConfig::load(Some(&missing)).expect_err("an explicit path must exist");
        assert!(err.to_string().contains("nope.toml"), "{err}");

        let present = dir.path().join("config.toml");
        std::fs::write(&present, "url = \"https://c.example.com\"\n").expect("write");
        let file = FileConfig::load(Some(&present)).expect("reads");
        assert_eq!(file.url.as_deref(), Some("https://c.example.com"));
    }
}
