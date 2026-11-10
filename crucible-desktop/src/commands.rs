//! What the keymap's commands and the palette's entries do, and the window's key handler that
//! runs whichever command a key is bound to.

use rune::{Key, KeyEvent, Shortcut, ShortcutError, Tx};
use rune_kit::ShortcutRecorderState;

use crate::keymap::{Command, Keymap};
use crate::palette::{self, Entry, Item};
use crate::unfurl::LogMode;
use crate::workspace::{Edge, PaneKind, neighbour};
use crate::{App, Backend, Selection};

impl App {
    pub fn run_command(&mut self, command: Command, tx: &mut Tx) {
        match command {
            Command::Palette => self.toggle_palette(),
            Command::Search => self.toggle_finder(),
            Command::Focus(edge) => self.move_focus(edge),
            Command::NextTab => self.cycle_tab(1),
            Command::PreviousTab => self.cycle_tab(-1),
            Command::Show(kind) => {
                self.show_pane(kind);
                self.pane_focus = Some(kind);
            }
            Command::LogStream => self.log_mode = Some(LogMode::Stream),
            Command::LogByTask => self.log_mode = Some(LogMode::ByTask),
            Command::CopyLink => {
                if let Some(link) = self.link_here(self.run.expanded.as_deref()) {
                    tx.copy(link.url());
                }
            }
            Command::RerunFailed => {
                if self.rerunnable().is_some() {
                    self.open_rerun(Vec::new());
                }
            }
            Command::FixWithAgent => {
                let task = self.run.expanded.clone();
                if let Some(brief) = self.hand_off(task.as_deref()) {
                    tx.copy(brief);
                }
            }
            Command::SwitchBackend => self.switch(match self.backend {
                Backend::Controller => Backend::Local,
                Backend::Local => Backend::Controller,
            }),
            Command::Open(page) => self.toggle_page(page),
            Command::SaveDraft => {
                if self.remote.working_edited && !self.remote.draft_busy {
                    self.save_draft();
                }
            }
            Command::LaunchWorkingCopy => match self.backend {
                Backend::Local => self.launch(),
                Backend::Controller => self.launch_working(),
            },
            Command::ZoomIn => self.set_zoom(crate::settings::zoom_step(self.settings.zoom, 1)),
            Command::ZoomOut => {
                self.set_zoom(crate::settings::zoom_step(self.settings.zoom, -1));
            }
            Command::ZoomReset => self.set_zoom(1.0),
            Command::Back => self.go(-1),
            Command::Forward => self.go(1),
            Command::Reload => {
                self.remote.live_client.reconnect();
                self.remote.admin_client.reconnect();
            }
            Command::Profile(n) => {
                self.switch(Backend::Controller);
                self.use_profile_at(n as usize);
            }
            Command::EditTask => {
                if let Some(task) = self.run.expanded.clone() {
                    self.edit_task(&task);
                }
            }
        }
    }

    /// The pack as it compiles now, for what the views show: a local pack's, else the working
    /// copy's.
    pub fn shown_preview(&self) -> Option<&crate::preview::Preview> {
        match self.backend {
            Backend::Local => self.selected_pack().map(|pack| &pack.preview),
            Backend::Controller => self.remote.working_preview.as_ref(),
        }
    }

    /// Open `task`'s definition in the editor.
    pub fn edit_task(&mut self, task: &str) {
        let task = task.split_once('[').map_or(task, |(name, _)| name);
        if let Some((path, line)) = self.shown_preview().and_then(|p| p.definition(task)) {
            self.open_path_in_editor(&path, line);
        }
    }

    pub fn toggle_palette(&mut self) {
        self.palette = match self.palette.take() {
            Some(_) => None,
            None => {
                let mut open = palette::Palette::default();
                open.query.focused = true;
                Some(open)
            }
        };
    }

    /// What the palette offers now: the open run's tasks, the commands, playbooks and packs.
    pub fn palette_entries(&self) -> Vec<Entry> {
        let tasks: Vec<&str> = self.run_tasks().iter().map(|t| t.name.as_str()).collect();
        let playbooks: Vec<&str> = self
            .remote
            .live
            .playbooks(&self.launch_filter())
            .into_iter()
            .map(|p| p.id.as_str())
            .collect();
        let packs: Vec<&str> = self.packs.iter().map(|p| p.name.as_str()).collect();
        let profiles: Vec<String> = crate::profile::usable(&self.settings)
            .into_keys()
            .filter(|name| *name != self.remote.target.name)
            .collect();
        let profiles: Vec<&str> = profiles.iter().map(String::as_str).collect();
        palette::entries(&tasks, &playbooks, &packs, &profiles)
    }

    /// Run the palette's highlighted entry, or the one at `index` of its ranked list.
    pub fn pick_entry(&mut self, index: Option<usize>, tx: &mut Tx) {
        let Some(open) = &self.palette else {
            return;
        };
        let entries = self.palette_entries();
        let ranked = palette::rank(&entries, &open.query.text);
        let Some(entry) = ranked.get(index.unwrap_or(open.selected)) else {
            return;
        };
        let item = entry.item.clone();
        self.palette = None;
        match item {
            Item::Run(command) => self.run_command(command, tx),
            Item::Task(task) => self.pick_task(&task, None),
            Item::Playbook(id) => {
                self.switch(Backend::Controller);
                self.select(Selection::playbook(&id));
            }
            Item::Pack(name) => {
                self.switch(Backend::Local);
                self.select(Selection::Local(name));
            }
            Item::Profile(name) => {
                self.switch(Backend::Controller);
                self.use_profile(&name);
            }
        }
    }

