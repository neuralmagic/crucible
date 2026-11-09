//! Handing a pack file to the user's editor: a deep link to a line of a file on disk, or the
//! file's text over stdin as a scratch buffer.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// The desktop the app runs on, for the commands that differ between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
}

impl Os {
    pub const HOST: Os = if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Linux
    };

    /// The user's shell: `$SHELL`, else the system's default.
    pub fn login_shell(self) -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| {
                match self {
                    Os::Mac => "/bin/zsh",
                    Os::Linux => "/bin/sh",
                }
                .to_string()
            })
    }

    /// What hands a link or file to the default app.
    fn opener(self) -> &'static str {
        match self {
            Os::Mac => "open",
            Os::Linux => "xdg-open",
        }
    }
}

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

    /// The arguments the command-line tool takes to open `path` at 1-based `line`.
    fn goto(self, path: &Path, line: usize) -> Vec<String> {
        let at = format!("{}:{line}", path.display());
        match self {
            Editor::VsCode | Editor::Cursor => vec!["--goto".to_string(), format!("{at}:1")],
            Editor::Zed => vec![at],
        }
    }

    fn installed(self) -> bool {
        match Os::HOST {
            Os::Mac => bundle(self).is_some(),
            Os::Linux => cli(self).is_some(),
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

    /// What opens files when Settings leaves it to the app: the editor `named` (the user's
    /// `$VISUAL` or `$EDITOR`), in the first terminal installed when it is a terminal editor,
    /// else the first editor app installed. `Err` inside `Ok` is a shell command to run.
    pub fn detect(named: Option<&str>) -> Result<Result<Editor, String>, String> {
        if let Some(named) = named.map(str::trim).filter(|n| !n.is_empty()) {
            if let Some(app) = from_command(named) {
                return Ok(Ok(app));
            }
            let terminal = Terminal::ALL
                .into_iter()
                .find(|t| t.installed(Os::HOST))
                .ok_or_else(|| {
                    format!(
                        "{named} needs a terminal and none was found. Set an editor command in \
                         Settings, like `{EXAMPLE}`."
                    )
                })?;
            return Ok(Err(terminal.command(Os::HOST, named)));
        }
        [Editor::Cursor, Editor::Zed, Editor::VsCode]
            .into_iter()
            .find(|editor| editor.installed())
            .map(Ok)
            .ok_or_else(|| "No editor found. Pick one in Settings.".to_string())
    }
}

/// A terminal app a terminal editor can be opened in, in the order they are tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Ghostty,
    Kitty,
    WezTerm,
    /// macOS's own, always there.
    Apple,
    /// GNOME's, Fedora's default.
    Ptyxis,
}

impl Terminal {
    pub const ALL: [Terminal; 5] = [
        Terminal::Ghostty,
        Terminal::Kitty,
        Terminal::WezTerm,
        Terminal::Apple,
        Terminal::Ptyxis,
    ];

    fn app(self) -> &'static str {
        match self {
            Terminal::Ghostty => "Ghostty.app",
            Terminal::Kitty => "kitty.app",
            Terminal::WezTerm => "WezTerm.app",
            Terminal::Apple => "Utilities/Terminal.app",
            Terminal::Ptyxis => "Ptyxis.app",
        }
    }

    /// Its program on Linux.
    fn program(self) -> &'static str {
        match self {
            Terminal::Ghostty => "ghostty",
            Terminal::Kitty => "kitty",
            Terminal::WezTerm => "wezterm",
            Terminal::Apple => "Terminal",
            Terminal::Ptyxis => "ptyxis",
        }
    }

    fn installed(self, os: Os) -> bool {
        match os {
            Os::Mac => {
                self == Terminal::Apple
                    && Path::new("/System/Applications/Utilities/Terminal.app").is_dir()
                    || app_bundle(self.app()).is_some()
            }
            Os::Linux => self != Terminal::Apple && on_path(self.program()).is_some(),
        }
    }

    /// The command that opens `{file}` at `{line}` in `editor`, in a new window of this terminal
    /// in `{dir}`: the terminal opens a script that changes to `{dir}` and runs the editor, which
    /// each of these runs in the user's shell.
    pub fn command(self, os: Os, editor: &str) -> String {
        let open = if editor
            .split_whitespace()
            .next()
            .is_some_and(|program| program.ends_with("hx") || program.ends_with("helix"))
        {
            format!("{editor} {{file}}:{{line}}")
        } else {
            format!("{editor} +{{line}} {{file}}")
        };
        let opener = match (os, self) {
            (Os::Mac, Terminal::Ghostty) => "open -na Ghostty {script}",
            (Os::Mac, Terminal::Kitty) => "open -na kitty {script}",
            (Os::Mac, Terminal::WezTerm) => "open -na WezTerm {script}",
            (Os::Mac, Terminal::Apple) => "open -na Terminal {script}",
            (Os::Mac, Terminal::Ptyxis) => "open -na Ptyxis {script}",
            (Os::Linux, Terminal::Ghostty) => "ghostty -e {script}",
            (Os::Linux, Terminal::Kitty) => "kitty {script}",
            (Os::Linux, Terminal::WezTerm) => "wezterm start -- {script}",
            (Os::Linux, Terminal::Apple | Terminal::Ptyxis) => "ptyxis --new-window -- {script}",
        };
        format!("{opener} {SCRIPT_SPLIT} {open}")
    }
}

