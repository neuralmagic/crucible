//! The app's own settings. Only the log viewer's font so far.
//!
//! The sans is the theme's Inter. The mono slot and the log fonts are the theme's IBM Plex Mono (OFL)
//! or a font read from this machine's font store at startup; no proprietary font is ever bundled.

use std::fs;
use std::path::{Path, PathBuf};

use rune::tok::Family;
use serde::{Deserialize, Serialize};

use crate::editor::Editor;
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
    /// Draw charts through the CRT pass.
    pub crt: bool,
    /// Announce the signed-in user's runs as they finish.
    pub notify: bool,
    /// The editor pack files open in; `None` finds one.
    pub editor: Option<Editor>,
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
    crt: Option<bool>,
    notify: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
}

impl Settings {
    pub fn load() -> Self {
        let path = config_path();
        let config: Config = fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
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
            crt: config.crt.unwrap_or(true),
            notify: config.notify.unwrap_or(true),
            editor: config.editor,
            path,
        }
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

    pub fn set_notify(&mut self, on: bool) {
        self.notify = on;
        self.save();
    }

    pub fn set_editor(&mut self, editor: Option<Editor>) {
        self.editor = editor;
        self.save();
    }

    pub fn set_appearance(&mut self, appearance: Appearance) {
        self.appearance = appearance;
        self.save();
    }

    fn save(&self) {
        let config = Config {
            log_font: Some(self.log_font),
            tx02_path: None,
            layout: self.layout.clone(),
            theme: self.theme,
            appearance: self.appearance,
            crt: Some(self.crt),
            notify: Some(self.notify),
            editor: self.editor,
        };
        let Ok(text) = toml::to_string(&config) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&self.path, text);
    }
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
    use crate::settings::*;

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
