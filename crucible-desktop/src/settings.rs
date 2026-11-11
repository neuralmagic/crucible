//! The app's own settings, in `~/.config/crucible-playbooks/config.toml`.
//!
//! The sans is the theme's Inter. The mono slot and the log fonts are the theme's IBM Plex Mono (OFL)
//! or a font read from this machine's font store at startup; no proprietary font is ever bundled.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rune::tok::Family;
use serde::{Deserialize, Serialize};

use crate::editor::Editor;
use crate::profile::Profile;
use crate::workspace::Workspace;

/// The colours the app paints with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    Graphite,
    Midnight,
    Ember,
    Moss,
}

impl Theme {
    pub const ALL: [Theme; 4] = [Theme::Graphite, Theme::Midnight, Theme::Ember, Theme::Moss];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Graphite => "Graphite",
            Theme::Midnight => "Midnight",
            Theme::Ember => "Ember",
            Theme::Moss => "Moss",
        }
    }
}

/// Light or dark, or whichever the system is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    pub fn dark(self, system_dark: bool) -> bool {
        match self {
            Appearance::System => system_dark,
            Appearance::Light => false,
            Appearance::Dark => true,
        }
    }
}

/// The zooms the app steps between, smallest first.
pub const ZOOMS: [f32; 7] = [0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

/// The zoom one step in `direction` (+1 in, -1 out) from `zoom`, staying on [`ZOOMS`].
pub fn zoom_step(zoom: f32, direction: i32) -> f32 {
    let next = match ZOOMS.iter().position(|z| (z - zoom).abs() < 0.01) {
        Some(at) => at as i32 + direction,
        None => {
            let below = ZOOMS.iter().filter(|z| **z < zoom).count() as i32;
            if direction > 0 { below } else { below - 1 }
        }
    }
    .clamp(0, ZOOMS.len() as i32 - 1);
    ZOOMS[next as usize]
}

/// How large the app sets its text, from the theme's own sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextSize {
    Small,
    #[default]
    Default,
    Large,
    Larger,
}

