//! `crux` — the crucible controller as a CLI. Every subcommand is one [`crate::ops`] operation,
//! the same struct the controller's hosted MCP tools decode.
//!
//! Start with a minted key: `export CONTROLLER_API_TOKEN=crk_…`, or `api_token` in the config
//! file.

#![allow(clippy::disallowed_macros)]

use crate::client::Client;
use crate::config::ConnectArgs;
use crate::ops::{self, Operation};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::time::Duration;

/// A token-frugal CLI for the crucible controller.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    connect: ConnectArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Who the controller thinks you are, and how you got there.
    Whoami(ops::Whoami),

    /// List registered playbooks: what `launch` can start.
    Playbooks(ops::playbooks::Playbooks),

    /// The admin caps on `--max-cost` and `--max-time`. Read before `launch` or `draft-launch`.
    PlaybookCaps(ops::playbooks::PlaybookCaps),

    /// The JSON Schema a playbook's params are validated against. Read before `launch`.
    PlaybookSchema(ops::playbooks::PlaybookSchema),

    /// Launch a playbook. Costs money against --max-cost.
    Launch(ops::playbooks::Launch),

    /// Playbook launches: key, playbook, status, count, cost against cap, and what stopped one.
    PlaybookRuns(ops::playbooks::PlaybookRuns),

    /// One playbook launch whole, as JSON.
    PlaybookRun(ops::playbooks::PlaybookRun),

    /// The runs leaderboard.
    Runs(ops::runs::Runs),

    /// One run with its candidates, as JSON.
    Run(ops::runs::Run),

    /// Recurrences: what fires when, and what is blocked on an owner signing in.
    Schedules(ops::launches::Schedules),

    /// Tracker watches: what each sweeps, and what is blocked on an owner signing in.
    Watches(ops::launches::Watches),

    /// One tracker watch, as JSON.
    Watch(ops::launches::Watch),

    /// Webhooks: what each launches, how it verifies, and when it last took a delivery.
    Webhooks(ops::launches::Webhooks),

    /// One webhook, as JSON.
    Webhook(ops::launches::Webhook),

    /// A webhook's recorded deliveries, newest first, with how each settled.
    WebhookDeliveries(ops::launches::WebhookDeliveries),

    /// Register a provider from a JSON body (`POST /api/providers`).
    ProviderCreate(ops::providers::ProviderCreate),

    /// Replace a provider's registration from a JSON body (`PUT /api/providers/<id>`).
    ProviderUpdate(ops::providers::ProviderUpdate),

    /// Deregister a provider and every default naming it. Refused while issues or schedules pin
    /// it; `provider-update` with `"enabled": false` retires one that work still names.
    ProviderDelete(ops::providers::ProviderDelete),

    /// The provider each scope's unpinned dispatches take, per workload class, and its fallback.
    DispatchDefaults(ops::providers::DispatchDefaults),

    /// Set one dispatch default from a JSON body (`PUT /api/config/dispatch-defaults`). Admin only.
    DispatchDefaultSet(ops::providers::DispatchDefaultSet),

    /// Check a transform's CEL from a JSON body (`filter`, `dedupe`, `derive`) the way a save
    /// does. Prints `ok`, or fails with one `field:line:col: message` per refusal.
    WebhookCheck(ops::launches::WebhookCheck),

    /// Starting points for a webhook body: a sender's verifier and a transform, as JSON.
    WebhookPresets(ops::launches::WebhookPresets),

    /// Create a webhook from a JSON body (`POST /api/webhooks`). Prints its secret, once.
    WebhookCreate(ops::launches::WebhookCreate),

    /// Replace a webhook from a JSON body. Its verifier and secret stay as created.
    WebhookUpdate(ops::launches::WebhookUpdate),

    /// Resume a paused webhook: its address takes deliveries again.
    WebhookResume(ops::launches::WebhookResume),

    /// Pause a webhook: its address answers not found and its queued deliveries fail.
    WebhookPause(ops::launches::WebhookPause),

    /// Replace a webhook's secret and print the new one, once.
    WebhookRotate(ops::launches::WebhookRotate),

    WebhookDelete(ops::launches::WebhookDelete),

    /// Run a transform on a sample delivery from a JSON body; nothing is stored.
    WebhookPreview(ops::launches::WebhookPreview),

    /// List issues. One line each: key, status, tier, git ref, codegen contract, upstream, title.
    Issues(ops::issues::Issues),

    /// One issue in full: provenance, the whole park reason, scopes, runs, and the event trail.
    Issue(ops::issues::Issue),

    /// Adopt a scenario: a controller issue with no upstream.
    Adopt(AdoptArgs),

    /// Park an issue: stop working it, and record why.
    Park(ops::issues::Park),

    /// Unpark an issue so the controller picks it up again.
    Unpark(ops::issues::Unpark),

    /// Change an issue's priority. Higher runs sooner.
    Bump(ops::issues::Bump),

    /// Wake the controller's reconcile loop. A full pass, not one issue.
    Reconcile(ops::issues::Reconcile),

    /// Re-dispatch an issue's run. Costs money.
    Redispatch(ops::issues::Redispatch),

    /// List work-pod turns.
    Turns(ops::issues::Turns),

    /// One turn, with its result and error text intact.
    Turn(ops::issues::Turn),

    /// A run's task graph in dependency order, each task with its latest status.
    Graph(ops::runs::Graph),

    /// Which build the controller is running, and whether it is the commit you expected.
    ///
    /// With --expect it is a gate: exits non-zero when the commit you named is not the one
    /// answering, so a deploy step can stop instead of reporting success it did not verify.
    /// Pass a full sha, e.g. `crux deployed --expect $(git rev-parse HEAD)`.
    Deployed(ops::runs::Deployed),

    /// One window of a run's engine output, resumable by cursor.
    RunLog(ops::runs::RunLog),

    /// The files a run's tasks captured: what the run produced, as opposed to what it narrated.
    RunFiles(ops::runs::RunFiles),

    /// One captured file's content, by the key `run-files` listed it under.
    RunFile(ops::runs::RunFile),

    /// The broker contract names this controller accepts.
    Contracts(ops::issues::Contracts),

    /// Approve a scenario or jira issue's scope pack.
    Approve(ops::issues::Approve),

    /// One issue's approval gate: the PR to act on and the pack digest under it.
    ApprovalDetail(ops::issues::ApprovalDetail),

    /// Every scope currently awaiting approval, plus the PRs already kept.
    Approvals(ops::issues::Approvals),

    /// Propose a playbook pack import: fetch, compile, and store the preview an admin registers.
    PlaybookImport(ops::playbooks::PlaybookImport),

    /// A draft pack's files at one save, with the version a save must be based on.
    DraftFiles(ops::drafts::DraftFiles),

    /// Create a draft pack in the authoring studio, from a template or a skeleton.
    DraftCreate(ops::drafts::DraftCreate),

    /// Save a draft pack version from a `{path: content}` JSON map.
    DraftSave(DraftSaveArgs),

    /// Write a draft pack into a directory, to edit it in your own editor.
    DraftPull(ops::drafts::DraftPull),

    /// Save a directory back as the next draft version. The whole tree is the save.
    DraftPush(ops::drafts::DraftPush),

    /// List the inference providers you may read: id, kind, default model, harness, the price per
    /// million tokens a run on it is charged, and whether launches may pick it.
    Providers(ops::providers::Providers),

    /// List the secrets you own: id, name, kind, visibility, mode, owner. Never a value.
    Secrets(ops::secrets::Secrets),

    /// Register a secret. The value is read from piped stdin, never from an argument or a
    /// terminal: `pbpaste | crux secret-create NAME --kind opaque`. Prints metadata, never the value.
    SecretCreate(SecretCreateArgs),

    /// Bind a secret to a draft, playbook, repo, or domain, so a run at that scope receives it.
    /// One scope flag and one projection flag.
    SecretBind(SecretBindArgs),

    /// What a draft version compiled to: its schema digest and diagnostics, without launching it.
    DraftPreview(ops::drafts::DraftPreview),

    /// Test-fire a draft's newest save. Costs money against --max-cost.
    DraftLaunch(ops::drafts::DraftLaunch),

    /// Delete a draft, every version of it, and its secret bindings, shares and schedules. Admin
    /// only; the id is free to create again.
    DraftDelete(ops::drafts::DraftDelete),

    /// Rename a draft. Its saves, secret bindings, shares, standing launches and launches follow.
    DraftRename(ops::drafts::DraftRename),

    /// Copy a draft's newest save into a new draft, with the bindings you could make yourself.
    DraftClone(ops::drafts::DraftClone),

    /// Publish a draft: register its newest compiling version as a playbook, with no review.
    DraftPublish(ops::drafts::DraftPublish),

    /// Graduate a draft: push its newest compiling version and open the export PR.
    DraftGraduate(ops::drafts::DraftGraduate),
}

