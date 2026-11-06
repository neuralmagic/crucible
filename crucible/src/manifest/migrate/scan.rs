//! What a pack script reads of its environment, what it sets, and what it runs or imports, read
//! from its text. Nothing here executes a script, so a read built at runtime is reported as
//! computed rather than named.

use regex_lite::Regex;

/// A script language whose environment reads the scanner understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    Shell,
    Python,
    Nu,
}

impl Language {
    /// The language of the pack file at `path`, by extension and then by shebang.
    pub(crate) fn of(path: &str, text: &str) -> Option<Language> {
        let ext = path.rsplit_once('.').map(|(_, ext)| ext);
        match ext {
            Some("sh" | "bash" | "ksh" | "zsh" | "dash") => return Some(Language::Shell),
            Some("py") => return Some(Language::Python),
            Some("nu") => return Some(Language::Nu),
            _ => {}
        }
        let shebang = text.lines().next()?.strip_prefix("#!")?;
        let mut words = shebang.split_whitespace();
        let program = words.next()?.rsplit('/').next()?;
        let interpreter = if program == "env" {
            words.find(|w| !w.starts_with('-'))?
        } else {
            program
        };
        Language::of_interpreter(interpreter)
    }

    /// The language an interpreter program name runs.
    pub(crate) fn of_interpreter(program: &str) -> Option<Language> {
        let program = program.rsplit('/').next().unwrap_or(program);
        match program {
            "sh" | "bash" | "dash" | "ksh" | "zsh" => Some(Language::Shell),
            "nu" => Some(Language::Nu),
            p if p == "python" || p.starts_with("python3") || p == "python2" => {
                Some(Language::Python)
            }
            _ => None,
        }
    }

    /// Scan `text` as this language.
    pub(crate) fn scan(self, text: &str) -> Vec<Event> {
        match self {
            Language::Shell => scan_shell(text),
            Language::Python => scan_python(text),
            Language::Nu => scan_nu(text),
        }
    }
}

/// One thing a script does with its environment or its pack, in the order its text does it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The script reads `name` from its environment, unless it set it first.
    Read { name: String, line: usize },
    /// The script sets `name`; `exported` when a child process it starts receives it.
    Set {
        name: String,
        exported: bool,
        line: usize,
    },
    /// The script marks an earlier assignment of `name` for export.
    Export { name: String, line: usize },
    /// The script reads a variable whose name it computes.
    Computed { line: usize },
    /// A command the script runs.
    Command(SimpleCommand),
    /// A string literal naming a script, which the script may run.
    Path { text: String, line: usize },
    /// A Python import: `level` leading dots, the dotted module, and the names taken from it.
    Import {
        level: usize,
        module: String,
        names: Vec<String>,
        line: usize,
    },
}

/// The words of one command, as a shell would split them before expanding them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SimpleCommand {
    pub words: Vec<Word>,
    pub line: usize,
}

/// One word with its quotes removed. `literal` is false when any part of it is an expansion,
/// whose value only the running shell knows; `text` then keeps the expansion as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Word {
    pub text: String,
    pub literal: bool,
}

/// Variables a shell sets for itself, so a read of one is not a read of the environment.
const SHELL_SETS: &[&str] = &[
    "BASH",
    "BASHOPTS",
    "BASHPID",
    "BASH_ARGC",
    "BASH_ARGV",
    "BASH_COMMAND",
    "BASH_LINENO",
    "BASH_REMATCH",
    "BASH_SOURCE",
    "BASH_SUBSHELL",
    "BASH_VERSINFO",
    "BASH_VERSION",
    "DIRSTACK",
    "EPOCHREALTIME",
    "EPOCHSECONDS",
    "EUID",
    "FUNCNAME",
    "GROUPS",
    "HOSTNAME",
    "HOSTTYPE",
    "IFS",
    "LINENO",
    "MACHTYPE",
    "MAPFILE",
    "OLDPWD",
    "OPTARG",
    "OPTERR",
    "OPTIND",
    "OSTYPE",
    "PIPESTATUS",
    "PPID",
    "PS1",
    "PS2",
    "PS4",
    "PWD",
    "RANDOM",
    "REPLY",
    "SECONDS",
    "SHELLOPTS",
    "SHLVL",
    "SRANDOM",
    "UID",
];

/// Variables nushell sets for itself.
const NU_SETS: &[&str] = &[
    "CMD_DURATION_MS",
    "CURRENT_FILE",
    "ENV_CONVERSIONS",
    "FILE_PWD",
    "LAST_EXIT_CODE",
    "NU_LIB_DIRS",
    "NU_PLUGIN_DIRS",
    "NU_VERSION",
    "PROCESS_PATH",
    "PWD",
    "config",
];