impl TextSize {
    pub const ALL: [TextSize; 4] = [
        TextSize::Small,
        TextSize::Default,
        TextSize::Large,
        TextSize::Larger,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TextSize::Small => "Small",
            TextSize::Default => "Default",
            TextSize::Large => "Large",
            TextSize::Larger => "Larger",
        }
    }

    /// What every text size and line height is multiplied by.
    pub fn factor(self) -> f32 {
        match self {
            TextSize::Small => 0.9,
            TextSize::Default => 1.0,
            TextSize::Large => 1.12,
            TextSize::Larger => 1.25,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogFont {
    /// The platform's monospaced UI font (SF Mono on macOS).
    System,
    /// IBM Plex Mono, bundled with the app.
    #[default]
    Plex,
    /// TX-02, read from the local font store when it is installed.
    Tx02,
}

impl LogFont {
    pub const ALL: [LogFont; 3] = [LogFont::System, LogFont::Plex, LogFont::Tx02];

    pub fn label(self) -> &'static str {
        match self {
            LogFont::System => "System mono",
            LogFont::Plex => "IBM Plex Mono",
            LogFont::Tx02 => "TX-02",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            LogFont::System => "The platform's monospaced UI font, SF Mono on macOS.",
            LogFont::Plex => "Bundled with the app (OFL), so it looks the same everywhere.",
            LogFont::Tx02 => "Read from this machine's font store; never bundled.",
        }
    }

    pub fn family(self) -> Family {
        match self {
            LogFont::System => Family::SystemMono,
            LogFont::Plex => Family::Named("IBM Plex Mono"),
            LogFont::Tx02 => Family::Named("TX-02"),
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub log_font: LogFont,
    /// The TX-02 face found on this machine, when there is one.
    pub tx02: Option<PathBuf>,
    /// The run workspace as the user last arranged it; `None` lays each run out by its graph.
    pub layout: Option<Workspace>,
    pub theme: Theme,
    pub appearance: Appearance,
    pub text_size: TextSize,
    /// Display points per layout point: below 1 fits more on screen.
    pub zoom: f32,
    /// Draw charts through the CRT pass.
    pub crt: bool,
    /// Keep profile keys in the system credential store, not in files.
    pub keychain: bool,
    /// Ask GitHub once a day whether a newer release exists.
    pub updates: bool,
    /// Announce the signed-in user's runs as they finish.
    pub notify: bool,
    /// Announce decision requests the signed-in user may answer as they open.
    pub ask: bool,
    /// The editor pack files open in; `None` finds one.
    pub editor: Option<Editor>,
    /// Open files with `editor_command` instead of an editor app.
    pub editor_custom: bool,
    /// A shell command with `{file}` and `{line}`, for editors that are not apps.
    pub editor_command: String,
    /// Keys bound by hand, by command id; a command left out keeps its defaults.
    pub keys: BTreeMap<String, Vec<String>>,
    /// Check the shown pack file with crucible-lsp.
    pub checks: bool,
    /// The profiles as written, each read on its own so one bad profile spoils only itself.
    profiles: BTreeMap<String, toml::Value>,
    /// The profile in use, by name.
    pub profile: Option<String>,
    /// The team each profile acts as (`team:<slug>`), by profile name.
    act_as: BTreeMap<String, String>,
    /// The cost and time ceilings a pack folder dropped on the app is test-fired under.
    pub drop_max_cost: f64,
    pub drop_max_time: String,
    /// Why the file on disk did not read. While set, nothing is saved over it.
    pub unreadable: Option<String>,
    path: PathBuf,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    log_font: Option<LogFont>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx02_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    layout: Option<Workspace>,
    theme: Theme,
    appearance: Appearance,
    text_size: TextSize,
    zoom: Option<f32>,
    crt: Option<bool>,
    keychain: Option<bool>,
    check_updates: Option<bool>,
    notify: Option<bool>,
    notify_decisions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    editor_custom: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor_command: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    keys: BTreeMap<String, Vec<String>>,
    checks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    drop_max_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drop_max_time: Option<String>,
    /// The team each profile acts as, by profile name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    act_as: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    profiles: BTreeMap<String, toml::Value>,
}

impl Settings {
    pub fn load() -> Self {
        Settings::load_from(config_path())
    }

    fn load_from(path: PathBuf) -> Self {
        let (mut config, unreadable) = match read(&path) {
            Ok(config) => (config, None),
            Err(why) => (Config::default(), Some(why)),
        };
        if unreadable.is_none()
            && config.profiles.is_empty()
            && let Some(dir) = path.parent().and_then(Path::parent)
            && let Ok(text) = fs::read_to_string(crate::profile::tauri_config(dir))
        {
            let crux_url = crux::config::ConnectArgs::from_env()
                .resolve()
                .map(|c| c.url)
                .unwrap_or_default();
            adopt_tauri(&mut config, crate::profile::from_tauri(&text, &crux_url));
        }
        let tx02 = config
            .tx02_path
            .filter(|path| path.is_file())
            .or_else(find_tx02);
        let log_font = match config.log_font {
            Some(LogFont::Tx02) if tx02.is_none() => LogFont::Plex,
            Some(font) => font,
            None if tx02.is_some() => LogFont::Tx02,
            None => LogFont::Plex,
        };
        Settings {
            log_font,
            tx02,
            layout: config.layout.and_then(Workspace::repaired),
            theme: config.theme,
            appearance: config.appearance,
            text_size: config.text_size,
            zoom: config
                .zoom
                .filter(|z| ZOOMS.iter().any(|known| (known - z).abs() < 0.01))
                .unwrap_or(1.0),
            crt: config.crt.unwrap_or(true),
            keychain: config.keychain.unwrap_or(false),
            updates: config.check_updates.unwrap_or(true),
            notify: config.notify.unwrap_or(true),
            ask: config.notify_decisions.unwrap_or(true),
            editor: config.editor,
            editor_custom: config.editor_custom.unwrap_or(false),
            editor_command: config.editor_command.unwrap_or_default(),
            keys: config.keys,
            checks: config.checks.unwrap_or(true),
            profiles: config.profiles,
            profile: config.profile,
            act_as: config.act_as,
            drop_max_cost: config
                .drop_max_cost
                .filter(|c| c.is_finite() && *c > 0.0)
                .unwrap_or(1.0),
            drop_max_time: config
                .drop_max_time
                .filter(|t| crate::form::minutes(t).is_some())
                .unwrap_or_else(|| "30m".to_string()),
            unreadable,
            path,
        }
    }

    /// Each configured profile, or why it cannot be read.
    pub fn profiles(&self) -> BTreeMap<String, Result<Profile, String>> {
        self.profiles
            .iter()
            .map(|(name, raw)| {
                let profile = raw
                    .clone()
                    .try_into::<Profile>()
                    .map_err(|e| format!("profile {name}: {}", e.message()))
                    .and_then(|p| p.check(name).map(|()| p));
                (name.clone(), profile)
            })
            .collect()
    }

    /// Add `profile` as `name`, or replace the one by that name.
    pub fn set_profile_def(&mut self, name: &str, profile: &Profile) -> Result<(), String> {
        profile.check(name)?;
        let raw = toml::Value::try_from(profile).map_err(|e| format!("{e}"))?;
        self.profiles.insert(name.to_string(), raw);
        self.save();
        Ok(())
    }

    pub fn remove_profile(&mut self, name: &str) {
        if self.profiles.remove(name).is_some() {
            if self.profile.as_deref() == Some(name) {
                self.profile = None;
            }
            self.save();
        }
    }

    /// Set the drop ceilings, when both read.
    pub fn set_drop(&mut self, max_cost: &str, max_time: &str) -> Result<(), String> {
        let cost: f64 = max_cost
            .trim()
            .trim_start_matches('$')
            .parse()
            .ok()
            .filter(|c: &f64| c.is_finite() && *c > 0.0)
            .ok_or("The cost ceiling is a positive number of dollars")?;
        if crate::form::minutes(max_time.trim()).is_none() {
            return Err("The time ceiling looks like 30m or 2h".into());
        }
        self.drop_max_cost = cost;
        self.drop_max_time = max_time.trim().to_string();
        self.save();
        Ok(())
    }

    /// The team profile `name` acts as.
    pub fn act_as(&self, name: &str) -> Option<String> {
        self.act_as.get(name).cloned()
    }

    pub fn set_act_as(&mut self, name: &str, team: Option<String>) {
        let changed = match team {
            Some(team) => self.act_as.insert(name.to_string(), team.clone()) != Some(team),
            None => self.act_as.remove(name).is_some(),
        };
        if changed {
            self.save();
        }
    }

    pub fn use_profile(&mut self, name: &str) {
        if self.profile.as_deref() != Some(name) {
            self.profile = Some(name.to_string());
            self.save();
        }
    }

    /// Where profiles keep their own keys.
    pub fn keys(&self) -> crate::keys::Store {
        self.keys_in(self.keychain)
    }

    /// The system credential store when `keychain`, else the key files.
    pub fn keys_in(&self, keychain: bool) -> crate::keys::Store {
        #[cfg(not(target_arch = "wasm32"))]
        if keychain {
            return crate::keys::Store::Keychain;
        }
        #[cfg(target_arch = "wasm32")]
        let _ = keychain;
        crate::keys::Store::Dir(self.keys_dir())
    }

    fn keys_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| PathBuf::from("profiles"), |dir| dir.join("profiles"))
    }

    pub fn config_path(&self) -> &Path {
        &self.path
    }

    pub fn set_log_font(&mut self, font: LogFont) {
        if font == LogFont::Tx02 && self.tx02.is_none() {
            return;
        }
        self.log_font = font;
        self.save();
    }

    pub fn set_layout(&mut self, layout: Option<Workspace>) {
        if self.layout != layout {
            self.layout = layout;
            self.save();
        }
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.save();
    }

    pub fn set_crt(&mut self, on: bool) {
        self.crt = on;
        self.save();
    }

    pub fn set_keychain(&mut self, on: bool) {
        self.keychain = on;
        self.save();
    }

    pub fn set_checks(&mut self, on: bool) {
        self.checks = on;
        self.save();
    }

    pub fn set_updates(&mut self, on: bool) {
        self.updates = on;
        self.save();
    }

    pub fn set_notify(&mut self, on: bool) {
        self.notify = on;
        self.save();
    }

    pub fn set_ask(&mut self, on: bool) {
        self.ask = on;
        self.save();
    }

    pub fn set_editor_command(&mut self, custom: bool, command: &str) {
        self.editor_custom = custom;
        self.editor_command = command.to_string();
        self.save();
    }

    pub fn set_editor(&mut self, editor: Option<Editor>) {
        self.editor_custom = false;
        self.editor = editor;
        self.save();
    }

    /// Bind `command` to `keys`, or back to its defaults with `None`.
    pub fn set_keys(&mut self, command: &str, keys: Option<Vec<String>>) {
        match keys {
            Some(keys) => self.keys.insert(command.to_string(), keys),
            None => self.keys.remove(command),
        };
        self.save();
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom;
        self.save();
    }

    pub fn set_text_size(&mut self, size: TextSize) {
        self.text_size = size;
        self.save();
    }

    pub fn set_appearance(&mut self, appearance: Appearance) {
        self.appearance = appearance;
        self.save();
    }

    fn save(&self) {
        if self.unreadable.is_some() {
            return;
        }
        let config = Config {
            log_font: Some(self.log_font),
            tx02_path: None,
            layout: self.layout.clone(),
            theme: self.theme,
            appearance: self.appearance,
            text_size: self.text_size,
            zoom: Some(self.zoom),
            crt: Some(self.crt),
            keychain: Some(self.keychain),
            check_updates: Some(self.updates),
            notify: Some(self.notify),
            notify_decisions: Some(self.ask),
            editor: self.editor,
            editor_custom: Some(self.editor_custom),
            editor_command: (!self.editor_command.is_empty()).then(|| self.editor_command.clone()),
            keys: self.keys.clone(),
            checks: Some(self.checks),
            profile: self.profile.clone(),
            act_as: self.act_as.clone(),
            profiles: self.profiles.clone(),
            drop_max_cost: Some(self.drop_max_cost),
            drop_max_time: Some(self.drop_max_time.clone()),
        };
        let Ok(text) = toml::to_string(&config) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let partial = file.with_extension("toml.partial");
        if fs::write(&partial, text).is_ok() && fs::rename(&partial, &file).is_err() {
            let _ = fs::remove_file(&partial);
        }
    }
}

/// The config at `path`, the default when there is no file, or why the file does not read.
fn read(path: &Path) -> Result<Config, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e.to_string()),
    };
    toml::from_str(&text).map_err(|e| {
        let line = e
            .span()
            .map_or(1, |span| text[..span.start].matches('\n').count() + 1);
        format!("line {line}: {}", e.message())
    })
}

