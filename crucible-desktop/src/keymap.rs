//! The app's commands and the keys that run them: defaults per platform, with the person's own
//! bindings from the config file laid over them. The command palette lists these commands with
//! their keys, and the window's key handler runs the one a key is bound to.

use std::collections::BTreeMap;

use rune::Shortcut;

use crate::Page;
use crate::workspace::{Edge, PaneKind};

/// Something the person can do from the keyboard or the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    Palette,
    /// Search every run of the selected playbook.
    Search,
    /// Focus the pane on that side of the focused one.
    Focus(Edge),
    NextTab,
    PreviousTab,
    Show(PaneKind),
    LogStream,
    LogByTask,
    CopyLink,
    RerunFailed,
    FixWithAgent,
    SwitchBackend,
    Open(Page),
    /// Save the working copy as the draft's next version.
    SaveDraft,
    /// Launch the pack as it stands: a local pack, or the working copy's draft.
    LaunchWorkingCopy,
    /// Open the selected task's definition in the editor.
    EditTask,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// Use the profile at this place in the list, from 0.
    Profile(u8),
    Back,
    Forward,
    /// Read everything from the controller again.
    Reload,
}

impl Command {
    pub const ALL: [Command; 52] = [
        Command::Palette,
        Command::Search,
        Command::Focus(Edge::Left),
        Command::Focus(Edge::Bottom),
        Command::Focus(Edge::Top),
        Command::Focus(Edge::Right),
        Command::NextTab,
        Command::PreviousTab,
        Command::Show(PaneKind::Tasks),
        Command::Show(PaneKind::Graph),
        Command::Show(PaneKind::Timeline),
        Command::Show(PaneKind::Data),
        Command::Show(PaneKind::Transcript),
        Command::Show(PaneKind::Compare),
        Command::Show(PaneKind::Log),
        Command::Show(PaneKind::Pack),
        Command::LogStream,
        Command::LogByTask,
        Command::CopyLink,
        Command::RerunFailed,
        Command::FixWithAgent,
        Command::SwitchBackend,
        Command::Open(Page::Runs),
        Command::Open(Page::Triggers),
        Command::Open(Page::Secrets),
        Command::Open(Page::Decisions),
        Command::Open(Page::Approvals),
        Command::Open(Page::Teams),
        Command::Open(Page::Providers),
        Command::Open(Page::Blueprints),
        Command::Open(Page::Activity),
        Command::Open(Page::Issues),
        Command::Open(Page::Admin),
        Command::Open(Page::Settings),
        Command::SaveDraft,
        Command::LaunchWorkingCopy,
        Command::EditTask,
        Command::ZoomIn,
        Command::ZoomOut,
        Command::ZoomReset,
        Command::Back,
        Command::Forward,
        Command::Reload,
        Command::Profile(0),
        Command::Profile(1),
        Command::Profile(2),
        Command::Profile(3),
        Command::Profile(4),
        Command::Profile(5),
        Command::Profile(6),
        Command::Profile(7),
        Command::Profile(8),
    ];

    /// Its name in the config file's `[keys]` table.
    pub fn id(self) -> String {
        match self {
            Command::Palette => "palette".into(),
            Command::Search => "search-runs".into(),
            Command::Focus(edge) => format!("focus-{}", edge_id(edge)),
            Command::NextTab => "next-tab".into(),
            Command::PreviousTab => "previous-tab".into(),
            Command::Show(kind) => format!("show-{}", kind.label().to_lowercase()),
            Command::LogStream => "log-stream".into(),
            Command::LogByTask => "log-by-task".into(),
            Command::CopyLink => "copy-link".into(),
            Command::RerunFailed => "rerun-failed".into(),
            Command::FixWithAgent => "fix-with-agent".into(),
            Command::SwitchBackend => "switch-backend".into(),
            Command::Open(page) => page.id().into(),
            Command::SaveDraft => "save-draft".into(),
            Command::LaunchWorkingCopy => "launch-working-copy".into(),
            Command::EditTask => "edit-task".into(),
            Command::ZoomIn => "zoom-in".into(),
            Command::ZoomOut => "zoom-out".into(),
            Command::ZoomReset => "zoom-reset".into(),
            Command::Profile(n) => format!("profile-{}", n + 1),
            Command::Back => "back".into(),
            Command::Forward => "forward".into(),
            Command::Reload => "reload".into(),
        }
    }

