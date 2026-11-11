//! A drafting session on this machine: a relay, an OpenShell sandbox, and the harness run in it
//! once per prompt.

use std::sync::{Arc, Mutex};

use crucible_contract::event::AgentEvent;
use crucible_harness::codex_json::CodexJsonParser;
use crucible_harness::line::LineFormat;
use crucible_harness::opencode_json::OpenCodeJsonParser;
use crucible_harness::pi_json::PiJsonParser;
use crucible_harness::stream_json::StreamJsonParser;
use rune::Waker;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::agent::relay::{self, Passed, Upstream};
use crate::agent::sandbox;
use crate::agent::{Endpoint, Entry, Format, Spec, State, Update};
use crate::logline::LogLine;
use crate::net::{Handle, Task};
use crate::profile::Target;

const DSL: &str = include_str!("../../../docs/dsl-reference.md");
/// The task name agent lines carry.
const SPEAKER: &str = "agent";

enum Ask {
    Prompt(String),
    Stop,
}

struct Inner {
    version: u64,
    state: State,
    entries: Vec<Entry>,
    saved: bool,
}

#[derive(Clone)]
struct Out {
    inner: Arc<Mutex<Inner>>,
    waker: Option<Waker>,
}

impl Out {
    fn change(&self, change: impl FnOnce(&mut Inner)) {
        if let Ok(mut inner) = self.inner.lock() {
            change(&mut inner);
            inner.version += 1;
        }
        if let Some(waker) = &self.waker {
            waker();
        }
    }

    fn state(&self, state: State) {
        self.change(|inner| inner.state = state);
    }

    fn starting(&self, what: &str) {
        self.state(State::Starting(what.to_string()));
    }

    fn entries(&self, entries: Vec<Entry>) {
        if !entries.is_empty() {
            self.change(|inner| inner.entries.extend(entries));
        }
    }

    fn note(&self, text: impl Into<String>) {
        self.entries(vec![Entry::Note(text.into())]);
    }

    fn passed(&self, passed: Passed) {
        self.change(|inner| match passed {
            Passed::Saved => {
                inner.saved = true;
                inner.entries.push(Entry::Note("Saved the draft".into()));
            }
            Passed::Launched => inner.entries.push(Entry::Note("Launched the draft".into())),
            Passed::Trouble(what) => inner.entries.push(Entry::Note(what)),
        });
    }
}

/// A session and its sandbox. Dropping it ends both.
pub struct Session {
    draft: String,
    asks: UnboundedSender<Ask>,
    inner: Arc<Mutex<Inner>>,
    _task: Option<Task>,
}

impl Session {
    /// Start a session for `spec` against `target`'s controller.
    pub fn start(
        runtime: Result<Handle, String>,
        target: Target,
        spec: Spec,
        waker: Option<Waker>,
    ) -> Session {
        let (asks, asked) = unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            version: 1,
            state: State::Starting("Starting".into()),
            entries: Vec::new(),
            saved: false,
        }));
        let out = Out {
            inner: Arc::clone(&inner),
            waker,
        };
        let draft = spec.draft.clone();
        let task = match runtime {
            Ok(runtime) => {
                let on = runtime.clone();
                Some(Task::spawn(&runtime, async move {
                    if let Err(why) = serve(&on, target, spec, asked, &out).await {
                        out.state(State::Failed(why));
                    }
                }))
            }
            Err(why) => {
                out.state(State::Failed(why));
                None
            }
        };
        Session {
            draft,
            asks,
            inner,
            _task: task,
        }
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn ask(&self, prompt: String) {
        let _ = self.asks.send(Ask::Prompt(prompt));
    }

    /// Stop the turn in flight.
    pub fn stop(&self) {
        let _ = self.asks.send(Ask::Stop);
    }

    /// What happened since version `seen`.
    pub fn take(&self, seen: u64) -> Option<Update> {
        let mut inner = self.inner.lock().ok()?;
        (inner.version > seen).then(|| Update {
            version: inner.version,
            state: inner.state.clone(),
            entries: std::mem::take(&mut inner.entries),
            saved: std::mem::take(&mut inner.saved),
        })
    }
}

fn decoder(format: Format, model: &str) -> Option<Box<dyn LineFormat + Send>> {
    Some(match format {
        Format::Claude => Box::new(StreamJsonParser::default().with_tool_io(true)),
        Format::Codex => Box::new(CodexJsonParser::new(model).with_tool_io(true)),
        Format::Opencode => Box::new(OpenCodeJsonParser::new(model)),
        Format::Pi => Box::new(PiJsonParser::new(model).with_tool_io(true)),
        Format::Text => return None,
    })
}

/// One stdout line as panel entries.
fn decode(decoder: &mut Option<Box<dyn LineFormat + Send>>, line: &str) -> Vec<Entry> {
    let Some(decoder) = decoder else {
        return vec![Entry::Agent(LogLine::parse(line))];
    };
    decoder
        .push(line)
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Raw { text, .. } => Some(LogLine::parse(text)),
            AgentEvent::Tokens(_) => None,
            event => LogLine::from_agent(SPEAKER, event),
        })
        .map(Entry::Agent)
        .collect()
}

