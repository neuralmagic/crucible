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

use std::collections::BTreeMap;

use rune::TextEdit;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::App;
use crate::agent::session::Session;
use crate::session::AgentStep;
use crate::workspace::PaneKind;

/// Agent events the panel keeps; older ones are dropped.
const KEPT: usize = 8000;
/// The task name the session's agent events carry, which no pack task can have.
pub const SPEAKER: &str = "@agent";

/// The model Claude Code runs when `[agent]` names none.
const CLAUDE_MODEL: &str = "claude-opus-5-5";

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
    /// Variables set for the harness, beside the controller's.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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
                "/usr/local/bin/crux".into(),
            ],
            env: BTreeMap::new(),
        }
    }
}

/// Where a session's agent reaches the controller, and the token that opens it.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// The controller's base URL as the sandbox sees it; `crux` takes this.
    pub base: String,
    pub token: String,
}

impl Endpoint {
    /// The controller's MCP tools.
    pub fn mcp(&self) -> String {
        format!("{}/mcp", self.base)
    }

    /// What the harness and everything it starts find in their environment: the endpoint as
    /// `crux` reads it, and as a harness with its own MCP config can.
    pub fn env(&self) -> Vec<String> {
        vec![
            format!("CONTROLLER_URL={}", self.base),
            format!("CONTROLLER_API_TOKEN={}", self.token),
            format!("CRUCIBLE_MCP_URL={}", self.mcp()),
            format!("CRUCIBLE_MCP_TOKEN={}", self.token),
        ]
    }
}

/// Where the default image keeps the Claude Code plugin that runs crucible-lsp.
const CLAUDE_PLUGIN: &str = "/usr/local/share/crucible-agent/plugin";

impl Config {
    /// `[agent].env` as `env` takes it.
    pub fn env(&self) -> impl Iterator<Item = String> + '_ {
        self.env
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
    }

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
                        .replace("{mcp_url}", &endpoint.mcp())
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
                    "url": endpoint.mcp(),
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
        argv.extend(
            [
                "--permission-mode",
                "bypassPermissions",
                "--plugin-dir",
                CLAUDE_PLUGIN,
            ]
            .map(str::to_string),
        );
        let model = if model.is_empty() {
            CLAUDE_MODEL
        } else {
            model
        };
        argv.extend(["--model".to_string(), model.to_string()]);
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

/// Something the panel shows beside a turn: what was asked, or what the session did.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Asked {
        turn: u32,
        text: String,
    },
    /// A note from the session during `turn`; 0 is before the first.
    Note {
        turn: u32,
        text: String,
    },
}

impl Entry {
    pub fn turn(&self) -> u32 {
        match self {
            Entry::Asked { turn, .. } | Entry::Note { turn, .. } => *turn,
        }
    }
}

/// What a session did since it was last read.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: u64,
    pub state: State,
    pub entries: Vec<Entry>,
    /// What the agent did, each event's attempt the turn it belongs to.
    pub steps: Vec<AgentStep>,
    /// Draft saves that landed since the last read.
    pub saved: bool,
    /// The agent launched the draft since the last read.
    pub launched: bool,
}

/// The panel's state for the profile in use.
pub struct Desk {
    pub prompt: TextEdit,
    session: Option<Session>,
    seen: u64,
    pub state: Option<State>,
    pub entries: Vec<Entry>,
    pub steps: Vec<AgentStep>,
    /// A prompt sent while the agent worked, held until the turn ends.
    pub queued: Option<String>,
    /// A save landed that the working copy has not been pulled to yet.
    pull_due: bool,
    /// The agent launched the draft, and the window has not moved to that launch yet: the
    /// draft's newest launch when it did.
    launched_past: Option<Option<String>>,
}