/// The flags of `adopt`. The body comes from a file or stdin rather than an argument by default:
/// a scenario body is the prose a paid agent turn will read, and anything worth adopting is
/// longer than a shell argument wants to be.
#[derive(Debug, clap::Args)]
pub struct AdoptArgs {
    #[arg(long)]
    title: String,
    /// The ask, as prose. `-` reads stdin; otherwise it is a path to read.
    #[arg(long, conflicts_with = "body_text")]
    body: Option<String>,
    /// The ask, inline. Use --body for anything longer than a sentence.
    #[arg(long)]
    body_text: Option<String>,
    /// A repo this touches. Repeat for several; the first is the clone target.
    #[arg(long = "repo", required = true)]
    repos: Vec<String>,
    /// Why the controller should spend money on this.
    #[arg(long)]
    justification: String,
    /// Treat the body as authoritative (skips grounded ranking).
    #[arg(long)]
    authoritative: bool,
    /// Branch or tag to clone. Omit for the repo's default branch.
    #[arg(long)]
    git_ref: Option<String>,
    /// A configured broker contract (see `contracts`). Omit for local measure.
    #[arg(long)]
    codegen_contract: Option<String>,
    #[arg(long)]
    json: bool,
}

impl AdoptArgs {
    fn into_op(self) -> Result<ops::issues::Adopt> {
        Ok(ops::issues::Adopt {
            title: self.title,
            body: read_body(self.body, self.body_text)?,
            affected_repos: self.repos,
            justification: self.justification,
            authoritative: self.authoritative,
            git_ref: self.git_ref,
            codegen_contract: self.codegen_contract,
            json: self.json,
        })
    }
}