/// What the agent is told before the user's request.
fn brief(spec: &Spec, prompt: &str) -> String {
    format!(
        "You are editing the crucible playbook draft `{draft}` for its owner, who is watching \
         this session and will read your last message.\n\n\
         ## Their request\n\n{prompt}\n\n\
         ## How to work\n\n\
         - The draft is at version {version}. `crucible_draft_files` reads the newest save and \
         its version.\n\
         - `crucible_draft_save` writes the next version from a whole `{{path: content}}` map: \
         send every file, changed or not, with `base_version` set to the version you read. A \
         refused save names the version to read again.\n\
         - `crucible_draft_preview` says how a save compiled. Clear every diagnostic before a \
         launch.\n\
         - `crucible_draft_launch` test-fires the newest save and costs money: at most \
         `max_cost` {max_cost} and `max_time` \"{max_time}\", and only when the request needs a \
         run to prove the change.\n\
         - `crucible_playbook_run` follows a launch by its key; `crucible_turns`, \
         `crucible_turn` and `crucible_run_log` explain a failure.\n\
         - `DSL.md` in the working directory is the pack language reference.\n\
         - The tools act as the owner, with everything the owner may do. Touch nothing but \
         this draft and its runs unless the request says so.\n\n\
         Finish with what you changed and what you verified.",
        draft = spec.draft,
        version = spec.version,
        max_cost = spec.max_cost,
        max_time = spec.max_time,
    )
}

fn sandbox_name() -> String {
    let id: String = rand::random::<[u8; 4]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{}{id}", sandbox::PREFIX)
}