/// The fonts registered with Rune: the theme's faces, plus TX-02 read from this machine when
/// present, and never shipped.
pub fn fonts(settings: &Settings) -> &'static [&'static [u8]] {
    let mut fonts: Vec<&'static [u8]> = rune_theme_opencode::FONTS.to_vec();
    if let Some(path) = &settings.tx02
        && let Ok(bytes) = fs::read(path)
    {
        fonts.push(Box::leak(bytes.into_boxed_slice()));
    }
    Box::leak(fonts.into_boxed_slice())
}

pub fn families() -> (&'static str, &'static str) {
    rune_theme_opencode::FAMILIES
}

/// Take the retired Tauri app's profiles, the one it opened by default and its drop ceilings,
/// where `config` sets none of its own.
fn adopt_tauri(config: &mut Config, old: crate::profile::Tauri) {
    config.profiles = old
        .profiles
        .iter()
        .filter_map(|(name, profile)| Some((name.clone(), toml::Value::try_from(profile).ok()?)))
        .collect();
    if config.profile.is_none() {
        config.profile = old.default;
    }
    if let (None, None, Some((cost, time))) =
        (config.drop_max_cost, &config.drop_max_time, old.drop)
    {
        config.drop_max_cost = Some(cost);
        config.drop_max_time = Some(time);
    }
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("crucible-playbooks").join("config.toml")
}

