//! Drafting with an agent: a coding agent in a sandbox that edits, launches and watches one
//! draft through the controller's MCP tools, its output streamed into the Pack tab.
//!
//! ```text
//! Pack tab ── prompt ──▶ Session ── exec ──▶ sandbox: the user's harness
//!    ▲                     │                    │ MCP
//!    └── entries, saves ───┘                  relay ──▶ controller /mcp, as the user
//! ```
//!
//! The session is the only part that knows where the agent runs. A hosted sandbox would be a
//! second one publishing the same [`Update`]s, with the controller's `/mcp` reached directly.

#[cfg(not(target_arch = "wasm32"))]
mod relay;
#[cfg(not(target_arch = "wasm32"))]
mod sandbox;
#[cfg(not(target_arch = "wasm32"))]
pub mod session;
#[cfg(target_arch = "wasm32")]
#[path = "agent/session_web.rs"]
pub mod session;

use rune::TextEdit;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::App;
use crate::agent::session::Session;
use crate::logline::LogLine;

/// Entries the panel keeps; older ones are dropped.
const KEPT: usize = 4000;

/// The stream a harness writes on stdout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    #[default]
    Claude,
    Codex,
    Opencode,
    Pi,
    Text,
}

/// `[agent]` in the config file: the image, the harness in it and how to start it.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub image: String,
    /// OpenShell providers attached to the sandbox: the harness's own credentials.
    pub providers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub format: Format,
    /// The harness's argv, with `{prompt}`, `{mcp_url}`, `{mcp_token}` and `{model}` filled
    /// in. Empty runs Claude Code.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The executables that may reach the controller's tools, as the kernel resolves them.
    pub binaries: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            image: "localhost/crucible-agent".into(),
            providers: vec!["claude-sub".into()],
            model: None,
            format: Format::Claude,
            command: Vec::new(),
            binaries: vec![
                "/usr/local/bin/node".into(),
                "/usr/local/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe".into(),
            ],
        }
    }
}

/// Where a session's agent reaches the controller's tools.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub url: String,
    pub token: String,
}

impl Config {
    /// The harness's argv for one prompt; `follow_up` continues the conversation where the
    /// harness can.
    pub fn argv(
        &self,
        prompt: &str,
        endpoint: &Endpoint,
        follow_up: bool,
    ) -> Result<Vec<String>, String> {
        let model = self.model.as_deref().unwrap_or_default();
        if !self.command.is_empty() {
            return Ok(self
                .command
                .iter()
                .map(|part| {
                    part.replace("{prompt}", prompt)
                        .replace("{mcp_url}", &endpoint.url)
                        .replace("{mcp_token}", &endpoint.token)
                        .replace("{model}", model)
                })
                .collect());
        }
        if self.format != Format::Claude {
            return Err("Set [agent].command for this harness".into());
        }
        let servers = json!({
            "mcpServers": {
                "crucible": {
                    "type": "http",
                    "url": endpoint.url,
                    "headers": {"Authorization": format!("Bearer {}", endpoint.token)},
                }
            }
        });
        let mut argv: Vec<String> = [
            "claude",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--strict-mcp-config",
            "--mcp-config",
        ]
        .map(str::to_string)
        .into();
        argv.push(servers.to_string());
        argv.extend([
            "--allowedTools".to_string(),
            "mcp__crucible,Read,Glob,Grep".to_string(),
        ]);
        if !model.is_empty() {
            argv.extend(["--model".to_string(), model.to_string()]);
        }
        if follow_up {
            argv.push("--continue".to_string());
        }
        argv.extend(["-p".to_string(), prompt.to_string()]);
        Ok(argv)
    }

    /// Whether a follow-up prompt continues the harness's own conversation.
    pub fn continues(&self) -> bool {
        self.command.is_empty() && self.format == Format::Claude
    }
}

