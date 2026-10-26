use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub drop: DropLaunch,
    /// The crucible checkout a loopback profile runs `just controller-local` in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub url: String,
    #[serde(default)]
    pub api: Api,
    /// An `http://` or `socks5://` proxy for every request the window makes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// Where API calls the app makes itself (drop-to-launch) go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// The profile's own URL, with no credential: a local controller's open guard.
    #[default]
    Ui,
    /// crux's resolved URL and key.
    Crux,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropLaunch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default = "default_max_cost")]
    pub max_cost: f64,
    #[serde(default = "default_max_time")]
    pub max_time: String,
}

impl Default for DropLaunch {
    fn default() -> Self {
        DropLaunch {
            profile: None,
            max_cost: default_max_cost(),
            max_time: default_max_time(),
        }
    }
}

fn default_max_cost() -> f64 {
    1.0
}

fn default_max_time() -> String {
    "30m".to_string()
}

#[derive(clap::Parser)]
struct Connect {
    #[command(flatten)]
    args: crux::config::ConnectArgs,
}

/// crux's resolution: `CONTROLLER_URL`, then its config file, then a local controller.
pub fn crux_config() -> Result<crux::config::Config, Error> {
    use clap::Parser;
    let connect = Connect::try_parse_from(["crucible-desktop"])?;
    connect
        .args
        .resolve()
        .map_err(|err| Error::from(err.to_string()))
}

impl Settings {
    pub fn path() -> Result<PathBuf, Error> {
        let dir = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(std::env::var_os("HOME").ok_or("no HOME")?).join(".config"),
        };
        Ok(dir.join("crucible-desktop").join("config.toml"))
    }

    /// The file as written, or, with no profiles in it, one profile named `default` on crux's
    /// controller.
    pub fn load() -> Result<Self, Error> {
        let path = Self::path()?;
        let mut settings: Settings = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(err) => return Err(err.into()),
        };
        if settings.profiles.is_empty() {
            let crux = crux_config()?;
            settings.profiles.insert(
                "default".to_string(),
                Profile {
                    url: crux.url,
                    api: Api::Crux,
                    proxy: None,
                },
            );
        }
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self) -> Result<(), Error> {
        self.validate()?;
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.profiles.is_empty() {
            return Err("at least one profile is required".into());
        }
        for (name, profile) in &self.profiles {
            let valid_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !valid_name {
                return Err(
                    format!("profile name {name:?}: use lowercase letters, digits and -").into(),
                );
            }
            let url = profile
                .base()
                .map_err(|err| format!("profile {name}: {err}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("profile {name}: the url must be http or https").into());
            }
            if let Some(proxy) = &profile.proxy {
                let valid = Url::parse(proxy).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "socks5") && u.host_str().is_some()
                });
                if !valid {
                    return Err(format!(
                        "profile {name}: proxy must be an http:// or socks5:// URL"
                    )
                    .into());
                }
            }
        }
        for (what, chosen) in [
            ("default_profile", &self.default_profile),
            ("drop.profile", &self.drop.profile),
        ] {
            if let Some(chosen) = chosen
                && !self.profiles.contains_key(chosen)
            {
                return Err(format!("{what} names no profile: {chosen}").into());
            }
        }
        if !(self.drop.max_cost.is_finite() && self.drop.max_cost > 0.0) {
            return Err("drop.max_cost must be a positive number of dollars".into());
        }
        if !is_duration(&self.drop.max_time) {
            return Err("drop.max_time must look like 30m, 2h or 1d".into());
        }
        if self.repo.as_ref().is_some_and(|repo| !repo.is_absolute()) {
            return Err("repo must be an absolute path".into());
        }
        Ok(())
    }

    /// The checkout a local controller starts from: `repo`, else the checkout the app was built
    /// in, when it is still there.
    pub fn controller_repo(&self) -> Result<PathBuf, Error> {
        let repo = self
            .repo
            .clone()
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")));
        if repo.join("justfile").is_file() {
            Ok(repo)
        } else {
            Err(format!(
                "{} is not a crucible checkout; set repo in Settings to run a local controller",
                repo.display()
            )
            .into())
        }
    }

    /// The profile a bare deep link or the first window opens.
    pub fn default_name(&self) -> Option<&str> {
        self.default_profile
            .as_deref()
            .or_else(|| self.profiles.keys().next().map(String::as_str))
    }

    /// The profile a dropped pack launches on: the configured one, else the first local one,
    /// else the default.
    pub fn drop_name(&self) -> Option<&str> {
        self.drop
            .profile
            .as_deref()
            .or_else(|| {
                self.profiles
                    .iter()
                    .find(|(_, p)| p.local_port().is_some())
                    .map(|(name, _)| name.as_str())
            })
            .or_else(|| self.default_name())
    }

    /// The profile whose UI lives at `origin`.
    pub fn by_origin(&self, origin: &str) -> Option<&str> {
        self.profiles
            .iter()
            .find(|(_, p)| p.origin().is_ok_and(|o| o == origin))
            .map(|(name, _)| name.as_str())
    }
}