fn find_tx02() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TX02_FONT") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    font_dirs().iter().find_map(|dir| scan(dir, 0))
}

fn font_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join("Library/Fonts"));
        dirs.push(home.join(".local/share/fonts"));
        dirs.push(home.join(".fonts"));
    }
    for dir in [
        "/Library/Fonts",
        "/System/Library/Fonts",
        "/usr/local/share/fonts",
        "/usr/share/fonts",
    ] {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

/// Depth-bounded so a font tree with a symlink loop cannot hang the app at startup.
fn scan(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 3 {
        return None;
    }
    let entries = fs::read_dir(dir).ok()?;
    let mut fallback = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = scan(&path, depth + 1) {
                return Some(found);
            }
            continue;
        }
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        let extension = path.extension()?.to_string_lossy().to_lowercase();
        if extension != "ttf" && extension != "otf" {
            continue;
        }
        if !(name.contains("tx-02") || name.contains("tx02")) {
            continue;
        }
        if ["italic", "oblique", "bold", "semicondensed"]
            .iter()
            .any(|skip| name.contains(skip))
        {
            continue;
        }
        if name.contains("regular") {
            return Some(path);
        }
        fallback.get_or_insert(path);
    }
    fallback
}

#[cfg(test)]
mod tests {
    #[test]
    fn zoom_steps_walk_the_known_zooms_and_stop_at_the_ends() {
        use crate::settings::{ZOOMS, zoom_step};
        assert_eq!(zoom_step(1.0, -1), 0.9);
        assert_eq!(zoom_step(1.0, 1), 1.1);
        assert_eq!(zoom_step(ZOOMS[0], -1), ZOOMS[0]);
        assert_eq!(zoom_step(1.5, 1), 1.5);
        assert_eq!(
            zoom_step(0.95, -1),
            0.9,
            "between stops goes to the one below"
        );
        assert_eq!(zoom_step(0.95, 1), 1.0);
    }

