//! The app's commands and the keys that run them: defaults per platform, with the person's own
//! bindings from the config file laid over them. The command palette lists these commands with
//! their keys, and the window's key handler runs the one a key is bound to.

use std::collections::BTreeMap;

use rune::Shortcut;

use crate::workspace::{Edge, PaneKind};

/// Something the person can do from the keyboard or the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    Palette,
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
    Settings,
}

impl Command {
    pub const ALL: [Command; 21] = [
        Command::Palette,
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
        Command::Show(PaneKind::Compare),
        Command::Show(PaneKind::Log),
        Command::Show(PaneKind::Pack),
        Command::LogStream,
        Command::LogByTask,
        Command::CopyLink,
        Command::RerunFailed,
        Command::FixWithAgent,
        Command::SwitchBackend,
        Command::Settings,
    ];

    /// Its name in the config file's `[keys]` table.
    pub fn id(self) -> String {
        match self {
            Command::Palette => "palette".into(),
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
            Command::Settings => "settings".into(),
        }
    }

    pub fn title(self) -> String {
        match self {
            Command::Palette => "Command palette".into(),
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
            Command::Settings => "Settings".into(),
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
            Command::Settings => vec![primary("cmd+,", "ctrl+,")],
            _ => Vec::new(),
        }
    }
}

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