/// What a session works on and under which limits.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub draft: String,
    /// The version the working copy was at when the session began.
    pub version: i64,
    pub config: Config,
    pub max_cost: f64,
    pub max_time: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Starting(String),
    /// Waiting for a prompt.
    Idle,
    Running,
    Failed(String),
}

/// One row of the panel.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Asked(String),
    Note(String),
    Agent(LogLine),
}

/// What a session did since it was last read.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: u64,
    pub state: State,
    pub entries: Vec<Entry>,
    /// Draft saves that landed since the last read.
    pub saved: bool,
}

/// The panel's state for the profile in use.
pub struct Desk {
    pub open: bool,
    pub prompt: TextEdit,
    session: Option<Session>,
    seen: u64,
    pub state: Option<State>,
    pub entries: Vec<Entry>,
    /// A save landed that the working copy has not been pulled to yet.
    pull_due: bool,
}

impl Default for Desk {
    fn default() -> Self {
        Desk {
            open: false,
            prompt: TextEdit::new(""),
            session: None,
            seen: 0,
            state: None,
            entries: Vec::new(),
            pull_due: false,
        }
    }
}

impl Desk {
    /// The draft the session in flight edits.
    pub fn draft(&self) -> Option<&str> {
        self.session.as_ref().map(Session::draft)
    }

    pub fn busy(&self) -> bool {
        matches!(self.state, Some(State::Starting(_) | State::Running))
    }
}

impl App {
    /// Whether this build can run an agent at all.
    pub fn agent_available(&self) -> bool {
        cfg!(not(target_arch = "wasm32"))
    }

    pub fn toggle_agent(&mut self) {
        self.remote.agent.open = !self.remote.agent.open;
    }