    use crate::settings::*;

    #[test]
    fn the_update_check_is_on_until_turned_off_and_stays_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crucible-playbooks").join("config.toml");
        let mut settings = Settings::load_from(path.clone());
        assert!(settings.updates);
        settings.set_updates(false);
        assert!(!Settings::load_from(path.clone()).updates);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("check_updates = false")
        );
    }

    #[test]
    fn with_no_profiles_of_its_own_the_app_takes_the_tauri_apps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crucible-playbooks").join("config.toml");
        let tauri = crate::profile::tauri_config(dir.path());
        fs::create_dir_all(tauri.parent().unwrap()).unwrap();
        fs::write(
            &tauri,
            "default_profile = \"local\"\n[profiles.local]\nurl = \"http://127.0.0.1:8870\"\n[drop]\nmax_cost = 3.0\nmax_time = \"1h\"\n",
        )
        .unwrap();
        let settings = Settings::load_from(path.clone());
        assert!(settings.profiles()["local"].is_ok());
        assert_eq!(settings.profile.as_deref(), Some("local"));
        assert_eq!(
            (settings.drop_max_cost, settings.drop_max_time.as_str()),
            (3.0, "1h")
        );

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[profiles.own]\nurl = \"https://own.example.com\"\n").unwrap();
        let own = Settings::load_from(path);
        assert_eq!(
            own.profiles().keys().collect::<Vec<_>>(),
            ["own"],
            "its own profiles win"
        );
        assert_eq!(own.drop_max_cost, 1.0);
    }

    #[test]
    fn a_bad_profile_spoils_only_itself_and_survives_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
theme = "moss"
profile = "good"

[profiles.good]
url = "https://api.example.com"