/// Splits a command that runs through a script: what opens `{script}` before it, what the
/// script runs after it.
const SCRIPT_SPLIT: &str = "--script--";

/// `$VISUAL`, else `$EDITOR`: from this process, or, for an app opened from Finder that never
/// saw the user's shell setup, from an interactive login shell, which is where shells set them.
pub fn login_editor() -> Option<String> {
    let own = ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()));
    if own.is_some() {
        return own;
    }
    let shell = Os::HOST.login_shell();
    let mut child = Command::new(&shell)
        .args([
            "-l",
            "-i",
            "-c",
            "printf '\\n@@VISUAL=%s\\n@@EDITOR=%s\\n' \"$VISUAL\" \"$EDITOR\"",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    editor_from_shell(&out)
}

/// The editor a login shell's marked output names, `$VISUAL` first.
fn editor_from_shell(out: &str) -> Option<String> {
    let value = |name: &str| {
        out.lines()
            .rev()
            .find_map(|line| line.strip_prefix(&format!("@@{name}=")))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    value("VISUAL").or_else(|| value("EDITOR"))
}

/// An editor command a terminal editor fits.
pub const EXAMPLE: &str = match Os::HOST {
    Os::Mac => "open -na Ghostty {script} --script-- nvim +{line} {file}",
    Os::Linux => "ghostty -e {script} --script-- nvim +{line} {file}",
};

/// `template` with `{file}`, `{line}` and `{dir}` filled in, the paths quoted for `sh`.
pub fn command_line(template: &str, path: &Path, line: usize, dir: &Path) -> String {
    template
        .replace("{file}", &shell_quote(&path.to_string_lossy()))
        .replace("{dir}", &shell_quote(&dir.to_string_lossy()))
        .replace("{line}", &line.to_string())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Run an editor command for `path` at `line` through `sh` in `dir`, the pack's directory, with
/// the usual tool directories on `PATH`: an app opened from Finder starts with only the system
/// ones.
pub fn run_command(template: &str, path: &Path, line: usize, dir: &Path) -> Result<(), String> {
    if !template.contains("{file}") {
        return Err("The editor command needs {file} where the path goes".to_string());
    }
    let command = match template.split_once(SCRIPT_SPLIT) {
        Some((opener, inside)) => {
            let script = launch_script(&command_line(inside.trim(), path, line, dir), dir)?;
            opener.replace("{script}", &shell_quote(&script.to_string_lossy()))
        }
        None => command_line(template, path, line, dir),
    };
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
        .arg(command)
        .current_dir(dir)
        .env("PATH", search)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("running the editor command: {e}"))
}

/// An executable script that changes to `dir` and runs `command`, for a terminal to open.
fn launch_script(command: &str, dir: &Path) -> Result<PathBuf, String> {
    let scripts = crate::httpcache::cache_root()
        .ok_or("no cache directory for the editor script")?
        .join("open");
    std::fs::create_dir_all(&scripts)
        .map_err(|e| format!("creating {}: {e}", scripts.display()))?;
    let path = scripts.join("editor.command");
    let body = format!(
        "#!/bin/sh\ncd {} && exec {command}\n",
        shell_quote(&dir.to_string_lossy())
    );
    std::fs::write(&path, body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("making {} executable: {e}", path.display()))?;
    Ok(path)
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
    app_bundle(editor.app())
}

/// `app` in /Applications or ~/Applications.
fn app_bundle(app: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        Some(PathBuf::from("/Applications")),
        home.map(|h| h.join("Applications")),
    ]
    .into_iter()
    .flatten()
    .map(|dir| dir.join(app))
    .find(|app| app.is_dir())
}

fn cli(editor: Editor) -> Option<PathBuf> {
    let (name, inside) = editor.cli();
    on_path(name).or_else(|| Some(bundle(editor)?.join(inside)).filter(|path| path.is_file()))
}

/// `name` in a directory of `$PATH`, or where Homebrew and local installs put it.
fn on_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
        ])
        .chain(home.map(|home| home.join(".local/bin")))
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Open `path` at `line` in `editor`: by its deep link on macOS, by its command-line tool on
/// Linux, where the editors register no link handler.
pub fn open_file(editor: Editor, path: &Path, line: usize) -> Result<(), String> {
    let mut command = match Os::HOST {
        Os::Mac => {
            let mut open = Command::new(Os::Mac.opener());
            open.arg(editor.link(path, line));
            open
        }
        Os::Linux => {
            let program = cli(editor).ok_or_else(|| {
                format!("{}'s command-line tool is not installed", editor.label())
            })?;
            let mut run = Command::new(program);
            run.args(editor.goto(path, line));
            run
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("opening {}: {e}", editor.label()))
}

/// Open `url` in the default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    Command::new(Os::HOST.opener())
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

    use crate::editor::{Editor, Os, Terminal, command_line, editor_from_shell, from_command};

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
                "ghostty --working-directory={dir} -e nvim +{line} {file}",
                Path::new("/Users/me/my packs/it's.star"),
                12,
                Path::new("/Users/me/my packs"),
            ),
            "ghostty --working-directory='/Users/me/my packs' -e nvim +12 \
             '/Users/me/my packs/it'\\''s.star'"
        );
        assert_eq!(
            command_line(
                "nvim --remote {file}",
                Path::new("/a/$(rm -rf ~)"),
                1,
                Path::new("/a")
            ),
            "nvim --remote '/a/$(rm -rf ~)'",
            "nothing in the path reaches the shell unquoted"
        );
    }

    #[test]
    fn a_terminal_editor_opens_in_a_new_terminal_window_through_a_script() {
        assert_eq!(
            Terminal::Ghostty.command(Os::Mac, "nvim"),
            "open -na Ghostty {script} --script-- nvim +{line} {file}"
        );
        assert_eq!(
            Terminal::Apple.command(Os::Mac, "hx"),
            "open -na Terminal {script} --script-- hx {file}:{line}"
        );
        assert_eq!(
            Terminal::Ghostty.command(Os::Linux, "nvim"),
            "ghostty -e {script} --script-- nvim +{line} {file}"
        );
        assert_eq!(
            Terminal::WezTerm.command(Os::Linux, "nvim"),
            "wezterm start -- {script} --script-- nvim +{line} {file}"
        );
        assert_eq!(
            Terminal::Ptyxis.command(Os::Linux, "hx"),
            "ptyxis --new-window -- {script} --script-- hx {file}:{line}"
        );
        assert!(!Terminal::Apple.installed(Os::Linux));
    }

    #[test]
    fn on_linux_an_editor_opens_by_its_command_line_tool() {
        let path = Path::new("/home/me/my packs/workflow.star");
        assert_eq!(
            Editor::VsCode.goto(path, 12),
            ["--goto", "/home/me/my packs/workflow.star:12:1"]
        );
        assert_eq!(
            Editor::Cursor.goto(path, 1),
            ["--goto", "/home/me/my packs/workflow.star:1:1"]
        );
        assert_eq!(
            Editor::Zed.goto(path, 3),
            ["/home/me/my packs/workflow.star:3"]
        );
    }

    #[test]
    fn the_launch_script_changes_to_the_pack_and_runs_the_editor() {
        let dir = Path::new("/Users/me/my packs/demo");
        let script = crate::editor::launch_script(
            &command_line("nvim +{line} {file}", &dir.join("workflow.star"), 7, dir),
            dir,
        )
        .expect("script");
        let body = std::fs::read_to_string(&script).expect("read");
        assert_eq!(
            body,
            "#!/bin/sh\ncd '/Users/me/my packs/demo' && exec nvim +7 \
             '/Users/me/my packs/demo/workflow.star'\n"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&script)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable");
    }

    #[test]
    fn a_named_app_editor_is_used_as_an_app() {
        assert_eq!(Editor::detect(Some("code --wait")), Ok(Ok(Editor::VsCode)));
    }

    #[test]
    fn the_login_shell_editor_is_read_past_whatever_the_shell_prints() {
        let noisy = "Welcome to fish\n@@VISUAL=\n@@EDITOR=nvim\n";
        assert_eq!(editor_from_shell(noisy), Some("nvim".to_string()));
        assert_eq!(
            editor_from_shell("@@VISUAL=code -w\n@@EDITOR=nvim\n"),
            Some("code -w".to_string())
        );
        assert_eq!(editor_from_shell("@@VISUAL=\n@@EDITOR=\n"), None);
        assert_eq!(editor_from_shell("nothing marked"), None);
    }

    #[test]
    fn a_command_without_a_file_slot_is_refused() {
        assert!(crate::editor::run_command("nvim", Path::new("/a"), 1, Path::new("/")).is_err());
    }
}