impl Default for Desk {
    fn default() -> Self {
        Desk {
            prompt: TextEdit::new(""),
            session: None,
            seen: 0,
            state: None,
            entries: Vec::new(),
            steps: Vec::new(),
            queued: None,
            pull_due: false,
            launched_past: None,
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

    /// Take a prompt typed now: the prompt to run at once, or `None` when the agent is working
    /// and it joined the queue.
    fn submit(&mut self, prompt: String) -> Option<String> {
        if !self.busy() {
            return Some(prompt);
        }
        self.queued = Some(match self.queued.take() {
            Some(queued) => format!("{queued}\n\n{prompt}"),
            None => prompt,
        });
        None
    }

    /// The queued prompt, once the agent is waiting for one.
    fn due(&mut self) -> Option<String> {
        matches!(self.state, Some(State::Idle))
            .then(|| self.queued.take())
            .flatten()
    }

    fn run(&mut self, prompt: String) {
        if let Some(session) = &self.session {
            session.ask(prompt);
            self.state = Some(match self.state.take() {
                Some(State::Idle) | None => State::Running,
                Some(other) => other,
            });
        }
    }
}

impl App {
    /// Whether this build can run an agent at all.
    pub fn agent_available(&self) -> bool {
        cfg!(not(target_arch = "wasm32"))
    }

    pub fn toggle_agent(&mut self) {
        if self.workspace().locate(PaneKind::Agent).is_some() {
            self.shown.retain(|shown| *shown != PaneKind::Agent);
            self.arrange(|layout| layout.close(PaneKind::Agent));
            self.save_layout();
        } else {
            self.show_pane(PaneKind::Agent);
        }
    }

    /// Why a prompt cannot be sent now.
    pub fn agent_blocked(&self) -> Option<&'static str> {
        if self.remote.working.is_none() {
            Some("Pull the draft first")
        } else if self.remote.working_edited {
            Some("Save your edits first")
        } else if self.remote.agent.prompt.text.trim().is_empty() {
            Some("Say what to change")
        } else {
            None
        }
    }

    /// Send the prompt to the draft's session, starting one when there is none. While the agent
    /// works, the prompt waits for the turn to end.
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
            self.start_agent(&working);
        }
        let desk = &mut self.remote.agent;
        desk.prompt = TextEdit::new("");
        if let Some(prompt) = desk.submit(prompt) {
            desk.run(prompt);
        }
    }

    /// A new session for `working`'s draft, in a sandbox of its own. The one before it ends.
    fn start_agent(&mut self, working: &crate::draft::Working) {
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
        desk.steps.clear();
        desk.seen = 0;
        desk.pull_due = false;
        desk.launched_past = None;
        desk.state = Some(State::Starting("Starting".into()));
        desk.session = Some(session);
    }

    /// Replace the session with a new one in a new sandbox, keeping a queued prompt.
    pub fn reset_agent(&mut self) {
        if let Some(working) = self.remote.working.clone() {
            self.start_agent(&working);
        }
    }

    /// Stop the turn in flight and run the queued prompt.
    pub fn send_agent_now(&mut self) {
        let desk = &mut self.remote.agent;
        if let Some(prompt) = desk.queued.take() {
            desk.run(prompt);
        }
    }

    pub fn drop_queued_prompt(&mut self) {
        self.remote.agent.queued = None;
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
        desk.queued = None;
        desk.pull_due = false;
        desk.launched_past = None;
    }

    /// Move the window to the launch the agent started, once the controller lists it, while the
    /// window still shows that draft.
    fn show_agent_launch(&mut self) -> bool {
        let Some(past) = self.remote.agent.launched_past.clone() else {
            return false;
        };
        let Some(draft) = self.remote.agent.draft().map(str::to_string) else {
            return false;
        };
        let Some(newest) = self
            .newest_launch(&draft)
            .map(|launch| launch.key.clone())
            .filter(|key| Some(key) != past.as_ref())
        else {
            return false;
        };
        self.remote.agent.launched_past = None;
        if self.selected_live().is_none_or(|shown| shown.id != draft) {
            return false;
        }
        self.ignition.launched(draft.clone());
        self.select(crate::Selection::Live {
            playbook: draft,
            launch: Some(newest),
        });
        true
    }

    /// Take what the session did, pull the working copy to a save it made, and follow a launch
    /// it started.
    pub(crate) fn refresh_agent(&mut self) -> bool {
        let desk = &mut self.remote.agent;
        let update = desk
            .session
            .as_ref()
            .and_then(|session| session.take(desk.seen));
        let mut changed = update.is_some();
        if update.as_ref().is_some_and(|update| update.launched) {
            let draft = self.remote.agent.draft().map(str::to_string);
            let newest = draft
                .and_then(|draft| self.newest_launch(&draft))
                .map(|launch| launch.key.clone());
            self.remote.agent.launched_past = Some(newest);
        }
        changed |= self.show_agent_launch();
        let desk = &mut self.remote.agent;
        if let Some(update) = update {
            desk.seen = update.version;
            desk.state = Some(update.state);
            desk.entries.extend(update.entries);
            desk.steps.extend(update.steps);
            let over = desk.steps.len().saturating_sub(KEPT);
            desk.steps.drain(..over);
            desk.pull_due |= update.saved;
            if let Some(prompt) = desk.due() {
                desk.run(prompt);
            }
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
    use crate::agent::{Config, Desk, Endpoint, Format, State};

    fn endpoint() -> Endpoint {
        Endpoint {
            base: "http://host.openshell.internal:4100".into(),
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
        assert!(
            argv.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["--plugin-dir", "/usr/local/share/crucible-agent/plugin"])
        );
        assert_eq!(
            endpoint().env(),
            [
                "CONTROLLER_URL=http://host.openshell.internal:4100",
                "CONTROLLER_API_TOKEN=tok",
                "CRUCIBLE_MCP_URL=http://host.openshell.internal:4100/mcp",
                "CRUCIBLE_MCP_TOKEN=tok",
            ]
        );

        let next = config.argv("now the docs", &endpoint(), true).unwrap();
        assert!(next.contains(&"--continue".to_string()));
        let plain = Config::default().argv("x", &endpoint(), false).unwrap();
        assert!(
            plain
                .windows(2)
                .any(|w| w == ["--model", "claude-opus-5-5"])
        );
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

    #[test]
    fn extra_variables_reach_the_harness_in_name_order() {
        let config: Config = toml::from_str(
            "providers = [\"model-gateway\"]\n[env]\nB = \"2\"\n\
             ANTHROPIC_BASE_URL = \"https://models.example.com\"\n",
        )
        .unwrap();
        assert_eq!(
            config.env().collect::<Vec<_>>(),
            ["ANTHROPIC_BASE_URL=https://models.example.com", "B=2"]
        );
        assert_eq!(config.image, Config::default().image);
        let back: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(back, config);
        assert_eq!(Config::default().env().count(), 0);
        assert!(!toml::to_string(&Config::default()).unwrap().contains("env"));
    }

    #[test]
    fn a_prompt_sent_while_the_agent_works_waits_for_the_turn_to_end() {
        let mut desk = Desk::default();
        assert_eq!(desk.submit("first".into()), Some("first".into()));
        desk.state = Some(State::Running);
        assert_eq!(desk.submit("second".into()), None);
        assert_eq!(desk.submit("third".into()), None);
        assert_eq!(desk.queued.as_deref(), Some("second\n\nthird"));
        assert_eq!(desk.due(), None);
        desk.state = Some(State::Starting("Creating the sandbox".into()));
        assert_eq!(desk.due(), None);
        desk.state = Some(State::Failed("gone".into()));
        assert_eq!(desk.due(), None);
        assert!(desk.queued.is_some());

        desk.state = Some(State::Idle);
        assert_eq!(desk.due(), Some("second\n\nthird".into()));
        assert_eq!(desk.queued, None);
        assert_eq!(desk.due(), None);
        assert_eq!(desk.submit("fourth".into()), Some("fourth".into()));
    }
}