/// The flags of `draft-save`: the file map comes from a file or stdin, since a save is the whole
/// pack and a shell cannot be asked to assemble it flag by flag.
#[derive(Debug, clap::Args)]
pub struct DraftSaveArgs {
    draft_id: String,
    /// The version these edits were made against (see `draft-files`).
    #[arg(long)]
    base_version: i64,
    /// The whole pack as a JSON object of path to content. `-` reads stdin.
    #[arg(long)]
    files: String,
    #[arg(long)]
    json: bool,
}

impl DraftSaveArgs {
    fn into_op(self) -> Result<ops::drafts::DraftSaveFiles> {
        Ok(ops::drafts::DraftSaveFiles {
            draft_id: self.draft_id,
            base_version: self.base_version,
            files: read_files(&self.files)?,
            json: self.json,
        })
    }
}

/// The flags of `secret-create`. The value never appears here: it is read from piped stdin.
#[derive(Debug, clap::Args)]
pub struct SecretCreateArgs {
    /// The name a pack's `[[secret]] name` refers to. Unique per owner.
    name: String,
    #[arg(long, value_enum)]
    kind: ops::secrets::SecretKind,
    /// Omit for `broker-only`: the run gets it, the agent in the sandbox does not.
    #[arg(long, value_enum)]
    visibility: Option<ops::secrets::Visibility>,
    /// `user:<login>` or a `group:` in your claims. Omit to own it yourself.
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    json: bool,
}

impl SecretCreateArgs {
    fn into_op(self, value: ops::secrets::SecretValue) -> ops::secrets::SecretCreate {
        ops::secrets::SecretCreate {
            name: self.name,
            kind: self.kind,
            visibility: self.visibility,
            owner: self.owner,
            value,
            json: self.json,
        }
    }
}

/// The flags of `secret-bind`: one scope flag and one projection flag, which the operation
/// carries as a kind and a value.
#[derive(Debug, clap::Args)]
pub struct SecretBindArgs {
    /// The secret's id, as `secrets` lists it.
    secret_id: String,
    /// Scope: a draft or registered playbook id.
    #[arg(long, group = "scope")]
    playbook: Option<String>,
    /// Scope: an `owner/name` repo.
    #[arg(long, group = "scope")]
    repo: Option<String>,
    /// Scope: a domain id.
    #[arg(long, group = "scope")]
    domain: Option<String>,
    /// Projection: the environment variable the value arrives in.
    #[arg(long, group = "projection")]
    env: Option<String>,
    /// Projection: the absolute path the value is written to.
    #[arg(long, group = "projection")]
    file: Option<String>,
    /// The manifest's `[[secret]] name` this binding satisfies. Defaults to the secret's name.
    #[arg(long)]
    declared_name: Option<String>,
    #[arg(long)]
    json: bool,
}

impl SecretBindArgs {
    fn into_op(self) -> Result<ops::secrets::SecretBind> {
        let (scope_kind, scope_id) = match (self.playbook, self.repo, self.domain) {
            (Some(id), None, None) => ("playbook", id),
            (None, Some(id), None) => ("repo", id),
            (None, None, Some(id)) => ("domain", id),
            _ => bail!("secret-bind needs exactly one of --playbook, --repo, --domain"),
        };
        let (projection_kind, projection) = match (self.env, self.file) {
            (Some(name), None) => ("env", name),
            (None, Some(path)) => ("file", path),
            _ => bail!("secret-bind needs exactly one of --env, --file"),
        };
        Ok(ops::secrets::SecretBind {
            secret_id: self.secret_id,
            scope_kind: scope_kind.to_string(),
            scope_id,
            projection_kind: projection_kind.to_string(),
            projection,
            declared_name: self.declared_name,
            json: self.json,
        })
    }
}