    pub fn title(self) -> String {
        match self {
            Command::Palette => "Command palette".into(),
            Command::Search => "Search runs".into(),
            Command::Focus(edge) => format!("Focus the pane {}", edge_word(edge)),
            Command::NextTab => "Next tab".into(),
            Command::PreviousTab => "Previous tab".into(),
            Command::Show(kind) => format!("Show {}", kind.label()),
            Command::LogStream => "Log: one stream".into(),
            Command::LogByTask => "Log: by task".into(),
            Command::CopyLink => "Copy a link to this run".into(),
            Command::RerunFailed => "Rerun what did not pass".into(),
            Command::FixWithAgent => "Fix with agent".into(),
            Command::SwitchBackend => "Switch between controller and local".into(),
            Command::Open(page) => page.title().into(),
            Command::SaveDraft => "Save the working copy".into(),
            Command::LaunchWorkingCopy => "Launch the working copy".into(),
            Command::EditTask => "Edit the selected task".into(),
            Command::ZoomIn => "Zoom in".into(),
            Command::ZoomOut => "Zoom out".into(),
            Command::ZoomReset => "Actual size".into(),
            Command::Profile(n) => format!("Use profile {}", n + 1),
            Command::Back => "Back".into(),
            Command::Forward => "Forward".into(),
            Command::Reload => "Reload from the controller".into(),
        }
    }

    /// The keys it has until the config file says otherwise.
    fn defaults(self) -> Vec<&'static str> {
        let primary = |mac: &'static str, other: &'static str| {
            if cfg!(target_os = "macos") {
                mac
            } else {
                other
            }
        };
        match self {
            Command::Palette => vec![primary("cmd+k", "ctrl+k")],
            Command::Search => vec![primary("cmd+shift+f", "ctrl+shift+f")],
            Command::Focus(Edge::Left) => vec!["h", "left"],
            Command::Focus(Edge::Bottom) => vec!["j", "down"],
            Command::Focus(Edge::Top) => vec!["k", "up"],
            Command::Focus(Edge::Right) => vec!["l", "right"],
            Command::NextTab => vec!["]"],
            Command::PreviousTab => vec!["["],
            Command::Show(PaneKind::Tasks) => vec![primary("cmd+1", "ctrl+1")],
            Command::Show(PaneKind::Graph) => vec![primary("cmd+2", "ctrl+2")],
            Command::Show(PaneKind::Timeline) => vec![primary("cmd+3", "ctrl+3")],
            Command::Show(PaneKind::Data) => vec![primary("cmd+4", "ctrl+4")],
            Command::Show(PaneKind::Compare) => vec![primary("cmd+5", "ctrl+5")],
            Command::Show(PaneKind::Log) => vec![primary("cmd+6", "ctrl+6")],
            Command::Show(PaneKind::Pack) => vec![primary("cmd+7", "ctrl+7")],
            Command::Show(PaneKind::Transcript) => vec![primary("cmd+8", "ctrl+8")],
            Command::Open(Page::Settings) => vec![primary("cmd+,", "ctrl+,")],
            Command::Open(Page::Decisions) => vec![primary("cmd+shift+d", "ctrl+shift+d")],
            Command::SaveDraft => vec![primary("cmd+s", "ctrl+s")],
            Command::LaunchWorkingCopy => vec![primary("cmd+enter", "ctrl+enter")],
            Command::EditTask => vec![primary("cmd+e", "ctrl+e")],
            Command::ZoomIn => vec![primary("cmd+=", "ctrl+=")],
            Command::ZoomOut => vec![primary("cmd+-", "ctrl+-")],
            Command::ZoomReset => vec![primary("cmd+0", "ctrl+0")],
            Command::Back => vec![primary("cmd+[", "alt+left")],
            Command::Forward => vec![primary("cmd+]", "alt+right")],
            Command::Reload => vec![primary("cmd+r", "ctrl+r")],
            Command::Profile(n) => {
                let (mac, other) = PROFILE_KEYS[n as usize % PROFILE_KEYS.len()];
                vec![primary(mac, other)]
            }
            _ => Vec::new(),
        }
    }
}

const PROFILE_KEYS: [(&str, &str); 9] = [
    ("cmd+alt+1", "ctrl+alt+1"),
    ("cmd+alt+2", "ctrl+alt+2"),
    ("cmd+alt+3", "ctrl+alt+3"),
    ("cmd+alt+4", "ctrl+alt+4"),
    ("cmd+alt+5", "ctrl+alt+5"),
    ("cmd+alt+6", "ctrl+alt+6"),
    ("cmd+alt+7", "ctrl+alt+7"),
    ("cmd+alt+8", "ctrl+alt+8"),
    ("cmd+alt+9", "ctrl+alt+9"),
];

fn edge_id(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Right => "right",
        Edge::Top => "up",
        Edge::Bottom => "down",
    }
}

fn edge_word(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "to the left",
        Edge::Right => "to the right",
        Edge::Top => "above",
        Edge::Bottom => "below",
    }
}

/// Which keys run which commands.
#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    binds: Vec<(Command, Vec<Shortcut>)>,
}

/// A binding in the config file that could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub command: String,
    pub key: String,
    pub why: String,
}