/// Whether `name` is one `language` sets for itself.
pub(crate) fn language_sets(language: Language, name: &str) -> bool {
    match language {
        Language::Shell => SHELL_SETS.contains(&name),
        Language::Nu => NU_SETS.contains(&name),
        Language::Python => false,
    }
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `word` is `NAME=...` or `NAME+=...`, returning the name.
fn assignment_name(word: &str) -> Option<&str> {
    let end = word.find('=')?;
    let name = word[..end].strip_suffix('+').unwrap_or(&word[..end]);
    let name = name.split_once('[').map_or(name, |(n, _)| n);
    let mut chars = name.chars();
    (chars.next().is_some_and(is_name_start) && chars.all(is_name_char)).then_some(name)
}

/// Words that open or close a compound command; the word after one is in command position.
const RESERVED: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "case",
    "esac", "function", "[[", "]]", "time", "coproc",
];

/// Builtins whose arguments name the variables they set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Declares {
    Export,
    Local,
    Read,
    For,
    Getopts,
    Mapfile,
    Printf,
}

impl Declares {
    fn of(program: &str) -> Option<Declares> {
        Some(match program {
            "export" => Declares::Export,
            "local" | "declare" | "typeset" | "readonly" => Declares::Local,
            "read" => Declares::Read,
            "for" | "select" => Declares::For,
            "getopts" => Declares::Getopts,
            "mapfile" | "readarray" => Declares::Mapfile,
            "printf" => Declares::Printf,
            _ => return None,
        })
    }
}

/// A heredoc whose body starts on the next line.
struct Heredoc {
    delimiter: String,
    expands: bool,
    strip_tabs: bool,
}

/// The parts of a POSIX shell script the scanner tracks: words, quoting, expansions, command
/// boundaries, assignments, and heredoc bodies.
struct Shell {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    events: Vec<Event>,
    heredocs: Vec<Heredoc>,
}

/// One command being assembled: its words so far and how the builtin among them reads its
/// arguments.
#[derive(Default)]
struct Assembling {
    words: Vec<Word>,
    line: usize,
    program_seen: bool,
    declares: Option<(Declares, Vec<Word>)>,
}

impl Shell {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Scan commands until the end of input or an unquoted `stop`, which is left unconsumed.
    fn commands(&mut self, stop: Option<char>) {
        let mut cmd = Assembling {
            line: self.line,
            ..Assembling::default()
        };
        loop {
            match self.peek() {
                None => break,
                Some(c) if Some(c) == stop => break,
                Some(' ' | '\t' | '\r') => {
                    self.bump();
                }
                Some('\\') if self.peek_at(1) == Some('\n') => {
                    self.bump();
                    self.bump();
                }
                Some('\n') => {
                    self.bump();
                    self.finish(&mut cmd);
                    self.heredoc_bodies();
                }
                Some('#') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                Some(';' | '&' | '|' | '(' | ')') => {
                    self.bump();
                    self.finish(&mut cmd);
                }
                Some('<' | '>') => self.redirection(),
                Some(_) => {
                    let word = self.word(stop);
                    self.push_word(&mut cmd, word);
                }
            }
        }
        self.finish(&mut cmd);
    }