/// The CLI definition, for the surface fixture test.
pub fn command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = cli.connect.resolve()?;
    let c = Client::connect(&cfg)?;

    let out = match cli.command {
        Command::Whoami(op) => op.run(&c).await?,
        Command::Playbooks(op) => op.run(&c).await?,
        Command::PlaybookCaps(op) => op.run(&c).await?,
        Command::PlaybookSchema(op) => op.run(&c).await?,
        Command::Launch(op) => op.run(&c).await?,
        Command::PlaybookRuns(op) => op.run(&c).await?,
        Command::PlaybookRun(op) => op.run(&c).await?,
        Command::Runs(op) => op.run(&c).await?,
        Command::Run(op) => op.run(&c).await?,
        Command::Schedules(op) => op.run(&c).await?,
        Command::Watches(op) => op.run(&c).await?,
        Command::Watch(op) => op.run(&c).await?,
        Command::Webhooks(op) => op.run(&c).await?,
        Command::Webhook(op) => op.run(&c).await?,
        Command::WebhookDeliveries(op) => op.run(&c).await?,
        Command::ProviderCreate(op) => op.run(&c).await?,
        Command::ProviderUpdate(op) => op.run(&c).await?,
        Command::ProviderDelete(op) => op.run(&c).await?,
        Command::DispatchDefaults(op) => op.run(&c).await?,
        Command::DispatchDefaultSet(op) => op.run(&c).await?,
        Command::WebhookCheck(op) => op.run(&c).await?,
        Command::WebhookPresets(op) => op.run(&c).await?,
        Command::WebhookCreate(op) => op.run(&c).await?,
        Command::WebhookUpdate(op) => op.run(&c).await?,
        Command::WebhookResume(op) => op.run(&c).await?,
        Command::WebhookPause(op) => op.run(&c).await?,
        Command::WebhookRotate(op) => op.run(&c).await?,
        Command::WebhookDelete(op) => op.run(&c).await?,
        Command::WebhookPreview(op) => op.run(&c).await?,
        Command::Issues(op) => op.run(&c).await?,
        Command::Issue(op) => op.run(&c).await?,
        Command::Adopt(args) => args.into_op()?.run(&c).await?,
        Command::Park(op) => op.run(&c).await?,
        Command::Unpark(op) => op.run(&c).await?,
        Command::Bump(op) => op.run(&c).await?,
        Command::Reconcile(op) => op.run(&c).await?,
        Command::Redispatch(op) => op.run(&c).await?,
        Command::Turns(op) => op.run(&c).await?,
        Command::Turn(op) => op.run(&c).await?,
        Command::Graph(op) => op.run(&c).await?,
        Command::Deployed(op) => op.run(&c).await?,
        Command::RunLog(op) if op.follow => {
            let mut stdout = std::io::stdout().lock();
            op.follow(&c, Duration::from_secs(op.interval.max(1)), &mut stdout)
                .await?
        }
        Command::RunLog(op) => op.run(&c).await?,
        Command::RunFiles(op) => op.run(&c).await?,
        Command::RunFile(op) => op.run(&c).await?,
        Command::Contracts(op) => op.run(&c).await?,
        Command::Approve(op) => op.run(&c).await?,
        Command::ApprovalDetail(op) => op.run(&c).await?,
        Command::Approvals(op) => op.run(&c).await?,
        Command::PlaybookImport(op) => op.run(&c).await?,
        Command::DraftFiles(op) => op.run(&c).await?,
        Command::DraftCreate(op) => op.run(&c).await?,
        Command::DraftSave(args) => args.into_op()?.run(&c).await?,
        Command::DraftPull(op) => op.run(&c).await?,
        Command::DraftPush(op) => op.run(&c).await?,
        Command::Providers(op) => op.run(&c).await?,
        Command::Secrets(op) => op.run(&c).await?,
        Command::SecretCreate(args) => {
            let stdin = std::io::stdin();
            let value = read_secret_value(&stdin, std::io::IsTerminal::is_terminal(&stdin))?;
            args.into_op(value).run(&c).await?
        }
        Command::SecretBind(args) => args.into_op()?.run(&c).await?,
        Command::DraftPreview(op) => op.run(&c).await?,
        Command::DraftLaunch(op) => op.run(&c).await?,
        Command::DraftDelete(op) => op.run(&c).await?,
        Command::DraftRename(op) => op.run(&c).await?,
        Command::DraftClone(op) => op.run(&c).await?,
        Command::DraftPublish(op) => op.run(&c).await?,
        Command::DraftGraduate(op) => op.run(&c).await?,
    };
    println!("{}", out.trim_end());
    Ok(())
}

