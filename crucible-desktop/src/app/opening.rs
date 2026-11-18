//! Opening files in the user's editor.

use crate::editor::Editor;
use crate::pack::{Disk, PackFile};
use crate::{App, LoginEditor, Open, editor};

impl App {
    /// The command the user set, or the editor app to open files in. `None` while the login
    /// shell is still being asked which editor it names.
    fn editor(&mut self) -> Option<Result<Result<Editor, String>, String>> {
        let command = self.settings.editor_command.trim();
        if self.settings.editor_custom && !command.is_empty() {
            return Some(Ok(Err(command.to_string())));
        }
        if self.settings.editor_custom {
            return Some(Err(format!(
                "Set the editor command in Settings, like `{}`",
                editor::EXAMPLE
            )));
        }
        match self.settings.editor {
            Some(app) => Some(Ok(Ok(app))),
            None => {
                let named = self.login_editor()?;
                Some(Editor::detect(named.as_deref()))
            }
        }
    }

    /// The editor the user's shell names, asked of a login shell on a thread of its own the
    /// first time. `None` until that answers.
    fn login_editor(&mut self) -> Option<Option<String>> {
        match &self.login_editor {
            LoginEditor::Known(named) => return Some(named.clone()),
            LoginEditor::Asking(_) => return None,
            LoginEditor::Unasked => {}
        }
        if let Some(own) = editor::env_editor() {
            self.login_editor = LoginEditor::Known(Some(own.clone()));
            return Some(Some(own));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        let asked = std::thread::Builder::new()
            .name("crucible-playbooks-login-editor".into())
            .spawn(move || {
                let _ = tx.send(editor::shell_editor());
                if let Some(waker) = waker {
                    waker();
                }
            });
        match asked {
            Ok(_) => {
                self.login_editor = LoginEditor::Asking(rx);
                None
            }
            Err(_) => {
                self.login_editor = LoginEditor::Known(None);
                Some(None)
            }
        }
    }

    /// Take the login shell's answer, and open what was waiting on it.
    pub(crate) fn finish_login_editor(&mut self) -> bool {
        let LoginEditor::Asking(rx) = &self.login_editor else {
            return false;
        };
        let named = match rx.try_recv() {
            Ok(named) => named,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        self.login_editor = LoginEditor::Known(named);
        if let Some(what) = self.opening.take() {
            self.open(what);
        }
        true
    }

    /// Open `what` in the user's editor, once it is known which that is.
    pub(crate) fn open(&mut self, what: Open) {
        let Some(editor) = self.editor() else {
            self.opening = Some(what);
            return;
        };
        self.editor_error = match editor {
            Ok(editor) => self.open_with(editor, &what).err(),
            Err(why) => Some(why),
        };
    }

    fn open_with(&self, editor: Result<Editor, String>, what: &Open) -> Result<(), String> {
        let root = std::path::Path::new("/");
        match what {
            Open::Path { path, line } => match editor {
                Ok(app) => editor::open_file(app, path, *line),
                Err(command) => {
                    editor::run_command(&command, path, *line, path.parent().unwrap_or(root))
                }
            },
            Open::PackFile { file, line } => {
                let Some(pack) = self.shown_pack() else {
                    return Ok(());
                };
                match (&pack.disk, pack.file(file), editor) {
                    (Some(Disk::Checkout(dir)), _, Ok(app)) => {
                        editor::open_file(app, &dir.join(file), *line)
                    }
                    (Some(Disk::Checkout(dir)), _, Err(command)) => {
                        editor::run_command(&command, &dir.join(file), *line, dir)
                    }
                    (
                        _,
                        Some(PackFile {
                            text: Some(text), ..
                        }),
                        Ok(app),
                    ) => editor::open_text(app, text),
                    (
                        _,
                        Some(PackFile {
                            text: Some(text), ..
                        }),
                        Err(command),
                    ) => editor::scratch(file, text).and_then(|path| {
                        editor::run_command(&command, &path, *line, path.parent().unwrap_or(root))
                    }),
                    _ => Err(format!("{file} has no text to open")),
                }
            }
            Open::Captured => {
                let path = self
                    .remote
                    .opened_file
                    .as_ref()
                    .and_then(|file| file.disk.clone())
                    .ok_or("The file has no copy on disk to open")?;
                match editor {
                    Ok(app) => editor::open_file(app, &path, 1),
                    Err(command) => {
                        editor::run_command(&command, &path, 1, path.parent().unwrap_or(root))
                    }
                }
            }
        }
    }

    /// Open line `line` (1-based) of the file at `path` in the user's editor.
    pub fn open_path_in_editor(&mut self, path: &std::path::Path, line: usize) {
        self.open(Open::Path {
            path: path.to_path_buf(),
            line,
        });
    }

    /// Open line `line` (1-based) of pack file `file` in the user's editor: the file itself from
    /// a checkout, or its text as a scratch buffer from a controller's copy.
    pub fn open_in_editor(&mut self, file: &str, line: usize) {
        self.open(Open::PackFile {
            file: file.to_string(),
            line,
        });
    }
}