impl Keymap {
    /// The defaults with `overrides` (command id to its keys, empty to unbind) laid over them.
    /// A key bound twice keeps its first command; what could not be used is returned.
    pub fn new(overrides: &BTreeMap<String, Vec<String>>) -> (Keymap, Vec<Refused>) {
        let mut refused: Vec<Refused> = overrides
            .keys()
            .filter(|id| !Command::ALL.iter().any(|c| c.id() == **id))
            .map(|id| Refused {
                command: id.clone(),
                key: String::new(),
                why: "no such command".into(),
            })
            .collect();
        let mut taken: Vec<Shortcut> = Vec::new();
        let mut binds = Vec::new();
        for command in Command::ALL {
            let keys: Vec<String> = match overrides.get(&command.id()) {
                Some(keys) => keys.clone(),
                None => command.defaults().into_iter().map(String::from).collect(),
            };
            let mut shortcuts = Vec::new();
            for key in keys {
                let refuse = |why: String| Refused {
                    command: command.id(),
                    key: key.clone(),
                    why,
                };
                match key.parse::<Shortcut>() {
                    Err(error) => refused.push(refuse(error.to_string())),
                    Ok(shortcut) if taken.contains(&shortcut) => {
                        refused.push(refuse("already bound".into()));
                    }
                    Ok(shortcut) => {
                        taken.push(shortcut.clone());
                        shortcuts.push(shortcut);
                    }
                }
            }
            binds.push((command, shortcuts));
        }
        (Keymap { binds }, refused)
    }

    pub fn command(&self, shortcut: &Shortcut) -> Option<Command> {
        self.binds
            .iter()
            .find(|(_, keys)| keys.contains(shortcut))
            .map(|(command, _)| *command)
    }

    pub fn keys(&self, command: Command) -> &[Shortcut] {
        self.binds
            .iter()
            .find(|(c, _)| *c == command)
            .map_or(&[], |(_, keys)| keys.as_slice())
    }

    /// Which command already has `shortcut`, other than `command`.
    pub fn clash(&self, command: Command, shortcut: &Shortcut) -> Option<Command> {
        self.command(shortcut).filter(|other| *other != command)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rune::{Mods, PhysicalKeyCode, Shortcut};

    use crate::keymap::{Command, Keymap};
    use crate::workspace::{Edge, PaneKind};

    fn key(text: &str) -> Shortcut {
        text.parse().unwrap()
    }

    #[test]
    fn every_command_has_a_distinct_id_and_the_defaults_all_parse() {
        let mut ids: Vec<String> = Command::ALL.iter().map(|c| c.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), Command::ALL.len());
        let (keymap, refused) = Keymap::new(&BTreeMap::new());
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(keymap.command(&key("h")), Some(Command::Focus(Edge::Left)));
        assert_eq!(
            keymap.command(&key("down")),
            Some(Command::Focus(Edge::Bottom))
        );
        assert_eq!(keymap.command(&key("]")), Some(Command::NextTab));
        assert_eq!(keymap.keys(Command::CopyLink), []);
    }

    #[test]
    fn a_key_event_finds_its_command_by_physical_key() {
        let (keymap, _) = Keymap::new(&BTreeMap::new());
        let mods = Mods {
            cmd: true,
            ..Mods::default()
        };
        let pressed = Shortcut::from_physical_key(PhysicalKeyCode::KeyK, mods).unwrap();
        assert_eq!(keymap.command(&pressed), Some(Command::Palette));
        let bare = Shortcut::from_physical_key(PhysicalKeyCode::KeyK, Mods::default()).unwrap();
        assert_eq!(keymap.command(&bare), Some(Command::Focus(Edge::Top)));
    }

    #[test]
    fn the_config_rebinds_unbinds_and_reports_what_it_could_not_use() {
        let overrides = BTreeMap::from([
            ("palette".to_string(), vec!["ctrl+p".to_string()]),
            ("focus-left".to_string(), vec![]),
            (
                "copy-link".to_string(),
                vec!["ctrl+p".to_string(), "nonsense+".to_string()],
            ),
            ("fly".to_string(), vec!["f".to_string()]),
        ]);
        let (keymap, refused) = Keymap::new(&overrides);
        assert_eq!(keymap.command(&key("ctrl+p")), Some(Command::Palette));
        assert_eq!(keymap.command(&key("h")), None);
        assert_eq!(keymap.keys(Command::Focus(Edge::Left)), []);
        let mut why: Vec<(&str, &str)> = refused
            .iter()
            .map(|r| (r.command.as_str(), r.key.as_str()))
            .collect();
        why.sort();
        assert_eq!(
            why,
            [
                ("copy-link", "ctrl+p"),
                ("copy-link", "nonsense+"),
                ("fly", "")
            ]
        );
        assert_eq!(
            keymap.clash(Command::Show(PaneKind::Log), &key("ctrl+p")),
            Some(Command::Palette)
        );
        assert_eq!(keymap.clash(Command::Palette, &key("ctrl+p")), None);
    }
}