    /// Consume a redirection operator and its target; a heredoc operator queues its body.
    fn redirection(&mut self) {
        let first = self.bump();
        let heredoc =
            first == Some('<') && self.peek() == Some('<') && self.peek_at(1) != Some('<');
        while self
            .peek()
            .is_some_and(|c| matches!(c, '<' | '>' | '&' | '|' | '-'))
        {
            self.bump();
        }
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.bump();
        }
        if self
            .peek()
            .is_none_or(|c| matches!(c, '\n' | ';' | '|' | '&'))
        {
            return;
        }
        let start = self.pos;
        let target = self.word(None);
        if heredoc {
            let raw: String = self.chars[start..self.pos].iter().collect();
            let quoted = raw.contains(['\'', '"', '\\']);
            let strip_tabs = self.chars[..start]
                .iter()
                .rev()
                .find(|c| !matches!(c, ' ' | '\t'))
                == Some(&'-');
            self.heredocs.push(Heredoc {
                delimiter: target.text,
                expands: !quoted,
                strip_tabs,
            });
        }
    }

    /// Read the bodies of the heredocs queued on the line just ended.
    fn heredoc_bodies(&mut self) {
        for heredoc in std::mem::take(&mut self.heredocs) {
            loop {
                if self.peek().is_none() {
                    return;
                }
                let start = self.pos;
                let start_line = self.line;
                while self.peek().is_some_and(|c| c != '\n') {
                    self.bump();
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                self.bump();
                let candidate = if heredoc.strip_tabs {
                    text.trim_start_matches('\t')
                } else {
                    text.as_str()
                };
                if candidate == heredoc.delimiter {
                    break;
                }
                if heredoc.expands {
                    let mut body = Shell {
                        chars: text.chars().collect(),
                        pos: 0,
                        line: start_line,
                        events: Vec::new(),
                        heredocs: Vec::new(),
                    };
                    body.expanding_text();
                    self.events.append(&mut body.events);
                }
            }
        }
    }

    /// Scan text in which only `$` and backquote expand, as in a heredoc body.
    fn expanding_text(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    self.bump();
                    self.bump();
                }
                '$' => {
                    self.dollar();
                }
                '`' => self.backquote(),
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Read one word up to an unquoted blank, operator, or `stop`.
    fn word(&mut self, stop: Option<char>) -> Word {
        let mut text = String::new();
        let mut literal = true;
        while let Some(c) = self.peek() {
            if Some(c) == stop
                || matches!(
                    c,
                    ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
                )
            {
                break;
            }
            match c {
                '\'' => {
                    self.bump();
                    while let Some(c) = self.bump() {
                        if c == '\'' {
                            break;
                        }
                        text.push(c);
                    }
                }
                '"' => {
                    self.bump();
                    literal &= self.double_quoted(&mut text);
                }
                '\\' => {
                    self.bump();
                    match self.bump() {
                        Some('\n') | None => {}
                        Some(c) => text.push(c),
                    }
                }
                '$' => {
                    let (raw, expanded) = self.dollar();
                    literal &= !expanded;
                    text.push_str(&raw);
                }
                '`' => {
                    literal = false;
                    self.backquote();
                    text.push_str("`...`");
                }
                c => {
                    self.bump();
                    text.push(c);
                }
            }
        }
        Word { text, literal }
    }

    /// The inside of a double-quoted string, after its opening quote. Returns whether nothing in
    /// it expanded.
    fn double_quoted(&mut self, text: &mut String) -> bool {
        let mut literal = true;
        while let Some(c) = self.peek() {
            match c {
                '"' => {
                    self.bump();
                    break;
                }
                '\\' => {
                    self.bump();
                    match self.bump() {
                        Some(c @ ('$' | '`' | '"' | '\\')) => text.push(c),
                        Some('\n') | None => {}
                        Some(c) => {
                            text.push('\\');
                            text.push(c);
                        }
                    }
                }
                '$' => {
                    let (raw, expanded) = self.dollar();
                    literal &= !expanded;
                    text.push_str(&raw);
                }
                '`' => {
                    literal = false;
                    self.backquote();
                    text.push_str("`...`");
                }
                c => {
                    self.bump();
                    text.push(c);
                }
            }
        }
        literal
    }

    fn backquote(&mut self) {
        self.bump();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '`' {
                break;
            }
            if c == '\\' {
                self.bump();
            }
            self.bump();
        }
        let inner: String = self.chars[start..self.pos].iter().collect();
        self.bump();
        let mut sub = Shell {
            chars: inner.chars().collect(),
            pos: 0,
            line: self.line,
            events: Vec::new(),
            heredocs: Vec::new(),
        };
        sub.commands(None);
        self.events.append(&mut sub.events);
    }

    /// An expansion starting at `$`. Returns its text as written and whether it expands at all.
    fn dollar(&mut self) -> (String, bool) {
        let start = self.pos;
        let line = self.line;
        self.bump();
        let expanded = match self.peek() {
            Some('(') if self.peek_at(1) == Some('(') => {
                self.bump();
                self.bump();
                self.arithmetic();
                true
            }
            Some('(') => {
                self.bump();
                self.commands(Some(')'));
                self.bump();
                true
            }
            Some('{') => {
                self.bump();
                self.braced(line);
                true
            }
            Some('\'') => {
                self.bump();
                while let Some(c) = self.bump() {
                    match c {
                        '\\' => {
                            self.bump();
                        }
                        '\'' => break,
                        _ => {}
                    }
                }
                false
            }
            Some(c) if is_name_start(c) => {
                let name = self.name();
                self.events.push(Event::Read { name, line });
                true
            }
            Some(c)
                if c.is_ascii_digit() || matches!(c, '@' | '*' | '#' | '?' | '$' | '!' | '-') =>
            {
                self.bump();
                true
            }
            _ => false,
        };
        let raw: String = self.chars[start..self.pos].iter().collect();
        (raw, expanded)
    }

    fn name(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek().filter(|c| is_name_char(*c)) {
            name.push(c);
            self.bump();
        }
        name
    }

    /// `${...}` after its opening brace: the parameter, then any operator word up to the
    /// matching close brace.
    fn braced(&mut self, line: usize) {
        let indirect = match self.peek() {
            Some('!') if self.peek_at(1).is_some_and(is_name_start) => {
                self.bump();
                true
            }
            Some('#') if self.peek_at(1).is_some_and(is_name_start) => {
                self.bump();
                false
            }
            _ => false,
        };
        if self.peek().is_some_and(is_name_start) {
            let name = self.name();
            if indirect {
                self.events.push(Event::Computed { line });
            }
            let assigns = matches!(
                (self.peek(), self.peek_at(1)),
                (Some(':'), Some('=')) | (Some('='), _)
            );
            self.events.push(Event::Read {
                name: name.clone(),
                line,
            });
            if assigns {
                self.events.push(Event::Set {
                    name,
                    exported: false,
                    line,
                });
            }
        }
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                '}' if depth == 0 => {
                    self.bump();
                    return;
                }
                '}' => {
                    depth -= 1;
                    self.bump();
                }
                '{' => {
                    depth += 1;
                    self.bump();
                }
                '\\' => {
                    self.bump();
                    self.bump();
                }
                '\'' => {
                    self.bump();
                    while self.bump().is_some_and(|c| c != '\'') {}
                }
                '"' => {
                    self.bump();
                    let mut ignored = String::new();
                    self.double_quoted(&mut ignored);
                }
                '$' => {
                    self.dollar();
                }
                '`' => self.backquote(),
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// `$((...))` after its opening parens. Bare names in it are variables, but an arithmetic
    /// expansion reading one from the environment is rare enough to take only `$NAME` forms.
    fn arithmetic(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                ')' if depth == 0 && self.peek_at(1) == Some(')') => {
                    self.bump();
                    self.bump();
                    return;
                }
                '(' => {
                    depth += 1;
                    self.bump();
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    self.bump();
                }
                '$' => {
                    self.dollar();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Add a word to the command being assembled, recording what an assignment or a
    /// declaring builtin sets.
    fn push_word(&mut self, cmd: &mut Assembling, word: Word) {
        if cmd.words.is_empty() && !cmd.program_seen {
            cmd.line = self.line;
        }
        if !cmd.program_seen {
            if RESERVED.contains(&word.text.as_str()) && word.literal {
                return;
            }
            if let Some(name) = assignment_name(&word.text) {
                self.events.push(Event::Set {
                    name: name.to_owned(),
                    exported: false,
                    line: self.line,
                });
                return;
            }
            cmd.program_seen = true;
            if word.literal
                && let Some(declares) = Declares::of(&word.text)
            {
                cmd.declares = Some((declares, Vec::new()));
            }
            cmd.words.push(word);
            return;
        }
        if let Some((declares, args)) = &mut cmd.declares {
            let declares = *declares;
            args.push(word.clone());
            self.declared_by(declares, args);
        }
        cmd.words.push(word);
    }

    /// Record what a declaring builtin's newest argument sets.
    fn declared_by(&mut self, declares: Declares, args: &[Word]) {
        let Some(word) = args.last() else {
            return;
        };
        let line = self.line;
        let option = |w: &Word| w.text.starts_with('-');
        let previous = args.len().checked_sub(2).and_then(|i| args.get(i));
        let set = |name: &str, exported: bool| Event::Set {
            name: name.to_owned(),
            exported,
            line,
        };
        let valid = |name: &str| {
            let mut c = name.chars();
            c.next().is_some_and(is_name_start) && c.all(is_name_char)
        };
        match declares {
            Declares::Export | Declares::Local => {
                if option(word) {
                    return;
                }
                let exported = declares == Declares::Export;
                if let Some(name) = assignment_name(&word.text) {
                    self.events.push(set(name, exported));
                } else if valid(&word.text) {
                    self.events.push(if exported {
                        Event::Export {
                            name: word.text.clone(),
                            line,
                        }
                    } else {
                        set(&word.text, false)
                    });
                }
            }
            Declares::Read => {
                let takes_value = previous.is_some_and(|p| {
                    matches!(p.text.as_str(), "-p" | "-d" | "-n" | "-N" | "-t" | "-u")
                });
                if !option(word) && !takes_value && valid(&word.text) {
                    self.events.push(set(&word.text, false));
                }
            }
            Declares::For => {
                if args.len() == 1 && valid(&word.text) {
                    self.events.push(set(&word.text, false));
                }
            }
            Declares::Getopts => {
                if args.len() == 2 && valid(&word.text) {
                    self.events.push(set(&word.text, false));
                }
            }
            Declares::Mapfile => {
                if !option(word) && valid(&word.text) {
                    self.events.push(set(&word.text, false));
                }
            }
            Declares::Printf => {
                if previous.is_some_and(|p| p.text == "-v") && valid(&word.text) {
                    self.events.push(set(&word.text, false));
                }
            }
        }
    }

    fn finish(&mut self, cmd: &mut Assembling) {
        let done = std::mem::take(cmd);
        cmd.line = self.line;
        if !done.words.is_empty() {
            self.events.push(Event::Command(SimpleCommand {
                words: done.words,
                line: done.line,
            }));
        }
    }
}

/// Scan a POSIX shell script. A shell script often embeds Python (`python3 -c`, a heredoc fed
/// to `python3 -`), so its text is scanned for Python's environment reads too.
pub(crate) fn scan_shell(text: &str) -> Vec<Event> {
    let mut shell = Shell {
        chars: text.chars().collect(),
        pos: 0,
        line: 1,
        events: Vec::new(),
        heredocs: Vec::new(),
    };
    shell.commands(None);
    let mut events = shell.events;
    events.extend(
        scan_python(text)
            .into_iter()
            .filter(|e| matches!(e, Event::Read { .. } | Event::Computed { .. })),
    );
    events.sort_by_key(event_line);
    events
}

fn event_line(event: &Event) -> usize {
    match event {
        Event::Read { line, .. }
        | Event::Set { line, .. }
        | Event::Computed { line }
        | Event::Import { line, .. }
        | Event::Path { line, .. }
        | Event::Export { line, .. } => *line,
        Event::Command(cmd) => cmd.line,
    }
}

/// The patterns a Python scan matches. Built per scan: patterns are small, and a pack holds few
/// scripts.
struct PythonPatterns {
    subscript: Regex,
    call: Regex,
    membership: Regex,
    putenv: Regex,
    computed_subscript: Regex,
    computed_call: Regex,
    import: Regex,
    from_import: Regex,
    script_path: Regex,
}

const NAME_LITERAL: &str = r#"(?:"([A-Za-z_][A-Za-z0-9_]*)"|'([A-Za-z_][A-Za-z0-9_]*)')"#;

impl PythonPatterns {
    fn new() -> Option<PythonPatterns> {
        let environ = r"\b(?:os\.)?environ";
        Some(PythonPatterns {
            subscript: Regex::new(&format!(r"{environ}\s*\[\s*{NAME_LITERAL}\s*\]")).ok()?,
            call: Regex::new(&format!(
                r"(?:{environ}\.(?:get|setdefault|pop)|\b(?:os\.)?getenv)\s*\(\s*{NAME_LITERAL}"
            ))
            .ok()?,
            membership: Regex::new(&format!(r"{NAME_LITERAL}\s+(?:not\s+)?in\s+{environ}\b"))
                .ok()?,
            putenv: Regex::new(&format!(r"\bos\.putenv\s*\(\s*{NAME_LITERAL}")).ok()?,
            computed_subscript: Regex::new(&format!(r#"{environ}\s*\[\s*[^"'\s\]]"#)).ok()?,
            computed_call: Regex::new(&format!(
                r#"(?:{environ}\.(?:get|setdefault|pop)|\b(?:os\.)?getenv)\s*\(\s*[^"'\s)]"#
            ))
            .ok()?,
            import: Regex::new(r"^\s*import\s+(.+)$").ok()?,
            from_import: Regex::new(r"^\s*from\s+(\.*)([A-Za-z_][A-Za-z0-9_.]*)?\s+import\s+(.+)$")
                .ok()?,
            script_path: Regex::new(r#"["']([A-Za-z0-9_./-]+\.(?:sh|bash|py|nu))["']"#).ok()?,
        })
    }
}

/// Byte offsets where each line starts, for turning a match offset into a line number.
struct Lines(Vec<usize>);

impl Lines {
    fn of(text: &str) -> Lines {
        Lines(
            std::iter::once(0)
                .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                .collect(),
        )
    }

    fn line(&self, offset: usize) -> usize {
        self.0.partition_point(|start| *start <= offset)
    }
}

fn literal_name(caps: &regex_lite::Captures<'_>, first: usize) -> Option<String> {
    caps.get(first)
        .or_else(|| caps.get(first + 1))
        .map(|m| m.as_str().to_owned())
}

/// Scan a Python module.
pub(crate) fn scan_python(text: &str) -> Vec<Event> {
    let Some(p) = PythonPatterns::new() else {
        return Vec::new();
    };
    let lines = Lines::of(text);
    let mut found: Vec<(usize, Event)> = Vec::new();
    for caps in p.subscript.captures_iter(text) {
        let (Some(whole), Some(name)) = (caps.get(0), literal_name(&caps, 1)) else {
            continue;
        };
        let line = lines.line(whole.start());
        let rest = text[whole.end()..].trim_start_matches([' ', '\t']);
        let assigned = rest.starts_with('=') && !rest.starts_with("==");
        found.push((
            whole.start(),
            if assigned {
                Event::Set {
                    name,
                    exported: true,
                    line,
                }
            } else {
                Event::Read { name, line }
            },
        ));
    }
    for (pattern, set) in [(&p.call, false), (&p.membership, false), (&p.putenv, true)] {
        for caps in pattern.captures_iter(text) {
            let (Some(whole), Some(name)) = (caps.get(0), literal_name(&caps, 1)) else {
                continue;
            };
            let line = lines.line(whole.start());
            found.push((
                whole.start(),
                if set {
                    Event::Set {
                        name,
                        exported: true,
                        line,
                    }
                } else {
                    Event::Read { name, line }
                },
            ));
        }
    }
    for caps in p.script_path.captures_iter(text) {
        if let Some(path) = caps.get(1) {
            found.push((
                path.start(),
                Event::Path {
                    text: path.as_str().to_owned(),
                    line: lines.line(path.start()),
                },
            ));
        }
    }
    for pattern in [&p.computed_subscript, &p.computed_call] {
        for m in pattern.find_iter(text) {
            found.push((
                m.start(),
                Event::Computed {
                    line: lines.line(m.start()),
                },
            ));
        }
    }
    let mut offset = 0;
    let mut source_lines = text.split_inclusive('\n').peekable();
    while let Some(raw) = source_lines.next() {
        let start = offset;
        offset += raw.len();
        let code = raw.split('#').next().unwrap_or("").trim_end();
        let line = lines.line(start);
        if let Some(caps) = p.from_import.captures(code) {
            let level = caps.get(1).map_or(0, |m| m.as_str().len());
            let module = caps.get(2).map_or("", |m| m.as_str()).to_owned();
            let mut names_text = caps.get(3).map_or("", |m| m.as_str()).to_owned();
            if names_text.trim_start().starts_with('(') && !names_text.contains(')') {
                for more in source_lines.by_ref() {
                    offset += more.len();
                    let more = more.split('#').next().unwrap_or("");
                    names_text.push_str(more);
                    if more.contains(')') {
                        break;
                    }
                }
            }
            let names = names_text
                .split(',')
                .filter_map(|n| {
                    n.trim_matches(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '\\')
                        .split_whitespace()
                        .next()
                        .map(str::to_owned)
                })
                .filter(|n| n != "*")
                .collect();
            found.push((
                start,
                Event::Import {
                    level,
                    module,
                    names,
                    line,
                },
            ));
        } else if let Some(caps) = p.import.captures(code) {
            for module in caps.get(1).map_or("", |m| m.as_str()).split(',') {
                if let Some(module) = module.split_whitespace().next() {
                    found.push((
                        start,
                        Event::Import {
                            level: 0,
                            module: module.to_owned(),
                            names: Vec::new(),
                            line,
                        },
                    ));
                }
            }
        }
    }
    found.sort_by_key(|(offset, _)| *offset);
    found.into_iter().map(|(_, event)| event).collect()
}

/// Scan a nushell script: `$env.NAME` reads and assignments, and each line's words as a
/// command so a script it runs or sources is followed.
pub(crate) fn scan_nu(text: &str) -> Vec<Event> {
    let (Ok(field), Ok(quoted), Ok(get), Ok(computed)) = (
        Regex::new(r"\$env\.([A-Za-z_][A-Za-z0-9_]*)"),
        Regex::new(&format!(r"\$env\.\s*{NAME_LITERAL}")),
        Regex::new(r#"\$env\s*\|\s*get\s+(?:-[a-z]+\s+)*["']?([A-Za-z_][A-Za-z0-9_]*)"#),
        Regex::new(r"\$env\s*\|\s*get\s+(?:-[a-z]+\s+)*\$"),
    ) else {
        return Vec::new();
    };
    let lines = Lines::of(text);
    let mut found: Vec<(usize, Event)> = Vec::new();
    for caps in field.captures_iter(text) {
        let (Some(whole), Some(name)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let line = lines.line(whole.start());
        let rest = text[whole.end()..].trim_start_matches([' ', '\t']);
        let name = name.as_str().to_owned();
        found.push((
            whole.start(),
            if rest.starts_with('=') && !rest.starts_with("==") {
                Event::Set {
                    name,
                    exported: true,
                    line,
                }
            } else {
                Event::Read { name, line }
            },
        ));
    }
    for caps in quoted.captures_iter(text) {
        if let (Some(whole), Some(name)) = (caps.get(0), literal_name(&caps, 1)) {
            found.push((
                whole.start(),
                Event::Read {
                    name,
                    line: lines.line(whole.start()),
                },
            ));
        }
    }
    for caps in get.captures_iter(text) {
        if let (Some(whole), Some(name)) = (caps.get(0), caps.get(1)) {
            found.push((
                whole.start(),
                Event::Read {
                    name: name.as_str().to_owned(),
                    line: lines.line(whole.start()),
                },
            ));
        }
    }
    for m in computed.find_iter(text) {
        found.push((
            m.start(),
            Event::Computed {
                line: lines.line(m.start()),
            },
        ));
    }
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let code = raw.split('#').next().unwrap_or("");
        let words: Vec<Word> = code
            .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '(' | ')'))
            .map(|w| w.trim_start_matches('^').trim_matches(['"', '\'', '`']))
            .filter(|w| !w.is_empty())
            .map(|w| Word {
                text: w.to_owned(),
                literal: !w.contains('$'),
            })
            .collect();
        if !words.is_empty() {
            found.push((
                start,
                Event::Command(SimpleCommand {
                    words,
                    line: lines.line(start),
                }),
            ));
        }
    }
    found.sort_by_key(|(offset, _)| *offset);
    found.into_iter().map(|(_, event)| event).collect()
}

#[cfg(test)]
mod tests {
    use crate::manifest::migrate::scan::{Event, Language, scan_nu, scan_python, scan_shell};

    fn reads(events: &[Event]) -> Vec<(String, usize)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Read { name, line } => Some((name.clone(), *line)),
                _ => None,
            })
            .collect()
    }

    fn sets(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Set { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    fn commands(events: &[Event]) -> Vec<Vec<String>> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Command(c) => Some(c.words.iter().map(|w| w.text.clone()).collect()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_language_comes_from_the_extension_and_then_the_shebang() {
        assert_eq!(Language::of("a/b.sh", ""), Some(Language::Shell));
        assert_eq!(Language::of("x.py", ""), Some(Language::Python));
        assert_eq!(Language::of("x.nu", ""), Some(Language::Nu));
        assert_eq!(
            Language::of("bin/run", "#!/usr/bin/env bash\n"),
            Some(Language::Shell)
        );
        assert_eq!(
            Language::of("bin/run", "#!/usr/bin/env -S python3 -u\n"),
            Some(Language::Python)
        );
        assert_eq!(
            Language::of("bin/run", "#!/bin/sh\n"),
            Some(Language::Shell)
        );
        assert_eq!(Language::of("README.md", "# hi\n"), None);
        assert_eq!(Language::of("tool", "\u{7f}ELF"), None);
    }

    #[test]
    fn shell_reads_every_expansion_form_and_skips_single_quotes_and_specials() {
        let events = scan_shell(
            "echo $A ${B} \"${C:-x}\" '$NOT' ${#D} \\$ESCAPED $1 $@ $? $$\n\
             x=\"$(printf %s \"$E\")\" y=`echo $F`\n\
             echo ${G:-${H}} $((I + $J))\n",
        );
        let names: Vec<String> = reads(&events).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["A", "B", "C", "D", "E", "F", "G", "H", "J"]);
    }

    #[test]
    fn shell_assignments_and_declaring_builtins_set_names_in_order() {
        let events = scan_shell(
            "ROOT=\"${ROOT:-/srv}\"\n\
             export TOKEN=abc\n\
             local scratch\n\
             for item in a b; do echo $item; done\n\
             read -r -p prompt answer\n\
             getopts ab: opt\n\
             printf -v stamp %s now\n\
             : \"${CACHE:=/tmp/c}\"\n",
        );
        assert_eq!(
            sets(&events),
            [
                "ROOT", "TOKEN", "scratch", "item", "answer", "opt", "stamp", "CACHE"
            ]
        );
        let roots: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Read { name, .. } | Event::Set { name, .. } if name == "ROOT"))
            .collect();
        assert!(
            matches!(roots.as_slice(), [Event::Read { .. }, Event::Set { .. }]),
            "{roots:?}"
        );
    }

    #[test]
    fn shell_commands_split_on_operators_and_keep_quoted_words_whole() {
        let events = scan_shell(
            "set -eu\ncd \"$here\" && python3 'scripts/a b.py' --x | tee out; ./run.sh\n",
        );
        assert_eq!(
            commands(&events),
            [
                vec!["set", "-eu"],
                vec!["cd", "$here"],
                vec!["python3", "scripts/a b.py", "--x"],
                vec!["tee", "out"],
                vec!["./run.sh"],
            ]
        );
        let Some(Event::Command(cd)) = events
            .iter()
            .find(|e| matches!(e, Event::Command(c) if c.words[0].text == "cd"))
        else {
            panic!("no cd command: {events:?}");
        };
        assert!(!cd.words[1].literal);
    }

    #[test]
    fn a_quoted_heredoc_is_literal_and_an_unquoted_one_expands() {
        let events = scan_shell("cat <<'EOF'\n$QUOTED\nEOF\ncat <<EOF\n$OPEN\nEOF\necho $AFTER\n");
        let names: Vec<String> = reads(&events).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["OPEN", "AFTER"]);
    }

    #[test]
    fn python_embedded_in_a_shell_script_is_read_too() {
        let events =
            scan_shell("python3 - <<'EOF'\nimport os\nprint(os.environ[\"EMBEDDED\"])\nEOF\n");
        assert_eq!(reads(&events), [("EMBEDDED".to_owned(), 3)]);
    }

    #[test]
    fn a_comment_is_not_code() {
        let events = scan_shell("# uses $COMMENTED\necho ok # and $TRAILING\n");
        assert!(reads(&events).is_empty(), "{events:?}");
    }

    #[test]
    fn python_reads_sets_and_computed_names_are_told_apart() {
        let events = scan_python(
            "import os\nfrom os import environ, getenv\n\
             a = os.environ[\"A\"]\n\
             b = os.environ.get('B', 'x')\n\
             c = getenv(\"C\")\n\
             if \"D\" in os.environ: pass\n\
             os.environ[\"E\"] = \"set\"\n\
             e = os.environ[\"E\"]\n\
             f = environ.setdefault(\"F\", \"1\")\n\
             g = os.environ[name]\n\
             h = os.environ.get(f\"X_{n}\")\n\
             same = os.environ[\"A\"] == \"y\"\n",
        );
        assert_eq!(
            reads(&events),
            [
                ("A".to_owned(), 3),
                ("B".to_owned(), 4),
                ("C".to_owned(), 5),
                ("D".to_owned(), 6),
                ("E".to_owned(), 8),
                ("F".to_owned(), 9),
                ("A".to_owned(), 12),
            ]
        );
        assert_eq!(sets(&events), ["E"]);
        let computed: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                Event::Computed { line } => Some(*line),
                _ => None,
            })
            .collect();
        assert_eq!(computed, [10, 11]);
    }

    #[test]
    fn python_imports_name_their_module_level_and_names() {
        let events = scan_python(
            "import os, json as j\nfrom . import util\nfrom ..pkg.mod import (\n    a,\n    b as c,\n)\nfrom taskio import read  # x\n",
        );
        let imports: Vec<(usize, String, Vec<String>)> = events
            .into_iter()
            .filter_map(|e| match e {
                Event::Import {
                    level,
                    module,
                    names,
                    ..
                } => Some((level, module, names)),
                _ => None,
            })
            .collect();
        assert_eq!(
            imports,
            [
                (0, "os".to_owned(), vec![]),
                (0, "json".to_owned(), vec![]),
                (1, String::new(), vec!["util".to_owned()]),
                (
                    2,
                    "pkg.mod".to_owned(),
                    vec!["a".to_owned(), "b".to_owned()]
                ),
                (0, "taskio".to_owned(), vec!["read".to_owned()]),
            ]
        );
    }

    #[test]
    fn nu_reads_env_fields_and_skips_assignments() {
        let events = scan_nu(
            "let a = $env.ALPHA\n$env.BETA = \"x\"\nlet b = $env.BETA\nlet c = ($env | get GAMMA)\nlet d = $env.DELTA?\n^python3 tools/x.py\n",
        );
        assert_eq!(
            reads(&events),
            [
                ("ALPHA".to_owned(), 1),
                ("BETA".to_owned(), 3),
                ("GAMMA".to_owned(), 4),
                ("DELTA".to_owned(), 5),
            ]
        );
        assert_eq!(sets(&events), ["BETA"]);
        assert!(commands(&events).contains(&vec!["python3".to_owned(), "tools/x.py".to_owned()]));
    }
}
