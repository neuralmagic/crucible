//! The retired Tauri app's settings, read as this app's profiles.

use crate::profile::{Auth, Profile};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use url::Url;

/// What the retired Tauri desktop app's `config.toml` held, as far as this app uses it.
#[derive(Debug, Default, PartialEq)]
pub struct Tauri {
    pub profiles: BTreeMap<String, Profile>,
    pub default: Option<String>,
    pub drop: Option<(f64, String)>,
}

/// The Tauri app's settings in `text`, as profiles of this app. A profile that called crux
/// (`api = "crux"`) gets `crux_url` and crux's key; the others keep their URL. Its `resolve` named
/// a domain to reach elsewhere, here the profile's own host when it is in that domain.
pub fn from_tauri(text: &str, crux_url: &str) -> Tauri {
    #[derive(Deserialize)]
    struct File {
        default_profile: Option<String>,
        #[serde(default)]
        profiles: BTreeMap<String, Old>,
        drop: Option<Drop>,
    }
    #[derive(Deserialize)]
    struct Old {
        url: String,
        #[serde(default)]
        api: Option<String>,
        proxy: Option<String>,
        #[serde(default)]
        resolve: BTreeMap<String, String>,
    }
    #[derive(Deserialize)]
    struct Drop {
        max_cost: Option<f64>,
        max_time: Option<String>,
    }
    let Ok(file) = toml::from_str::<File>(text) else {
        return Tauri::default();
    };
    let profiles: BTreeMap<String, Profile> = file
        .profiles
        .into_iter()
        .filter_map(|(name, old)| {
            let crux = old.api.as_deref() == Some("crux");
            let url = if crux {
                crux_url.to_string()
            } else {
                old.url.trim_end_matches('/').to_string()
            };
            let host = Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            let resolve = old
                .resolve
                .into_iter()
                .filter_map(|(domain, to)| {
                    let inside = host == domain || host.ends_with(&format!(".{domain}"));
                    inside.then(|| (host.clone(), to))
                })
                .collect();
            let profile = Profile {
                url,
                auth: if crux { Auth::Crux } else { Auth::Own },
                proxy: old.proxy,
                resolve,
                tunnel: None,
                shell: None,
            };
            profile.check(&name).ok().map(|()| (name, profile))
        })
        .collect();
    Tauri {
        default: file
            .default_profile
            .filter(|name| profiles.contains_key(name)),
        drop: file.drop.and_then(|d| Some((d.max_cost?, d.max_time?))),
        profiles,
    }
}

/// The retired Tauri app's settings file, under `config_dir`.
pub fn tauri_config(config_dir: &Path) -> PathBuf {
    config_dir.join("crucible-desktop").join("config.toml")
}