impl Profile {
    /// The UI's base URL, with the trailing slash `Url::join` needs.
    pub fn base(&self) -> Result<Url, url::ParseError> {
        Url::parse(&format!("{}/", self.url.trim().trim_end_matches('/')))
    }

    pub fn origin(&self) -> Result<String, url::ParseError> {
        Ok(self.base()?.origin().ascii_serialization())
    }

    /// The port of a controller on this machine, which the app starts itself.
    pub fn local_port(&self) -> Option<u16> {
        let url = self.base().ok()?;
        let loopback = match url.host()? {
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
            url::Host::Domain(name) => name == "localhost",
        };
        loopback.then(|| url.port_or_known_default()).flatten()
    }

    /// What drop-to-launch talks to.
    pub fn api_config(&self) -> Result<crux::config::Config, Error> {
        match self.api {
            Api::Ui => Ok(crux::config::Config {
                url: self.url.trim().trim_end_matches('/').to_string(),
                auth: crux::config::Auth::from_token(None),
            }),
            Api::Crux => crux_config(),
        }
    }
}

fn is_duration(text: &str) -> bool {
    let text = text.trim();
    let Some(unit) = text.chars().last() else {
        return false;
    };
    let digits = &text[..text.len() - unit.len_utf8()];
    matches!(unit, 's' | 'm' | 'h' | 'd')
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use crate::settings::*;

    fn profile(url: &str) -> Profile {
        Profile {
            url: url.to_string(),
            api: Api::Ui,
            proxy: None,
        }
    }

    #[test]
    fn a_proxy_parses_and_validates() {
        let parsed: Settings = toml::from_str(
            r#"
[profiles.jump]
url = "https://crucible.example.com"
proxy = "socks5://127.0.0.1:1080"
"#,
        )
        .unwrap();
        parsed.validate().unwrap();
        let again: Settings = toml::from_str(&toml::to_string_pretty(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
        assert!(
            !toml::to_string_pretty(&settings(&[("a", "https://x")]))
                .unwrap()
                .contains("proxy")
        );
    }

    #[test]
    fn a_bad_proxy_is_refused() {
        let with = |proxy: &str| {
            let mut s = settings(&[("a", "https://x")]);
            if let Some(p) = s.profiles.get_mut("a") {
                p.proxy = Some(proxy.into());
            }
            s.validate()
        };
        assert!(with("ftp://h:1").is_err());
        assert!(with("not a url").is_err());
        assert!(with("http://").is_err());
        assert!(with("http://proxy:3128").is_ok());
        assert!(with("socks5://127.0.0.1:1080").is_ok());
    }

    fn settings(profiles: &[(&str, &str)]) -> Settings {
        Settings {
            profiles: profiles
                .iter()
                .map(|(name, url)| (name.to_string(), profile(url)))
                .collect(),
            ..Settings::default()
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let text = r#"
default_profile = "mpp"

[profiles.local]
url = "http://127.0.0.1:8870"

[profiles.mpp]
url = "https://crucible.example.com/"
api = "crux"

[drop]
profile = "local"
max_cost = 2.5
max_time = "1h"
"#;
        let parsed: Settings = toml::from_str(text).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.profiles["mpp"].api, Api::Crux);
        assert_eq!(parsed.drop.max_cost, 2.5);
        let again: Settings = toml::from_str(&toml::to_string_pretty(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
    }

    #[test]
    fn drop_defaults_apply_when_the_table_is_absent() {
        let parsed: Settings =
            toml::from_str("[profiles.a]\nurl = \"http://localhost:1\"\n").unwrap();
        assert_eq!(parsed.drop, DropLaunch::default());
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<Settings>("url = \"https://x\"\n").is_err());
        assert!(
            toml::from_str::<Settings>("[profiles.a]\nurl = \"https://x\"\ntoken = \"t\"\n")
                .is_err()
        );
    }

    #[test]
    fn validation_refuses_bad_profiles_and_dangling_names() {
        assert!(Settings::default().validate().is_err());
        assert!(settings(&[("Bad Name", "https://x")]).validate().is_err());
        assert!(settings(&[("a", "not a url")]).validate().is_err());
        assert!(settings(&[("a", "ftp://x")]).validate().is_err());
        let mut s = settings(&[("a", "https://x")]);
        s.default_profile = Some("b".to_string());
        assert!(s.validate().is_err());
        let mut s = settings(&[("a", "https://x")]);
        s.drop.profile = Some("b".to_string());
        assert!(s.validate().is_err());
        let mut s = settings(&[("a", "https://x")]);
        s.drop.max_cost = 0.0;
        assert!(s.validate().is_err());
        let mut s = settings(&[("a", "https://x")]);
        s.drop.max_time = "soon".to_string();
        assert!(s.validate().is_err());
        settings(&[("a-1", "https://x")]).validate().unwrap();
    }

    #[test]
    fn repo_is_absolute_and_must_hold_a_justfile() {
        let mut s = settings(&[("a", "https://x")]);
        s.repo = Some(PathBuf::from("relative/crucible"));
        assert!(s.validate().is_err());

        let dir =
            std::env::temp_dir().join(format!("crucible-desktop-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        s.repo = Some(dir.clone());
        s.validate().unwrap();
        assert!(s.controller_repo().is_err());
        std::fs::write(dir.join("justfile"), "").unwrap();
        assert_eq!(s.controller_repo().unwrap(), dir);
        let again: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(again.repo, s.repo);
        std::fs::remove_dir_all(&dir).unwrap();

        s.repo = None;
        assert!(
            s.controller_repo().is_ok(),
            "the build checkout is the fallback"
        );
    }

    #[test]
    fn durations() {
        for ok in ["30m", "2h", "1d", "45s", " 10m "] {
            assert!(is_duration(ok), "{ok}");
        }
        for bad in ["", "m", "30", "1.5h", "30x", "-1m"] {
            assert!(!is_duration(bad), "{bad}");
        }
    }

    #[test]
    fn local_port_is_loopback_only() {
        assert_eq!(profile("http://127.0.0.1:8870").local_port(), Some(8870));
        assert_eq!(profile("http://localhost:9000/").local_port(), Some(9000));
        assert_eq!(profile("http://[::1]:7000").local_port(), Some(7000));
        assert_eq!(profile("https://crucible.example.com").local_port(), None);
    }

    #[test]
    fn profile_selection() {
        let mut s = settings(&[
            ("mpp", "https://crucible.example.com"),
            ("local", "http://127.0.0.1:8870"),
        ]);
        assert_eq!(s.default_name(), Some("local"));
        assert_eq!(s.drop_name(), Some("local"));
        s.default_profile = Some("mpp".to_string());
        assert_eq!(s.default_name(), Some("mpp"));
        assert_eq!(s.by_origin("https://crucible.example.com"), Some("mpp"));
        assert_eq!(s.by_origin("https://elsewhere.example.com"), None);
        s.profiles.remove("local");
        assert_eq!(s.drop_name(), Some("mpp"));
    }
}
