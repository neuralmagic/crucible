//! Handing a pack file to the user's editor: a deep link to a line of a file on disk, or the
//! file's text over stdin as a scratch buffer.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Editor {
    VsCode,
    Cursor,
    Zed,
}

impl Editor {
    pub const ALL: [Editor; 3] = [Editor::VsCode, Editor::Cursor, Editor::Zed];

    pub fn label(self) -> &'static str {
        match self {
            Editor::VsCode => "VS Code",
            Editor::Cursor => "Cursor",
            Editor::Zed => "Zed",
        }
    }

    fn app(self) -> &'static str {
        match self {
            Editor::VsCode => "Visual Studio Code.app",
            Editor::Cursor => "Cursor.app",
            Editor::Zed => "Zed.app",
        }
    }

    /// The command-line tool's name, and where the app bundle keeps it.
    fn cli(self) -> (&'static str, &'static str) {
        match self {
            Editor::VsCode => ("code", "Contents/Resources/app/bin/code"),
            Editor::Cursor => ("cursor", "Contents/Resources/app/bin/cursor"),
            Editor::Zed => ("zed", "Contents/MacOS/cli"),
        }
    }

    /// A link that opens `path` at 1-based `line`.
    pub fn link(self, path: &Path, line: usize) -> String {
        let encoded = crate::lsp::uri(path);
        let path = encoded.trim_start_matches("file://");
        match self {
            Editor::VsCode => format!("vscode://file{path}:{line}:1"),
            Editor::Cursor => format!("cursor://file{path}:{line}:1"),
            Editor::Zed => format!("zed://file{path}:{line}"),
        }
    }

    /// The editor `$VISUAL` or `$EDITOR` names, else the first one installed.
    pub fn detect() -> Option<Editor> {
        let named = ["VISUAL", "EDITOR"]
            .into_iter()
            .filter_map(|var| std::env::var(var).ok())
            .find_map(|value| from_command(&value));
        named.or_else(|| {
            [Editor::Cursor, Editor::Zed, Editor::VsCode]
                .into_iter()
                .find(|editor| bundle(*editor).is_some())
        })
    }
}

/// The editor a command like `code --wait` or `/usr/local/bin/zed` runs.
fn from_command(command: &str) -> Option<Editor> {
    let program = command.split_whitespace().next()?;
    let name = Path::new(program).file_name()?.to_str()?;
    match name {
        "code" | "code-insiders" => Some(Editor::VsCode),
        "cursor" => Some(Editor::Cursor),
        "zed" | "zeditor" => Some(Editor::Zed),
        _ => None,
    }
}

fn bundle(editor: Editor) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        Some(PathBuf::from("/Applications")),
        home.map(|h| h.join("Applications")),
    ]
    .into_iter()
    .flatten()
    .map(|dir| dir.join(editor.app()))
    .find(|app| app.is_dir())
}

fn cli(editor: Editor) -> Option<PathBuf> {
    let (name, inside) = editor.cli();
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .chain([
                    PathBuf::from("/usr/local/bin"),
                    PathBuf::from("/opt/homebrew/bin"),
                ])
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
        })
        .or_else(|| Some(bundle(editor)?.join(inside)).filter(|path| path.is_file()))
}

/// Open `path` at `line` in `editor`.
pub fn open_file(editor: Editor, path: &Path, line: usize) -> Result<(), String> {
    Command::new("open")
        .arg(editor.link(path, line))
        .spawn()
        .map(drop)
        .map_err(|e| format!("opening {}: {e}", editor.label()))
}

/// Open `text` in `editor` as an unsaved buffer.
pub fn open_text(editor: Editor, text: &str) -> Result<(), String> {
    let program = cli(editor)
        .ok_or_else(|| format!("{}'s command-line tool is not installed", editor.label()))?;
    let mut child = Command::new(&program)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("starting {}: {e}", program.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{} has no stdin", program.display()))?;
    let text = text.to_string();
    std::thread::spawn(move || {
        let _ = stdin.write_all(text.as_bytes());
        drop(stdin);
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::editor::{Editor, from_command};

    #[test]
    fn links_name_the_file_and_line() {
        let path = Path::new("/Users/me/my packs/workflow.star");
        assert_eq!(
            Editor::VsCode.link(path, 12),
            "vscode://file/Users/me/my%20packs/workflow.star:12:1"
        );
        assert_eq!(
            Editor::Cursor.link(path, 1),
            "cursor://file/Users/me/my%20packs/workflow.star:1:1"
        );
        assert_eq!(
            Editor::Zed.link(path, 3),
            "zed://file/Users/me/my%20packs/workflow.star:3"
        );
    }

    #[test]
    fn editor_commands_name_their_editor() {
        assert_eq!(from_command("code --wait"), Some(Editor::VsCode));
        assert_eq!(from_command("/opt/homebrew/bin/zed -w"), Some(Editor::Zed));
        assert_eq!(from_command("cursor"), Some(Editor::Cursor));
        assert_eq!(from_command("nvim"), None);
        assert_eq!(from_command(""), None);
    }
}