    /// Move the palette's highlight by `step`, within its ranked list.
    pub fn step_palette(&mut self, step: isize) {
        let shown = {
            let Some(open) = &self.palette else {
                return;
            };
            let entries = self.palette_entries();
            palette::rank(&entries, &open.query.text).len()
        };
        if let Some(open) = &mut self.palette {
            let last = shown.saturating_sub(1) as isize;
            open.selected = (open.selected as isize + step).clamp(0, last) as usize;
        }
    }

    /// The pane keyboard navigation starts from: the focused one, else Tasks.
    pub fn focused_pane(&self) -> PaneKind {
        let workspace = self.workspace();
        self.pane_focus
            .filter(|kind| workspace.showing(*kind))
            .unwrap_or(PaneKind::Tasks)
    }

    fn move_focus(&mut self, edge: Edge) {
        let workspace = self.workspace();
        let from = self
            .pane_focus
            .and_then(|kind| workspace.locate(kind))
            .or_else(|| workspace.locate(PaneKind::Tasks))
            .unwrap_or((0, 0));
        if self.pane_focus.is_none() {
            self.pane_focus = workspace.group(from).map(|g| g.active);
            return;
        }
        if let Some(to) = neighbour(&self.geometry.groups, from, edge) {
            self.pane_focus = workspace.group(to).map(|g| g.active);
        }
    }

    fn cycle_tab(&mut self, step: isize) {
        let workspace = self.workspace();
        let Some(group) = workspace
            .locate(self.focused_pane())
            .and_then(|at| workspace.group(at))
        else {
            return;
        };
        let Some(i) = group.tabs.iter().position(|k| *k == group.active) else {
            return;
        };
        let n = group.tabs.len() as isize;
        let next = group.tabs[((i as isize + step).rem_euclid(n)) as usize];
        self.show_pane(next);
        self.pane_focus = Some(next);
    }

    /// Bind `command` to `shortcut` alone, or to nothing with `None`. Refused when another
    /// command has that key.
    pub fn rebind(
        &mut self,
        command: Command,
        shortcut: Option<&Shortcut>,
    ) -> Result<(), ShortcutError> {
        if let Some(shortcut) = shortcut
            && let Some(other) = self.keymap.clash(command, shortcut)
        {
            return Err(ShortcutError::InvalidAccelerator(format!(
                "{} already runs {}",
                shortcut,
                other.title()
            )));
        }
        let keys = shortcut.map(|s| s.to_string()).into_iter().collect();
        self.set_keys(command, Some(keys));
        Ok(())
    }

    /// Put `command` back on its default keys.
    pub fn reset_keys(&mut self, command: Command) {
        self.set_keys(command, None);
        self.recorders.remove(&command);
    }

    fn set_keys(&mut self, command: Command, keys: Option<Vec<String>>) {
        self.settings.set_keys(&command.id(), keys);
        let (keymap, refused) = Keymap::new(&self.settings.keys);
        self.keymap = keymap;
        self.keys_refused = refused;
    }

    /// The recorder showing `command`'s first key in Settings.
    pub fn recorder(&mut self, command: Command) -> &mut ShortcutRecorderState {
        let first = self.keymap.keys(command).first().cloned();
        self.recorders
            .entry(command)
            .or_insert_with(|| ShortcutRecorderState::new(first))
    }
}

/// The palette field's keys: arrows move the highlight, Escape or the palette's own key close it.
/// Return goes through the field's submit.
pub fn palette_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match ev.key {
        Key::Up => app.step_palette(-1),
        Key::Down => app.step_palette(1),
        Key::Escape => app.palette = None,
        _ if pressed(tx, ev).is_some_and(|s| app.keymap.command(&s) == Some(Command::Palette)) => {
            app.palette = None;
        }
        _ => return false,
    }
    true
}

/// The finder field's keys: the arrows move through hits, Escape and the finder's own key close it.
pub fn finder_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match ev.key {
        Key::Up => app.step_finder(-1),
        Key::Down => app.step_finder(1),
        Key::Escape => app.remote.finder = None,
        _ if pressed(tx, ev).is_some_and(|s| app.keymap.command(&s) == Some(Command::Search)) => {
            app.remote.finder = None;
        }
        _ => return false,
    }
    true
}

/// The window's keys: whichever command the key is bound to, unless a key is being recorded.
pub fn on_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if rune_knobs::key(&mut app.knobs, ev) {
        tx.motion(app.knobs.slide());
        return true;
    }
    if app
        .recorders
        .values()
        .any(ShortcutRecorderState::is_recording)
    {
        return false;
    }
    let Some(command) = pressed(tx, ev).and_then(|s| app.keymap.command(&s)) else {
        return false;
    };
    app.run_command(command, tx);
    true
}

/// The chord a key event is, by the physical key behind it.
fn pressed(tx: &Tx, ev: &KeyEvent) -> Option<Shortcut> {
    Shortcut::from_physical_key(tx.physical_key()?, ev.mods).ok()
}
