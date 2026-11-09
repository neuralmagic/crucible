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

    /// The editor `$VISUAL` or `$EDITOR` names, else the first one installed. One they name that
    /// is not an app this knows, a terminal editor such as `nvim`, is refused rather than passed
    /// over for an app the user did not pick.
    pub fn detect() -> Result<Editor, String> {
        let named = ["VISUAL", "EDITOR"]
            .into_iter()
            .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()));
        if let Some(named) = named {
            return from_command(&named).ok_or_else(|| {
                format!(
                    "$EDITOR is {}, which needs a terminal. Set an editor command in Settings, \
                     like `{EXAMPLE}`.",
                    named.trim()
                )
            });
        }
        [Editor::Cursor, Editor::Zed, Editor::VsCode]
            .into_iter()
            .find(|editor| bundle(*editor).is_some())
            .ok_or_else(|| "No editor found. Pick one in Settings.".to_string())
    }
}

/// An editor command a terminal editor fits.
pub const EXAMPLE: &str = "ghostty -e nvim +{line} {file}";

/// `template` with `{file}` and `{line}` filled in, the path quoted for `sh`.
pub fn command_line(template: &str, path: &Path, line: usize) -> String {
    template
        .replace("{file}", &shell_quote(&path.to_string_lossy()))
        .replace("{line}", &line.to_string())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Run an editor command for `path` at `line` through `sh`, with the usual tool directories on
/// `PATH`: an app opened from Finder starts with only the system ones.
pub fn run_command(template: &str, path: &Path, line: usize) -> Result<(), String> {
    if !template.contains("{file}") {
        return Err("The editor command needs {file} where the path goes".to_string());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/Applications/Ghostty.app/Contents/MacOS",
        "/Applications/kitty.app/Contents/MacOS",
        "/Applications/WezTerm.app/Contents/MacOS",
        "/Applications/Alacritty.app/Contents/MacOS",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = home {
        dirs.extend([home.join(".local/bin"), home.join(".cargo/bin")]);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    let search = std::env::join_paths(dirs).map_err(|e| format!("building PATH: {e}"))?;
    Command::new("/bin/sh")
        .arg("-c")
        .arg(command_line(template, path, line))
        .env("PATH", search)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("running the editor command: {e}"))
}

/// Write `text` to a scratch file named `name`, for an editor that opens files only.
pub fn scratch(name: &str, text: &str) -> Result<PathBuf, String> {
    let dir = crate::httpcache::cache_root()
        .ok_or("no cache directory for a scratch copy")?
        .join("scratch");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let file = Path::new(name)
        .file_name()
        .map_or_else(|| "pack-file".into(), |n| n.to_owned());
    let path = dir.join(file);
    std::fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
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

/// Open `url` in the default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    Command::new("open")
        .arg(url)
        .spawn()
        .map(drop)
        .map_err(|e| format!("opening {url}: {e}"))
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

    use crate::editor::{Editor, command_line, from_command};

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

    #[test]
    fn an_editor_command_gets_the_path_quoted_and_the_line() {
        assert_eq!(
            command_line(
                "ghostty -e nvim +{line} {file}",
                Path::new("/Users/me/my packs/it's.star"),
                12
            ),
            "ghostty -e nvim +12 '/Users/me/my packs/it'\\''s.star'"
        );
        assert_eq!(
            command_line("nvim --remote {file}", Path::new("/a/$(rm -rf ~)"), 1),
            "nvim --remote '/a/$(rm -rf ~)'",
            "nothing in the path reaches the shell unquoted"
        );
    }

    #[test]
    fn a_command_without_a_file_slot_is_refused() {
        assert!(crate::editor::run_command("nvim", Path::new("/a"), 1).is_err());
    }
}