/// A secret value arrives on piped stdin only. A terminal would echo it as it is typed.
fn read_secret_value(mut input: impl Read, terminal: bool) -> Result<ops::secrets::SecretValue> {
    if terminal {
        bail!(
            "pipe the secret value on stdin, e.g. `pbpaste | crux secret-create NAME --kind opaque`"
        );
    }
    let mut raw = String::new();
    input
        .read_to_string(&mut raw)
        .context("reading the secret value from stdin")?;
    ops::secrets::SecretValue::new(raw)
}

/// `--body -` reads stdin, `--body PATH` reads a file, `--body-text` is inline.
fn read_body(path: Option<String>, inline: Option<String>) -> Result<String> {
    if let Some(text) = inline {
        return Ok(text);
    }
    let Some(path) = path else {
        bail!("adopt needs a body: pass --body <file|-> or --body-text <text>");
    };
    let body = if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading the body from stdin")?;
        buf
    } else {
        std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?
    };
    let body = body.trim_end().to_string();
    if body.trim().is_empty() {
        bail!("the adopt body is empty (the controller will 422 on it)");
    }
    Ok(body)
}

/// `--files -` reads stdin, `--files PATH` reads a file. Either way it is one JSON object of path
/// to content.
fn read_files(source: &str) -> Result<std::collections::BTreeMap<String, String>> {
    let raw = if source == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading the file map from stdin")?;
        buf
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading {source}"))?
    };
    let files: std::collections::BTreeMap<String, String> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {source} as a JSON object of path to content"))?;
    if files.is_empty() {
        bail!("the file map is empty; a save with no files would empty the pack");
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use crate::cli::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// A bare invocation is a usage error, not a server: there is nothing to serve.
    #[test]
    fn a_bare_invocation_is_a_usage_error() {
        assert!(Cli::try_parse_from(["crux"]).is_err());
    }

    #[test]
    fn connection_flags_are_global_and_reach_every_subcommand() {
        let cli = Cli::try_parse_from([
            "crux",
            "issues",
            "--url",
            "https://crucible.example.com",
            "--kind",
            "scenario",
        ])
        .expect("a global flag after the subcommand parses");
        assert_eq!(
            cli.connect.url.as_deref(),
            Some("https://crucible.example.com")
        );
        match cli.command {
            Command::Issues(op) => {
                assert_eq!(op.kind.as_deref(), Some("scenario"));
                assert_eq!(op.status, None);
                assert_eq!(op.limit, None);
                assert!(!op.json);
            }
            _ => panic!("expected issues"),
        }
    }

    /// Issue keys carry `/` and `#`. clap must take one as a positional value, not mistake it for
    /// anything else, and the client encodes it later.
    #[test]
    fn an_issue_key_survives_the_parser_verbatim() {
        let cli = Cli::try_parse_from(["crux", "issue", "owner/repo#12"]).expect("parses");
        match cli.command {
            Command::Issue(op) => {
                assert_eq!(op.key, "owner/repo#12");
                assert_eq!(op.body_max, 6000);
            }
            _ => panic!("expected issue"),
        }
    }

    #[test]
    fn park_requires_a_reason_and_bump_defaults_its_priority() {
        assert!(
            Cli::try_parse_from(["crux", "park", "owner/repo#1"]).is_err(),
            "parking without a recorded reason must not be possible"
        );
        let cli = Cli::try_parse_from(["crux", "bump", "owner/repo#1"]).expect("parses");
        match cli.command {
            Command::Bump(op) => assert_eq!(op.priority, 100),
            _ => panic!("expected bump"),
        }
    }

    #[test]
    fn run_log_follow_excludes_json_and_a_window_limit() {
        let cli = Cli::try_parse_from(["crux", "run-log", "r1", "-f", "--interval", "2"])
            .expect("parses");
        match cli.command {
            Command::RunLog(op) => {
                assert!(op.follow);
                assert_eq!(op.interval, 2);
                assert_eq!(op.cursor, Some(0));
            }
            _ => panic!("expected run-log"),
        }
        assert!(Cli::try_parse_from(["crux", "run-log", "r1", "-f", "--json"]).is_err());
        assert!(Cli::try_parse_from(["crux", "run-log", "r1", "-f", "--limit", "5"]).is_err());
        assert!(
            Cli::try_parse_from(["crux", "run-log", "r1", "--interval", "2"]).is_err(),
            "--interval means nothing without --follow"
        );
    }

    #[test]
    fn secret_create_takes_no_value_argument() {
        let cli = Cli::try_parse_from([
            "crux",
            "secret-create",
            "bugwatch-label-token",
            "--kind",
            "opaque",
            "--visibility",
            "broker-only",
        ])
        .expect("parses");
        match cli.command {
            Command::SecretCreate(args) => {
                assert_eq!(args.name, "bugwatch-label-token");
                assert_eq!(args.kind, ops::secrets::SecretKind::Opaque);
                assert_eq!(args.visibility, Some(ops::secrets::Visibility::BrokerOnly));
                assert_eq!(args.owner, None);
            }
            _ => panic!("expected secret-create"),
        }
        for bad in [
            vec![
                "crux",
                "secret-create",
                "n",
                "--kind",
                "opaque",
                "--value",
                "x",
            ],
            vec![
                "crux",
                "secret-create",
                "n",
                "--kind",
                "opaque",
                "ghp_positional",
            ],
            vec!["crux", "secret-create", "n", "--kind", "password"],
            vec!["crux", "secret-create", "n"],
        ] {
            assert!(Cli::try_parse_from(&bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn a_secret_value_comes_from_a_pipe_and_never_a_terminal() {
        assert!(read_secret_value("ghp_abc\n".as_bytes(), false).is_ok());
        let refused = read_secret_value("ghp_abc\n".as_bytes(), true)
            .err()
            .expect("a terminal is refused");
        assert!(
            refused.to_string().contains("pipe the secret value"),
            "{refused}"
        );
        assert!(
            read_secret_value("".as_bytes(), false).is_err(),
            "empty is refused"
        );
    }

    #[test]
    fn secret_bind_takes_one_scope_and_one_projection() {
        let cli = Cli::try_parse_from([
            "crux",
            "secret-bind",
            "01a0",
            "--playbook",
            "docs-drift",
            "--env",
            "GH_TOKEN",
        ])
        .expect("parses");
        match cli.command {
            Command::SecretBind(args) => {
                let op = args.into_op().expect("one scope, one projection");
                assert_eq!(op.secret_id, "01a0");
                assert_eq!(
                    (op.scope_kind.as_str(), op.scope_id.as_str()),
                    ("playbook", "docs-drift")
                );
                assert_eq!(
                    (op.projection_kind.as_str(), op.projection.as_str()),
                    ("env", "GH_TOKEN")
                );
                assert_eq!(op.declared_name, None);
            }
            _ => panic!("expected secret-bind"),
        }
        assert!(
            Cli::try_parse_from([
                "crux",
                "secret-bind",
                "01a0",
                "--playbook",
                "a",
                "--repo",
                "o/n",
                "--env",
                "X"
            ])
            .is_err(),
            "two scopes is a parse error"
        );
        assert!(
            Cli::try_parse_from([
                "crux",
                "secret-bind",
                "01a0",
                "--playbook",
                "a",
                "--env",
                "X",
                "--file",
                "/p"
            ])
            .is_err(),
            "two projections is a parse error"
        );
    }

    #[test]
    fn redispatch_requires_a_justification() {
        assert!(Cli::try_parse_from(["crux", "redispatch", "k"]).is_err());
        assert!(Cli::try_parse_from(["crux", "redispatch", "k", "--justification", "why"]).is_ok());
    }

    #[test]
    fn adopt_takes_repeated_repos_and_rejects_none() {
        let cli = Cli::try_parse_from([
            "crux",
            "adopt",
            "--title",
            "t",
            "--body-text",
            "b",
            "--repo",
            "owner/one",
            "--repo",
            "owner/two",
            "--justification",
            "j",
            "--git-ref",
            "nv_dev",
        ])
        .expect("parses");
        match cli.command {
            Command::Adopt(args) => {
                let op = args.into_op().expect("an inline body");
                assert_eq!(op.affected_repos, vec!["owner/one", "owner/two"]);
                assert_eq!(op.git_ref.as_deref(), Some("nv_dev"));
                assert!(!op.authoritative);
                assert_eq!(op.body, "b");
            }
            _ => panic!("expected adopt"),
        }
        assert!(
            Cli::try_parse_from([
                "crux",
                "adopt",
                "--title",
                "t",
                "--body-text",
                "b",
                "--justification",
                "j"
            ])
            .is_err(),
            "affected_repos is required by the API, so require it here"
        );
    }

    /// `--body` and `--body-text` are two ways to say the same thing; accepting both would leave
    /// the precedence to chance.
    #[test]
    fn the_two_body_sources_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from([
                "crux",
                "adopt",
                "--title",
                "t",
                "--body",
                "-",
                "--body-text",
                "b",
                "--repo",
                "o/r",
                "--justification",
                "j"
            ])
            .is_err()
        );
    }

    #[test]
    fn a_body_file_is_read_and_an_empty_one_is_refused() {
        let dir = std::env::temp_dir().join(format!("crux-body-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("body.md");
        std::fs::write(&path, "the ask\n\n").expect("write");
        let text = read_body(Some(path.display().to_string()), None).expect("read");
        assert_eq!(text, "the ask");

        std::fs::write(&path, "   \n").expect("write");
        assert!(read_body(Some(path.display().to_string()), None).is_err());
        assert!(read_body(None, None).is_err(), "no body at all is an error");
        assert_eq!(
            read_body(Some("ignored".into()), Some("inline".into())).expect("inline wins"),
            "inline"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graph_takes_a_mermaid_flag() {
        let cli = Cli::try_parse_from(["crux", "graph", "0192run", "--mermaid"]).expect("parses");
        match cli.command {
            Command::Graph(op) => {
                assert_eq!(op.run_id, "0192run");
                assert!(op.mermaid);
            }
            _ => panic!("expected graph"),
        }
    }

    #[test]
    fn an_import_takes_a_repo_and_the_naming_it_proposes() {
        let cli = Cli::try_parse_from([
            "crux",
            "playbook-import",
            "owner/packs",
            "--git-ref",
            "main",
            "--path",
            "packs/calibrate",
            "--id",
            "calibrate",
            "--description",
            "EPP calibration",
        ])
        .expect("parses");
        match cli.command {
            Command::PlaybookImport(op) => {
                assert_eq!(op.repo, "owner/packs");
                assert_eq!(op.git_ref.as_deref(), Some("main"));
                assert_eq!(op.path.as_deref(), Some("packs/calibrate"));
                assert_eq!(op.id.as_deref(), Some("calibrate"));
                assert_eq!(op.description.as_deref(), Some("EPP calibration"));
            }
            _ => panic!("expected playbook-import"),
        }
        assert!(
            Cli::try_parse_from(["crux", "playbook-import"]).is_err(),
            "there is nothing to fetch without a repo"
        );
    }

    /// Saving blind is how one editor overwrites another, so the base is required rather than
    /// defaulted.
    #[test]
    fn a_draft_save_cannot_omit_the_base_it_edited_from() {
        assert!(Cli::try_parse_from(["crux", "draft-save", "calibrate", "--files", "-"]).is_err());
        let cli = Cli::try_parse_from([
            "crux",
            "draft-save",
            "calibrate",
            "--base-version",
            "7",
            "--files",
            "-",
        ])
        .expect("parses");
        match cli.command {
            Command::DraftSave(args) => {
                assert_eq!(args.draft_id, "calibrate");
                assert_eq!(args.base_version, 7);
                assert_eq!(args.files, "-");
            }
            _ => panic!("expected draft-save"),
        }
    }

    #[test]
    fn a_file_map_is_read_as_json_and_an_empty_one_is_refused() {
        let dir = std::env::temp_dir().join(format!("crux-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("files.json");
        std::fs::write(&path, r#"{"wf.crux":"task a {}"}"#).expect("write");
        let files = read_files(&path.display().to_string()).expect("read");
        assert_eq!(files.get("wf.crux").map(String::as_str), Some("task a {}"));

        std::fs::write(&path, "{}").expect("write");
        assert!(
            read_files(&path.display().to_string()).is_err(),
            "an empty map would empty the pack"
        );
        std::fs::write(&path, "not json").expect("write");
        assert!(read_files(&path.display().to_string()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A push carries the base it edited from, the same discipline `draft-save` has: without one
    /// there is nothing protecting the other editor's save.
    #[test]
    fn a_directory_push_cannot_omit_the_base_it_pulled() {
        assert!(
            Cli::try_parse_from(["crux", "draft-push", "calibrate", "./pack"]).is_err(),
            "a push with no base is not a save"
        );
        assert!(
            Cli::try_parse_from(["crux", "draft-push", "calibrate", "./pack", "--base", "4"])
                .is_err(),
            "--base is left free for a base digest"
        );
        match Cli::try_parse_from([
            "crux",
            "draft-push",
            "calibrate",
            "./pack",
            "--base-version",
            "4",
        ])
        .expect("parses")
        .command
        {
            Command::DraftPush(op) => {
                assert_eq!(op.draft_id, "calibrate");
                assert_eq!(op.dir, std::path::PathBuf::from("./pack"));
                assert_eq!(op.base_version, 4);
            }
            _ => panic!("expected draft-push"),
        }
        match Cli::try_parse_from(["crux", "draft-pull", "calibrate", "./pack"])
            .expect("parses")
            .command
        {
            Command::DraftPull(op) => {
                assert_eq!(op.version, None, "a pull defaults to the newest save");
                assert_eq!(op.dir, std::path::PathBuf::from("./pack"));
            }
            _ => panic!("expected draft-pull"),
        }
    }

    #[test]
    fn draft_files_defaults_to_the_newest_version() {
        let cli = Cli::try_parse_from(["crux", "draft-files", "calibrate"]).expect("parses");
        match cli.command {
            Command::DraftFiles(op) => {
                assert_eq!(op.draft_id, "calibrate");
                assert_eq!(op.version, None);
            }
            _ => panic!("expected draft-files"),
        }
    }

    /// A test-fire spends money, so it carries both ceilings or it does not parse; a graduation
    /// opens a PR somewhere, so it carries the repo.
    #[test]
    fn a_test_fire_carries_its_ceilings_and_a_graduation_carries_its_repo() {
        assert!(
            Cli::try_parse_from(["crux", "draft-launch", "calibrate"]).is_err(),
            "a launch with no ceilings parsed"
        );
        match Cli::try_parse_from([
            "crux",
            "draft-launch",
            "calibrate",
            "--params",
            r#"{"repo":"o/r"}"#,
            "--max-cost",
            "4",
            "--max-time",
            "30m",
            "--schema-digest",
            "sha256:bb",
        ])
        .expect("parses")
        .command
        {
            Command::DraftLaunch(op) => {
                assert_eq!(op.draft_id, "calibrate");
                assert_eq!(
                    op.params,
                    std::collections::BTreeMap::from([("repo".to_string(), "o/r".to_string())])
                );
                assert_eq!(op.max_cost, 4.0);
                assert_eq!(op.max_time, "30m");
                assert_eq!(op.schema_digest.as_deref(), Some("sha256:bb"));
            }
            _ => panic!("expected draft-launch"),
        }
        assert!(
            Cli::try_parse_from([
                "crux",
                "draft-launch",
                "calibrate",
                "--params",
                r#"{"n": 1}"#,
                "--max-cost",
                "4",
                "--max-time",
                "30m",
            ])
            .is_err(),
            "a draft launch takes form values as strings"
        );

        match Cli::try_parse_from([
            "crux",
            "draft-launch",
            "calibrate",
            "--max-cost",
            "1",
            "--max-time",
            "5m",
            "--rerun-of",
            "run-7",
            "--from",
            "check",
            "--from",
            "report",
        ])
        .expect("parses")
        .command
        {
            Command::DraftLaunch(op) => {
                assert_eq!(op.rerun_of.as_deref(), Some("run-7"));
                assert_eq!(op.from, ["check", "report"]);
            }
            _ => panic!("expected draft-launch"),
        }
        assert!(
            Cli::try_parse_from([
                "crux",
                "launch",
                "survey",
                "--max-cost",
                "1",
                "--max-time",
                "5m",
                "--from",
                "check",
            ])
            .is_err(),
            "--from without --rerun-of parsed"
        );

        assert!(
            Cli::try_parse_from(["crux", "draft-graduate", "calibrate"]).is_err(),
            "a graduation with no repo parsed"
        );
        match Cli::try_parse_from([
            "crux",
            "draft-graduate",
            "calibrate",
            "--repo",
            "wren/packs",
        ])
        .expect("parses")
        .command
        {
            Command::DraftGraduate(op) => {
                assert_eq!(op.repo, "wren/packs");
                assert_eq!(op.path, None);
            }
            _ => panic!("expected draft-graduate"),
        }
    }

    /// Both launch surfaces pin a provider the same way: `--model` rides on `--provider`, since a
    /// model alone names no service, and the controller would 422 it anyway.
    #[test]
    fn a_launch_pins_its_provider_and_a_model_needs_one() {
        match Cli::try_parse_from([
            "crux",
            "launch",
            "survey",
            "--max-cost",
            "4",
            "--max-time",
            "30m",
            "--provider",
            "openai",
            "--model",
            "gpt-5.6-luna",
        ])
        .expect("parses")
        .command
        {
            Command::Launch(op) => {
                assert_eq!(op.id, "survey");
                assert_eq!(op.provider.as_deref(), Some("openai"));
                assert_eq!(op.model.as_deref(), Some("gpt-5.6-luna"));
                assert_eq!(op.dispatch_target, None);
                assert_eq!(
                    op.params,
                    serde_json::json!({}),
                    "the default params object"
                );
            }
            _ => panic!("expected launch"),
        }
        match Cli::try_parse_from([
            "crux",
            "launch",
            "survey",
            "--max-cost",
            "4",
            "--max-time",
            "30m",
            "--provider",
            "openai",
        ])
        .expect("parses")
        .command
        {
            Command::Launch(op) => {
                assert_eq!(op.provider.as_deref(), Some("openai"));
                assert_eq!(op.model, None, "the provider's default model");
            }
            _ => panic!("expected launch"),
        }
        assert!(
            Cli::try_parse_from([
                "crux",
                "launch",
                "survey",
                "--params",
                "{not json",
                "--max-cost",
                "4",
                "--max-time",
                "30m",
            ])
            .is_err(),
            "--params must be JSON"
        );
        for surface in ["launch", "draft-launch"] {
            let Err(err) = Cli::try_parse_from([
                "crux",
                surface,
                "survey",
                "--max-cost",
                "4",
                "--max-time",
                "30m",
                "--model",
                "gpt-5.6-luna",
            ]) else {
                panic!("{surface}: a model with no provider parsed");
            };
            assert!(err.to_string().contains("--provider"), "{surface}: {err}");
        }
    }
}