async fn serve(
    runtime: &Handle,
    target: Target,
    spec: Spec,
    mut asks: UnboundedReceiver<Ask>,
    out: &Out,
) -> Result<(), String> {
    let config = &spec.config;
    out.starting("Starting the relay");
    let upstream = crate::net::blocking(move || Upstream::of(&target))
        .await
        .ok_or("opening the controller connection stopped early")??;
    let (passed, mut passes) = unbounded_channel();
    let relay = relay::bind(runtime, upstream, &spec.draft, passed).await?;
    let endpoint = Endpoint {
        url: format!("http://{}:{}/mcp", sandbox::HOST, relay.port),
        token: relay.token.clone(),
    };

    out.starting("Creating the sandbox");
    let sandbox = sandbox::create(
        runtime,
        &sandbox_name(),
        &config.image,
        &config.providers,
        &config.binaries,
        relay.port,
    )
    .await?;
    if let Err(why) = sandbox.write("DSL.md", DSL).await {
        out.note(why);
    }
    out.state(State::Idle);

    let mut turns = 0u32;
    loop {
        let prompt = loop {
            match asks.recv().await {
                Some(Ask::Prompt(prompt)) => break prompt,
                Some(Ask::Stop) => {}
                None => return Ok(()),
            }
        };
        out.change(|inner| {
            inner.state = State::Running;
            inner.entries.push(Entry::Asked(prompt.clone()));
        });
        let follow_up = turns > 0 && config.continues();
        let text = if follow_up {
            prompt
        } else {
            brief(&spec, &prompt)
        };
        let argv = config.argv(&text, &endpoint, follow_up)?;
        let mut decoder = decoder(config.format, config.model.as_deref().unwrap_or_default());
        let cancel = CancellationToken::new();
        let mut stopped = false;
        let done = {
            let exec = sandbox.exec(&argv, &cancel, |line| {
                out.entries(decode(&mut decoder, line));
            });
            tokio::pin!(exec);
            loop {
                tokio::select! {
                    done = &mut exec => break done,
                    Some(passed) = passes.recv() => out.passed(passed),
                    ask = asks.recv() => match ask {
                        Some(Ask::Stop) => {
                            stopped = true;
                            cancel.cancel();
                        }
                        Some(Ask::Prompt(_)) => {}
                        None => return Ok(()),
                    },
                }
            }
        };
        while let Ok(passed) = passes.try_recv() {
            out.passed(passed);
        }
        match done {
            _ if stopped => out.note("Stopped"),
            Ok(done) => {
                out.entries(
                    done.stderr_lines
                        .iter()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| Entry::Agent(LogLine::parse(line)))
                        .collect(),
                );
                match (done.transport_error, done.exit_code) {
                    (Some((_, why)), _) => out.note(format!("The agent's stream broke: {why}")),
                    (None, Some(0)) => {}
                    (None, Some(code)) => out.note(format!("The agent exited {code}")),
                    (None, None) => out.note("The agent ended without an exit code"),
                }
            }
            Err(why) => out.note(why),
        }
        turns += 1;
        out.state(State::Idle);
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::session::{DSL, brief, decode, decoder, sandbox_name};
    use crate::agent::{Config, Entry, Format, Spec};
    use crate::logline::Body;

    #[test]
    fn a_sandbox_name_fits_the_gateways_limit() {
        let name = sandbox_name();
        assert!(name.starts_with("cr-draft-"));
        assert_eq!(name.len(), 17);
        assert_ne!(name, sandbox_name());
    }

    #[test]
    fn the_dsl_reference_is_bundled() {
        assert!(DSL.len() > 10_000);
    }

    #[test]
    fn plain_output_is_kept_line_for_line() {
        let mut none = decoder(Format::Text, "");
        let entries = decode(&mut none, "{\"type\":\"whatever\"}");
        let [Entry::Agent(line)] = entries.as_slice() else {
            panic!("{entries:?}");
        };
        assert_eq!(line.body, Body::Plain("{\"type\":\"whatever\"}".into()));
    }

    #[test]
    fn claude_stream_json_becomes_agent_lines() {
        let mut claude = decoder(Format::Claude, "");
        let init = decode(
            &mut claude,
            r#"{"type":"system","subtype":"init","model":"claude-fable-5-2","tools":["a","b"]}"#,
        );
        assert!(
            matches!(init.as_slice(), [Entry::Agent(line)] if matches!(line.body, Body::Note(_))),
            "{init:?}"
        );
        let stray = decode(&mut claude, "npm warn something");
        let [Entry::Agent(line)] = stray.as_slice() else {
            panic!("{stray:?}");
        };
        assert_eq!(line.body, Body::Plain("npm warn something".into()));
    }

    #[test]
    fn the_brief_names_the_draft_the_request_and_the_ceilings() {
        let spec = Spec {
            draft: "triage-wren".into(),
            version: 4,
            config: Config::default(),
            max_cost: 1.5,
            max_time: "30m".into(),
        };
        let text = brief(&spec, "Add a retry to the fetch task.");
        assert!(text.contains("draft `triage-wren`"));
        assert!(text.contains("## Their request\n\nAdd a retry to the fetch task.\n"));
        assert!(text.contains("The draft is at version 4."));
        assert!(text.contains("`max_cost` 1.5 and `max_time` \"30m\""));
        assert!(text.contains("`{path: content}`"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the local OpenShell gateway and localhost/crucible-agent"]
    async fn a_sandboxed_process_reaches_the_controller_through_the_relay() {
        use axum::Router;
        use axum::routing::post;
        use serde_json::json;
        use tokio_util::sync::CancellationToken;

        use crate::agent::{relay, sandbox};
        use crate::profile::{Profile, Target};

        let controller = Router::new().route(
            "/mcp",
            post(|| async {
                json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
                    {"name": "crucible_draft_save"},
                    {"name": "crucible_draft_delete"},
                ]}})
                .to_string()
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, controller).await.unwrap() });

        let keys = tempfile::tempdir().unwrap();
        let profile = Profile {
            url,
            ..Profile::default()
        };
        let target = Target::new(
            "test",
            profile,
            &crate::keys::Store::Dir(keys.path().into()),
        );
        let upstream = relay::Upstream::of(&target).unwrap();
        let (passed, _passes) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Handle::current();
        let relay = relay::bind(&runtime, upstream, "triage-wren", passed)
            .await
            .unwrap();

        let config = Config::default();
        let sandbox = sandbox::create(
            &runtime,
            &sandbox_name(),
            &config.image,
            &config.providers,
            &config.binaries,
            relay.port,
        )
        .await
        .unwrap();
        sandbox.write("DSL.md", DSL).await.unwrap();

        let run = |argv: Vec<String>| {
            let sandbox = &sandbox;
            async move {
                let mut lines = Vec::new();
                let done = sandbox
                    .exec(&argv, &CancellationToken::new(), |line| {
                        lines.push(line.to_string())
                    })
                    .await
                    .unwrap();
                (
                    done.exit_code,
                    lines.join("\n"),
                    done.stderr_lines.join("\n"),
                )
            }
        };
        let (code, size, _) = run(vec!["wc".into(), "-c".into(), "DSL.md".into()]).await;
        assert_eq!(code, Some(0));
        assert!(size.starts_with(&DSL.len().to_string()), "{size}");

        let ask = |bearer: &str| {
            format!(
                "fetch('http://{}:{}/mcp',{{method:'POST',headers:{{authorization:'Bearer {bearer}'}},\
                 body:JSON.stringify({{jsonrpc:'2.0',id:1,method:'tools/list'}})}})\
                 .then(async r=>console.log(r.status, await r.text()))",
                sandbox::HOST,
                relay.port
            )
        };
        let node = |script: String| vec!["node".to_string(), "-e".to_string(), script];
        let (code, said, errors) = run(node(ask(&relay.token))).await;
        assert_eq!(code, Some(0), "{said}\n{errors}");
        assert!(said.starts_with("200 "), "{said}\n{errors}");
        assert!(said.contains("crucible_draft_save"), "{said}");
        assert!(said.contains("crucible_draft_delete"), "{said}");

        let (_, refused, _) = run(node(ask("not-the-token"))).await;
        assert!(refused.starts_with("401 "), "{refused}");
    }
}
