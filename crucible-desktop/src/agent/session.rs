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
use crate::agent::{Endpoint, Entry, Format, SPEAKER, Spec, State, Update};
use crate::net::{Handle, Task};
use crate::profile::Target;
use crate::session::AgentStep;

/// Where an agent image keeps the drafting skill.
const SKILL: &str = "/usr/local/share/crucible-agent/plugin/skills/draft-a-playbook/SKILL.md";
/// Where an agent image keeps the crucible docs.
const DOCS: &str = "/usr/local/share/crucible-agent/docs";

enum Ask {
    Prompt(String),
    Stop,
}

struct Inner {
    version: u64,
    state: State,
    /// The turn in flight; 0 before the first prompt.
    turn: u32,
    entries: Vec<Entry>,
    steps: Vec<AgentStep>,
    saved: bool,
    launched: bool,
}

impl Inner {
    fn note(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Note {
            turn: self.turn,
            text: text.into(),
        });
    }
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

    fn asked(&self, prompt: &str) {
        self.change(|inner| {
            inner.turn += 1;
            inner.state = State::Running;
            inner.entries.push(Entry::Asked {
                turn: inner.turn,
                text: prompt.to_string(),
            });
        });
    }

    fn events(&self, events: Vec<AgentEvent>) {
        if events.is_empty() {
            return;
        }
        self.change(|inner| {
            for event in events {
                match event {
                    AgentEvent::Raw { text, .. } => inner.note(text),
                    event => inner.steps.push(AgentStep {
                        task: SPEAKER.to_string(),
                        attempt: inner.turn,
                        event,
                    }),
                }
            }
        });
    }

    fn note(&self, text: impl Into<String>) {
        self.change(|inner| inner.note(text));
    }

    fn passed(&self, passed: Passed) {
        self.change(|inner| match passed {
            Passed::Saved => {
                inner.saved = true;
                inner.note("Saved the draft");
            }
            Passed::Launched => {
                inner.launched = true;
                inner.note("Launched the draft");
            }
            Passed::Trouble(what) => inner.note(what),
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
            turn: 0,
            entries: Vec::new(),
            steps: Vec::new(),
            saved: false,
            launched: false,
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
                    if let Err(ended) = serve(&on, target, spec, asked, &out).await {
                        out.state(ended);
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

    /// Run `prompt` as the next turn. Mid-turn, the turn in flight is stopped for it.
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
            steps: std::mem::take(&mut inner.steps),
            saved: std::mem::take(&mut inner.saved),
            launched: std::mem::take(&mut inner.launched),
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

/// What one stdout line says the agent did. With no decoder, the line is what it said.
fn decode(decoder: &mut Option<Box<dyn LineFormat + Send>>, line: &str) -> Vec<AgentEvent> {
    match decoder {
        Some(decoder) => decoder.push(line),
        None => vec![AgentEvent::Text {
            delta: line.to_string(),
        }],
    }
}

/// What the agent is told before the user's request.
fn brief(spec: &Spec, prompt: &str) -> String {
    format!(
        "You are editing the crucible playbook draft `{draft}` for its owner, who is watching \
         this session and will read your last message.\n\n\
         ## Their request\n\n{prompt}\n\n\
         ## How to work\n\n\
         - Follow the draft-a-playbook skill, `{skill}`: pull the draft into `./pack` with \
         `crux`, edit there, push it back, and check the compile.\n\
         - The draft is at version {version} as this session starts.\n\
         - A launch costs money: at most `--max-cost {max_cost}` and `--max-time {max_time}`, \
         and only when the request needs a run to prove the change.\n\
         - The crucible docs are in `{docs}`.\n\
         - `crux` and the `crucible_*` tools act as the owner, with everything the owner may \
         do. Touch nothing but this draft and its runs unless the request says so.\n\n\
         Finish with what you changed and what you verified.",
        draft = spec.draft,
        version = spec.version,
        max_cost = spec.max_cost,
        max_time = spec.max_time,
        docs = DOCS,
        skill = SKILL,
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
) -> Result<(), State> {
    let config = &spec.config;
    out.starting("Starting the relay");
    let upstream = crate::net::blocking(move || Upstream::of(&target))
        .await
        .ok_or_else(|| "opening the controller connection stopped early".to_string())??;
    let (passed, mut passes) = unbounded_channel();
    let relay = relay::bind(runtime, upstream, &spec.draft, passed).await?;
    let endpoint = Endpoint {
        base: format!("http://{}:{}", sandbox::HOST, relay.port),
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
    .await
    .map_err(State::NotSetUp)?;
    out.state(State::Idle);

    let mut turns = 0u32;
    let mut next: Option<String> = None;
    loop {
        let prompt = match next.take() {
            Some(prompt) => prompt,
            None => loop {
                match asks.recv().await {
                    Some(Ask::Prompt(prompt)) => break prompt,
                    Some(Ask::Stop) => {}
                    None => return Ok(()),
                }
            },
        };
        out.asked(&prompt);
        let follow_up = turns > 0 && config.continues();
        let text = if follow_up {
            prompt
        } else {
            brief(&spec, &prompt)
        };
        let mut argv = vec!["env".to_string()];
        argv.extend(endpoint.env());
        argv.extend(config.env());
        argv.extend(config.argv(&text, &endpoint, follow_up)?);
        let mut decoder = decoder(config.format, config.model.as_deref().unwrap_or_default());
        let cancel = CancellationToken::new();
        let mut stopped = false;
        let done = {
            let exec = sandbox.exec(&argv, &cancel, |line| {
                out.events(decode(&mut decoder, line));
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
                        Some(Ask::Prompt(prompt)) => {
                            stopped = true;
                            cancel.cancel();
                            next = Some(prompt);
                        }
                        None => return Ok(()),
                    },
                }
            }
        };
        while let Ok(passed) = passes.try_recv() {
            out.passed(passed);
        }
        match done {
            _ if next.is_some() => out.note("Interrupted"),
            _ if stopped => out.note("Stopped"),
            Ok(done) => {
                for line in done.stderr_lines.iter().filter(|l| !l.trim().is_empty()) {
                    out.note(line.clone());
                }
                match (done.transport_error, done.exit_code) {
                    (Some((_, why)), _) => {
                        return Err(format!("The sandbox stopped answering: {why}").into());
                    }
                    (None, Some(0)) => {}
                    (None, Some(code)) => out.note(format!("The agent exited {code}")),
                    (None, None) => out.note("The agent ended without an exit code"),
                }
            }
            Err(why) => return Err(why.into()),
        }
        turns += 1;
        if next.is_none() {
            out.state(State::Idle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crucible_contract::event::AgentEvent;

    use crate::agent::relay::Passed;
    use crate::agent::session::{DOCS, Inner, Out, SKILL, brief, decode, decoder, sandbox_name};
    use crate::agent::{Config, Entry, Format, Spec, State};

    #[test]
    fn a_sandbox_name_fits_the_gateways_limit() {
        let name = sandbox_name();
        assert!(name.starts_with("cr-draft-"));
        assert_eq!(name.len(), 17);
        assert_ne!(name, sandbox_name());
    }

    #[test]
    fn plain_output_is_what_the_agent_said() {
        let mut none = decoder(Format::Text, "");
        assert_eq!(
            decode(&mut none, "{\"type\":\"whatever\"}"),
            [AgentEvent::Text {
                delta: "{\"type\":\"whatever\"}".into()
            }]
        );
    }

    #[test]
    fn claude_stream_json_becomes_agent_events() {
        let mut claude = decoder(Format::Claude, "");
        let init = decode(
            &mut claude,
            r#"{"type":"system","subtype":"init","model":"claude-fable-5-2","tools":["a","b"]}"#,
        );
        assert!(
            matches!(init.as_slice(), [AgentEvent::Init { model, tools: 2, .. }] if model == "claude-fable-5-2"),
            "{init:?}"
        );
        let stray = decode(&mut claude, "npm warn something");
        assert!(
            matches!(stray.as_slice(), [AgentEvent::Raw { text, .. }] if text == "npm warn something"),
            "{stray:?}"
        );
    }

    #[test]
    fn events_join_the_turn_in_flight_and_stray_lines_become_notes() {
        let out = Out {
            inner: Arc::new(Mutex::new(Inner {
                version: 1,
                state: State::Idle,
                turn: 0,
                entries: Vec::new(),
                steps: Vec::new(),
                saved: false,
                launched: false,
            })),
            waker: None,
        };
        out.note("before anything");
        out.asked("add a retry");
        out.events(vec![
            AgentEvent::Text {
                delta: "on it".into(),
            },
            AgentEvent::Raw {
                text: "npm warn".into(),
                stream: crucible_contract::event::RawStream::Stdout,
            },
        ]);
        out.passed(Passed::Saved);
        out.asked("now the docs");
        out.events(vec![AgentEvent::Text {
            delta: "done".into(),
        }]);
        let inner = out.inner.lock().unwrap();
        assert_eq!(inner.state, State::Running);
        assert!(inner.saved);
        assert_eq!(
            inner.entries,
            [
                Entry::Note {
                    turn: 0,
                    text: "before anything".into()
                },
                Entry::Asked {
                    turn: 1,
                    text: "add a retry".into()
                },
                Entry::Note {
                    turn: 1,
                    text: "npm warn".into()
                },
                Entry::Note {
                    turn: 1,
                    text: "Saved the draft".into()
                },
                Entry::Asked {
                    turn: 2,
                    text: "now the docs".into()
                },
            ]
        );
        let said: Vec<(u32, &str)> = inner
            .steps
            .iter()
            .map(|step| (step.attempt, step.task.as_str()))
            .collect();
        assert_eq!(said, [(1, "@agent"), (2, "@agent")]);
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
        assert!(text.contains("The draft is at version 4 as this session starts."));
        assert!(text.contains(
            "skill, `/usr/local/share/crucible-agent/plugin/skills/draft-a-playbook/SKILL.md`"
        ));
        assert!(text.contains("`--max-cost 1.5` and `--max-time 30m`"));
        assert!(text.contains("docs are in `/usr/local/share/crucible-agent/docs`"));
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
        for carried in [format!("{DOCS}/dsl-reference.md"), SKILL.to_string()] {
            let (code, _, errors) = run(vec!["test".into(), "-s".into(), carried.clone()]).await;
            assert_eq!(code, Some(0), "{carried}: {errors}");
        }

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

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the local OpenShell gateway and localhost/crucible-agent"]
    async fn a_prompt_sent_mid_turn_stops_the_turn_and_runs_next() {
        use crate::agent::session::Session;
        use crate::profile::{Profile, Target};

        let keys = tempfile::tempdir().unwrap();
        let profile = Profile {
            url: "http://127.0.0.1:9".into(),
            ..Profile::default()
        };
        let target = Target::new(
            "test",
            profile,
            &crate::keys::Store::Dir(keys.path().into()),
        );
        let spec = Spec {
            draft: "triage-wren".into(),
            version: 1,
            config: Config {
                format: Format::Text,
                command: ["sh", "-c", "echo began; sleep 300"]
                    .map(String::from)
                    .into(),
                ..Config::default()
            },
            max_cost: 1.0,
            max_time: "30m".into(),
        };
        let session = Session::start(Ok(tokio::runtime::Handle::current()), target, spec, None);

        let mut seen = 0;
        let mut began = 0;
        let mut notes: Vec<String> = Vec::new();
        let mut state = State::Starting(String::new());
        let mut until = async |began_at: usize, note: Option<&str>, idle: bool| {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(150);
            loop {
                if let Some(update) = session.take(seen) {
                    seen = update.version;
                    state = update.state;
                    began += update.steps.len();
                    notes.extend(update.entries.into_iter().filter_map(|entry| match entry {
                        Entry::Note { text, .. } => Some(text),
                        Entry::Asked { .. } => None,
                    }));
                }
                let noted = note.is_none_or(|note| notes.iter().any(|n| n == note));
                if began >= began_at && noted && (!idle || state == State::Idle) {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "began {began}, notes {notes:?}, state {state:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        };

        session.ask("one".into());
        until(1, None, false).await;
        session.ask("two".into());
        until(2, Some("Interrupted"), false).await;
        session.stop();
        until(2, Some("Stopped"), true).await;
        drop(session);
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