[profiles.typo]
url = "https://api.example.com"
proxxy = "socks5://127.0.0.1:1080"
"#,
        )
        .unwrap();
        let mut settings = Settings::load_from(path.clone());
        assert_eq!(
            settings.theme,
            Theme::Moss,
            "the rest of the file still reads"
        );
        let profiles = settings.profiles();
        assert!(profiles["good"].is_ok());
        assert!(profiles["typo"].as_ref().unwrap_err().contains("proxxy"));

        settings.set_theme(Theme::Ember);
        let again = Settings::load_from(path.clone());
        assert!(
            fs::read_to_string(&path).unwrap().contains("proxxy"),
            "the bad profile is kept as written for its owner to fix"
        );
        assert_eq!(again.profile.as_deref(), Some("good"));

        let jump = Profile {
            url: "https://internal.example.com".into(),
            proxy: Some("socks5h://127.0.0.1:1080".into()),
            tunnel: Some("ssh -N -D 1080 jumpbox".into()),
            shell: Some("/bin/bash".into()),
            ..Profile::default()
        };
        settings.set_profile_def("jump", &jump).unwrap();
        assert_eq!(
            Settings::load_from(path.clone()).profiles()["jump"],
            Ok(jump)
        );
        assert!(
            settings
                .set_profile_def("bad name", &Profile::default())
                .is_err()
        );
        settings.use_profile("jump");
        settings.remove_profile("jump");
        let after = Settings::load_from(path);
        assert_eq!(
            after.profile, None,
            "removing the profile in use forgets it"
        );
        assert!(!after.profiles().contains_key("jump"));
    }

    #[test]
    fn the_team_a_profile_acts_as_is_kept_per_profile_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crucible-playbooks").join("config.toml");
        let mut settings = Settings::load_from(path.clone());
        assert_eq!(settings.act_as("mpp"), None);

        settings.set_act_as("mpp", Some("team:infra".into()));
        settings.set_act_as("local", Some("team:lab".into()));
        let read = Settings::load_from(path.clone());
        assert_eq!(read.act_as("mpp").as_deref(), Some("team:infra"));
        assert_eq!(read.act_as("local").as_deref(), Some("team:lab"));

        settings.set_act_as("mpp", None);
        let read = Settings::load_from(path.clone());
        assert_eq!(read.act_as("mpp"), None);
        assert_eq!(read.act_as("local").as_deref(), Some("team:lab"));
        settings.set_act_as("local", None);
        assert!(!fs::read_to_string(&path).unwrap().contains("act_as"));
    }

    #[test]
    fn a_file_that_does_not_read_is_reported_and_never_saved_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crucible-playbooks").join("config.toml");
        let tauri = crate::profile::tauri_config(dir.path());
        fs::create_dir_all(tauri.parent().unwrap()).unwrap();
        fs::write(&tauri, "[profiles.old]\nurl = \"http://127.0.0.1:8870\"\n").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let written = "profile = \"mine\"\ntheme = \"mauve\"\n\n[profiles.mine]\nurl = \"https://mine.example.com\"\n";
        fs::write(&path, written).unwrap();

        let mut settings = Settings::load_from(path.clone());
        let why = settings.unreadable.clone().expect("the file does not read");
        assert!(why.starts_with("line 2: "), "{why}");
        assert!(why.contains("mauve"), "{why}");
        assert!(
            settings.profiles().is_empty(),
            "the Tauri app's profiles do not stand in for a file that is there"
        );

        settings.set_theme(Theme::Ember);
        settings.set_zoom(1.25);
        settings.use_profile("other");
        settings
            .set_profile_def(
                "new",
                &Profile {
                    url: "https://new.example.com".into(),
                    ..Profile::default()
                },
            )
            .unwrap();
        settings.remove_profile("new");
        settings.set_drop("2", "1h").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), written);
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "nothing is left beside it"
        );

        fs::write(&path, written.replace("mauve", "moss")).unwrap();
        let mut fixed = Settings::load_from(path.clone());
        assert_eq!(fixed.unreadable, None);
        assert_eq!(fixed.theme, Theme::Moss);
        fixed.set_theme(Theme::Ember);
        let again = Settings::load_from(path);
        assert_eq!(again.theme, Theme::Ember);
        assert!(again.profiles()["mine"].is_ok());
    }

    #[test]
    fn no_file_is_not_an_unreadable_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crucible-playbooks").join("config.toml");
        let mut settings = Settings::load_from(path.clone());
        assert_eq!(settings.unreadable, None);
        settings.set_theme(Theme::Midnight);
        assert_eq!(Settings::load_from(path).theme, Theme::Midnight);
    }

    #[test]
    fn a_save_replaces_the_file_whole_and_writes_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("playbooks.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "theme = \"moss\"\n").unwrap();
        let path = dir.path().join("config").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let mut settings = Settings::load_from(path.clone());
        assert_eq!(settings.theme, Theme::Moss);
        settings.set_theme(Theme::Ember);
        assert!(
            fs::symlink_metadata(&path).unwrap().is_symlink(),
            "the link stays a link"
        );
        assert!(fs::read_to_string(&real).unwrap().contains("ember"));
        for folder in [real.parent().unwrap(), path.parent().unwrap()] {
            assert_eq!(
                fs::read_dir(folder).unwrap().count(),
                1,
                "no partial file is left in {}",
                folder.display()
            );
        }
    }

    #[test]
    fn the_tx02_scan_ignores_italic_and_condensed_faces() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "TX-02-SemiCondensed.otf",
            "TX-02-Italic.otf",
            "TX-02-Regular.otf",
        ] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        assert_eq!(
            scan(dir.path(), 0).unwrap().file_name().unwrap(),
            "TX-02-Regular.otf"
        );
    }
}