    /// Why a prompt cannot be sent now.
    pub fn agent_blocked(&self) -> Option<&'static str> {
        if self.remote.working.is_none() {
            Some("Pull the draft first")
        } else if self.remote.working_edited {
            Some("Save your edits first")
        } else if self.remote.agent.busy() {
            Some("The agent is working")
        } else if self.remote.agent.prompt.text.trim().is_empty() {
            Some("Say what to change")
        } else {
            None
        }
    }

    /// Send the prompt to the draft's session, starting one when there is none.
    pub fn send_agent(&mut self) {
        if self.agent_blocked().is_some() {
            return;
        }
        let Some(working) = self.remote.working.clone() else {
            return;
        };
        let prompt = self.remote.agent.prompt.text.trim().to_string();
        let usable = self.remote.agent.draft() == Some(working.draft.as_str())
            && !matches!(self.remote.agent.state, Some(State::Failed(_)));
        if !usable {
            let spec = Spec {
                draft: working.draft.clone(),
                version: working.base,
                config: self.settings.agent.clone(),
                max_cost: self.settings.drop_max_cost,
                max_time: self.settings.drop_max_time.clone(),
            };
            let session = Session::start(
                self.net.handle(),
                self.remote.target.clone(),
                spec,
                self.waker.clone(),
            );
            let desk = &mut self.remote.agent;
            desk.entries.clear();
            desk.seen = 0;
            desk.pull_due = false;
            desk.state = Some(State::Starting("Starting".into()));
            desk.session = Some(session);
        }
        let desk = &mut self.remote.agent;
        if let Some(session) = &desk.session {
            session.ask(prompt);
            desk.state = Some(match desk.state.take() {
                Some(State::Idle) | None => State::Running,
                Some(other) => other,
            });
        }
        desk.prompt = TextEdit::new("");
    }

    /// Stop the turn in flight, keeping the session.
    pub fn stop_agent(&mut self) {
        if let Some(session) = &self.remote.agent.session {
            session.stop();
        }
    }

    /// End the session and delete its sandbox.
    pub fn end_agent(&mut self) {
        let desk = &mut self.remote.agent;
        desk.session = None;
        desk.state = None;
        desk.pull_due = false;
    }

    /// Take what the session did, and pull the working copy to a save it made.
    pub(crate) fn refresh_agent(&mut self) -> bool {
        let desk = &mut self.remote.agent;
        let update = desk
            .session
            .as_ref()
            .and_then(|session| session.take(desk.seen));
        let changed = update.is_some();
        if let Some(update) = update {
            desk.seen = update.version;
            desk.state = Some(update.state);
            desk.entries.extend(update.entries);
            let over = desk.entries.len().saturating_sub(KEPT);
            desk.entries.drain(..over);
            desk.pull_due |= update.saved;
        }
        if self.remote.agent.pull_due
            && !self.remote.draft_busy
            && !self.remote.working_edited
            && self.draft_target().map(|(draft, _)| draft).as_deref() == self.remote.agent.draft()
        {
            self.remote.agent.pull_due = false;
            self.pull_draft();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::{Config, Endpoint, Format};

    fn endpoint() -> Endpoint {
        Endpoint {
            url: "http://host.openshell.internal:4100/mcp".into(),
            token: "tok".into(),
        }
    }

    #[test]
    fn claude_gets_the_relay_as_its_only_mcp_server() {
        let config = Config {
            model: Some("claude-fable-5-2".into()),
            ..Config::default()
        };
        let argv = config.argv("add a retry", &endpoint(), false).unwrap();
        assert_eq!(argv[0], "claude");
        assert_eq!(argv[argv.len() - 2..], ["-p", "add a retry"]);
        let at = argv.iter().position(|a| a == "--mcp-config").unwrap();
        let servers: serde_json::Value = serde_json::from_str(&argv[at + 1]).unwrap();
        assert_eq!(
            servers["mcpServers"]["crucible"],
            serde_json::json!({
                "type": "http",
                "url": "http://host.openshell.internal:4100/mcp",
                "headers": {"Authorization": "Bearer tok"},
            })
        );
        assert!(argv.contains(&"--strict-mcp-config".to_string()));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--model", "claude-fable-5-2"])
        );
        assert!(!argv.contains(&"--continue".to_string()));
        assert!(!argv.iter().any(|a| a.contains("Bash")));

        let next = config.argv("now the docs", &endpoint(), true).unwrap();
        assert!(next.contains(&"--continue".to_string()));
        let plain = Config::default().argv("x", &endpoint(), false).unwrap();
        assert!(!plain.contains(&"--model".to_string()));
    }

    #[test]
    fn a_custom_command_is_filled_in_and_never_continued() {
        let config = Config {
            format: Format::Text,
            model: Some("glm".into()),
            command: vec![
                "my-agent".into(),
                "--mcp={mcp_url}".into(),
                "--key".into(),
                "{mcp_token}".into(),
                "-m".into(),
                "{model}".into(),
                "{prompt}".into(),
            ],
            ..Config::default()
        };
        assert_eq!(
            config.argv("fix it", &endpoint(), true).unwrap(),
            [
                "my-agent",
                "--mcp=http://host.openshell.internal:4100/mcp",
                "--key",
                "tok",
                "-m",
                "glm",
                "fix it"
            ]
        );
        assert!(!config.continues());
        assert!(Config::default().continues());
    }

    #[test]
    fn another_harness_needs_its_command() {
        let config = Config {
            format: Format::Codex,
            ..Config::default()
        };
        assert_eq!(
            config.argv("x", &endpoint(), false),
            Err("Set [agent].command for this harness".into())
        );
    }

    #[test]
    fn the_config_reads_from_toml_and_refuses_a_typo() {
        let config: Config = toml::from_str(
            "image = \"localhost/mine\"\nproviders = [\"openrouter\"]\nformat = \"opencode\"\n\
             command = [\"opencode\", \"run\", \"{prompt}\"]\nbinaries = [\"/usr/bin/opencode\"]\n",
        )
        .unwrap();
        assert_eq!(config.format, Format::Opencode);
        assert_eq!(config.image, "localhost/mine");
        assert_eq!(config.model, None);
        assert!(toml::from_str::<Config>("imge = \"x\"").is_err());
        let back: Config = toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        assert_eq!(back, Config::default());
    }
}
