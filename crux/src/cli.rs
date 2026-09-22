//! `crux` — the crucible controller as a CLI. Every subcommand is one operation over
//! [`crate::ops`], the same code the controller's hosted MCP tools run.
//!
//! Start with a minted key: `export CONTROLLER_API_TOKEN=crk_…`, or `api_token` in the config
//! file.

#![allow(clippy::disallowed_macros)]

use crate::client::{AdoptBody, Client};
use crate::config::ConnectArgs;
use crate::ops;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Read;

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
    Whoami,

    /// List registered playbooks: what `launch` can start.
    Playbooks {
        #[arg(long)]
        json: bool,
    },

    /// The admin caps on `--max-cost` and `--max-time`. Read before `launch` or `draft-launch`.
    PlaybookCaps {
        #[arg(long)]
        json: bool,
    },

    /// The JSON Schema a playbook's params are validated against. Read before `launch`.
    PlaybookSchema {
        /// Playbook id.
        id: String,
    },

    /// Launch a playbook. Costs money against --max-cost.
    Launch {
        /// Playbook id.
        id: String,
        /// The params object its schema accepts, as JSON.
        #[arg(long, default_value = "{}")]
        params: String,
        /// Dollar ceiling for this launch.
        #[arg(long)]
        max_cost: f64,
        /// Wall-clock ceiling, e.g. 30m, 2h.
        #[arg(long)]
        max_time: String,
        /// Cluster to dispatch onto. Omit for the controller's default.
        #[arg(long)]
        dispatch_target: Option<String>,
        /// Registered inference provider the run's agent talks to, replacing the pack manifest's
        /// `[agent]` harness. Omit to resolve through the configured defaults.
        #[arg(long)]
        provider: Option<String>,
        /// The model to ask that provider for. Needs --provider; omit for its default.
        #[arg(long, requires = "provider")]
        model: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Playbook launches: key, playbook, status, count, cost against cap, and what stopped one.
    PlaybookRuns {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        playbook: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// One playbook launch whole, as JSON.
    PlaybookRun {
        /// Issue key the launch became.
        key: String,
    },

    /// The runs leaderboard.
    Runs {
        #[arg(long)]
        status: Option<String>,
        /// Repository, owner/repo.
        #[arg(long)]
        repo: Option<String>,
        /// Only runs dispatched to this cluster target.
        #[arg(long)]
        dispatch_target: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// One run with its candidates, as JSON.
    Run { run_id: String },

    /// Recurrences: what fires when, and what is blocked on an owner signing in.
    Schedules {
        #[arg(long)]
        json: bool,
    },

    /// Tracker watches: what each sweeps, and what is blocked on an owner signing in.
    Watches {
        #[arg(long)]
        json: bool,
    },

    /// One tracker watch, as JSON.
    Watch { id: String },

    /// List issues. One line each: key, status, tier, git ref, codegen contract, upstream, title.
    Issues {
        /// Input kind: github | scenario | jira.
        #[arg(long)]
        kind: Option<String>,
        /// Status: new | scoped | awaiting-approval | building | running | pr-open | parked | done.
        #[arg(long)]
        status: Option<String>,
        /// Max rows. Default 50, applied client-side.
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// One issue in full: provenance, the whole park reason, scopes, runs, and the event trail.
    Issue {
        /// Issue key, e.g. owner/repo#12 or scenario:<uuid>. Encoding is handled for you.
        key: String,
        /// Cut the issue body at this many chars.
        #[arg(long, default_value_t = 6000)]
        body_max: usize,
        #[arg(long)]
        json: bool,
    },

    /// Adopt a scenario: a controller issue with no upstream.
    Adopt {
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
    },

    /// Park an issue: stop working it, and record why.
    Park {
        key: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },

    /// Unpark an issue so the controller picks it up again.
    Unpark {
        key: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Change an issue's priority. Higher runs sooner.
    Bump {
        key: String,
        #[arg(long, default_value_t = 100)]
        priority: i64,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Wake the controller's reconcile loop. A full pass, not one issue.
    Reconcile {
        #[arg(long)]
        json: bool,
    },

    /// Re-dispatch an issue's run. Costs money.
    Redispatch {
        key: String,
        #[arg(long)]
        justification: String,
        #[arg(long)]
        json: bool,
    },

    /// List work-pod turns.
    Turns {
        /// Only turns for this issue key.
        #[arg(long)]
        issue: Option<String>,
        /// Work kind: grounded-rank | scope | run.
        #[arg(long)]
        kind: Option<String>,
        /// Lifecycle state: queued | running | succeeded | failed | collected | swept.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// One turn, with its result and error text intact.
    Turn {
        /// The turn's pod name.
        pod: String,
        #[arg(long)]
        json: bool,
    },

    /// A run's task graph in dependency order, each task with its latest status.
    Graph {
        run_id: String,
        /// Emit a mermaid flowchart instead of the ASCII table.
        #[arg(long)]
        mermaid: bool,
        #[arg(long)]
        json: bool,
    },

    /// Which build the controller is running, and whether it is the commit you expected.
    ///
    /// With --expect it is a gate: exits non-zero when the commit you named is not the one
    /// answering, so a deploy step can stop instead of reporting success it did not verify.
    /// Pass a full sha, e.g. `crux deployed --expect $(git rev-parse HEAD)`.
    Deployed {
        /// The commit you expect to be running. Compared by prefix, so a short sha works.
        #[arg(long)]
        expect: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// One window of a run's engine output, resumable by cursor.
    RunLog {
        run_id: String,
        /// Line to resume from, as the previous window's `cursor=` line reported it.
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        /// Lines in this window. Capped at 200, which is also the default.
        #[arg(long, conflicts_with = "follow")]
        limit: Option<usize>,
        /// Keep printing new lines until the run is no longer running.
        #[arg(long, short = 'f', conflicts_with = "json")]
        follow: bool,
        /// Seconds between polls under --follow.
        #[arg(long, default_value_t = 5, requires = "follow")]
        interval: u64,
        #[arg(long)]
        json: bool,
    },

    /// The files a run's tasks captured: what the run produced, as opposed to what it narrated.
    RunFiles {
        run_id: String,
        #[arg(long)]
        json: bool,
    },

    /// One captured file's content, by the key `run-files` listed it under.
    RunFile {
        run_id: String,
        /// `<task>/<declared path>`, fan-out brackets included.
        key: String,
        /// Write the content here instead of to stdout. Required for a file that is not text.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },

    /// The broker contract names this controller accepts.
    Contracts {
        #[arg(long)]
        json: bool,
    },

    /// Approve a scenario or jira issue's scope pack.
    Approve {
        key: String,
        #[arg(long)]
        json: bool,
    },

    /// One issue's approval gate: the PR to act on and the pack digest under it.
    ApprovalDetail {
        key: String,
        #[arg(long)]
        json: bool,
    },

    /// Every scope currently awaiting approval, plus the PRs already kept.
    Approvals {
        #[arg(long)]
        json: bool,
    },

    /// Propose a playbook pack import: fetch, compile, and store the preview an admin registers.
    PlaybookImport {
        /// owner/repo or a clone URL.
        repo: String,
        /// Branch or tag. Omit for the repo's default branch.
        #[arg(long)]
        git_ref: Option<String>,
        /// The pack directory inside the repo. Omit for the repo root.
        #[arg(long)]
        path: Option<String>,
        /// The registry id to propose registering under; prefills the review page.
        #[arg(long)]
        id: Option<String>,
        /// The description to propose; prefills the review page.
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// A draft pack's files at one save, with the version a save must be based on.
    DraftFiles {
        draft_id: String,
        /// Which save to read. Omit for the newest.
        #[arg(long)]
        version: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// Create a draft pack in the authoring studio, from a template or a skeleton.
    DraftCreate {
        draft_id: String,
        #[arg(long)]
        description: String,
        /// A registered playbook id to seed version 1 from.
        #[arg(long)]
        template: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Save a draft pack version from a `{path: content}` JSON map.
    DraftSave {
        draft_id: String,
        /// The version these edits were made against (see `draft-files`).
        #[arg(long)]
        base_version: i64,
        /// The whole pack as a JSON object of path to content. `-` reads stdin.
        #[arg(long)]
        files: String,
        #[arg(long)]
        json: bool,
    },

    /// Write a draft pack into a directory, to edit it in your own editor.
    DraftPull {
        draft_id: String,
        /// The directory to write the pack into. Created if it does not exist.
        dir: std::path::PathBuf,
        /// Which save to pull. Omit for the newest.
        #[arg(long)]
        version: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// Save a directory back as the next draft version. The whole tree is the save.
    DraftPush {
        draft_id: String,
        /// The directory holding the pack.
        dir: std::path::PathBuf,
        /// The version these edits were made against (what `draft-pull` printed).
        #[arg(long = "base-version", visible_alias = "base")]
        base_version: i64,
        #[arg(long)]
        json: bool,
    },

    /// List the secrets you own: id, name, kind, visibility, mode, owner. Never a value.
    Secrets {
        #[arg(long)]
        json: bool,
    },

    /// Bind a secret to a draft, playbook, repo, or domain, so a run at that scope receives it.
    /// One scope flag and one projection flag.
    SecretBind {
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
    },

    /// What a draft version compiled to: its schema digest and diagnostics, without launching it.
    DraftPreview {
        draft_id: String,
        /// Which save to compile. Omit for the newest.
        #[arg(long)]
        version: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// Test-fire a draft's newest save. Costs money against --max-cost.
    DraftLaunch {
        draft_id: String,
        /// The form values its schema accepts, as a JSON object of string to string.
        #[arg(long, default_value = "{}")]
        params: String,
        /// Dollar ceiling for this launch.
        #[arg(long)]
        max_cost: f64,
        /// Wall-clock ceiling, e.g. 30m, 2h.
        #[arg(long)]
        max_time: String,
        /// The schema digest these values were filled against (see `draft-preview`). A save that
        /// landed since is refused instead of launched.
        #[arg(long)]
        schema_digest: Option<String>,
        /// Cluster to dispatch onto. Omit for the controller's default.
        #[arg(long)]
        dispatch_target: Option<String>,
        /// Registered inference provider the run's agent talks to, replacing the pack manifest's
        /// `[agent]` harness. Omit to resolve through the configured defaults.
        #[arg(long)]
        provider: Option<String>,
        /// The model to ask that provider for. Needs --provider; omit for its default.
        #[arg(long, requires = "provider")]
        model: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Delete a draft and every version of it. Admin only; the id is free to create again.
    DraftDelete { draft_id: String },

    /// Graduate a draft: push its newest compiling version and open the export PR.
    DraftGraduate {
        draft_id: String,
        /// `owner/repo` the export PR opens against.
        #[arg(long)]
        repo: String,
        /// The pack directory inside that repo. Omit for the repo root.
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = cli.connect.resolve()?;
    let client = Client::connect(&cfg)?;

    let out = match cli.command {
        Command::Whoami => ops::whoami(&client).await?,
        Command::Playbooks { json } => ops::playbooks(&client, json).await?,
        Command::PlaybookCaps { json } => ops::playbook_caps(&client, json).await?,
        Command::PlaybookSchema { id } => ops::playbook_schema(&client, &id).await?,
        Command::Launch {
            id,
            params,
            max_cost,
            max_time,
            dispatch_target,
            provider,
            model,
            json,
        } => {
            let params: serde_json::Value =
                serde_json::from_str(&params).context("--params is not valid JSON")?;
            ops::launch(
                &client,
                &id,
                ops::Launch {
                    params,
                    max_cost,
                    max_time,
                    dispatch_target,
                    provider,
                    model,
                },
                json,
            )
            .await?
        }
        Command::PlaybookRuns {
            status,
            playbook,
            json,
        } => ops::playbook_runs(&client, status.as_deref(), playbook.as_deref(), json).await?,
        Command::PlaybookRun { key } => ops::playbook_run(&client, &key).await?,
        Command::Runs {
            status,
            repo,
            dispatch_target,
            limit,
            json,
        } => {
            ops::runs(
                &client,
                status.as_deref(),
                repo.as_deref(),
                dispatch_target.as_deref(),
                limit,
                json,
            )
            .await?
        }
        Command::Run { run_id } => ops::run(&client, &run_id).await?,
        Command::Schedules { json } => ops::schedules(&client, json).await?,
        Command::Watches { json } => ops::watches(&client, json).await?,
        Command::Watch { id } => ops::watch(&client, &id).await?,
        Command::Issues {
            kind,
            status,
            limit,
            json,
        } => ops::issues(&client, kind.as_deref(), status.as_deref(), limit, json).await?,
        Command::Issue {
            key,
            body_max,
            json,
        } => ops::issue(&client, &key, body_max, json).await?,
        Command::Adopt {
            title,
            body,
            body_text,
            repos,
            justification,
            authoritative,
            git_ref,
            codegen_contract,
            json,
        } => {
            ops::adopt(
                &client,
                &AdoptBody {
                    title,
                    body: read_body(body, body_text)?,
                    affected_repos: repos,
                    justification,
                    authoritative,
                    git_ref,
                    codegen_contract,
                },
                json,
            )
            .await?
        }
        Command::Park { key, reason, json } => ops::park(&client, &key, &reason, json).await?,
        Command::Unpark { key, reason, json } => {
            ops::unpark(&client, &key, reason.as_deref(), json).await?
        }
        Command::Bump {
            key,
            priority,
            reason,
            json,
        } => ops::bump(&client, &key, priority, reason.as_deref(), json).await?,
        Command::Reconcile { json } => ops::reconcile(&client, json).await?,
        Command::Redispatch {
            key,
            justification,
            json,
        } => ops::redispatch(&client, &key, &justification, json).await?,
        Command::Turns {
            issue,
            kind,
            state,
            json,
        } => {
            ops::turns(
                &client,
                issue.as_deref(),
                kind.as_deref(),
                state.as_deref(),
                json,
            )
            .await?
        }
        Command::Turn { pod, json } => ops::turn(&client, &pod, json).await?,
        Command::Graph {
            run_id,
            mermaid,
            json,
        } => ops::graph(&client, &run_id, mermaid, json).await?,
        Command::Deployed { expect, json } => {
            ops::deployed(&client, expect.as_deref(), json).await?
        }
        Command::RunLog {
            run_id,
            cursor,
            follow: true,
            interval,
            ..
        } => {
            let mut stdout = std::io::stdout().lock();
            ops::run_log_follow(
                &client,
                &run_id,
                cursor,
                std::time::Duration::from_secs(interval.max(1)),
                &mut stdout,
            )
            .await?
        }
        Command::RunLog {
            run_id,
            cursor,
            limit,
            json,
            ..
        } => ops::run_log(&client, &run_id, cursor, limit, json).await?,
        Command::RunFiles { run_id, json } => ops::run_files(&client, &run_id, json).await?,
        Command::RunFile { run_id, key, out } => {
            ops::run_file(&client, &run_id, &key, out.as_deref()).await?
        }
        Command::Contracts { json } => ops::contracts(&client, json).await?,
        Command::Approve { key, json } => ops::approve(&client, &key, json).await?,
        Command::ApprovalDetail { key, json } => ops::approval_detail(&client, &key, json).await?,
        Command::Approvals { json } => ops::approvals(&client, json).await?,
        Command::PlaybookImport {
            repo,
            git_ref,
            path,
            id,
            description,
            json,
        } => {
            ops::playbook_import(
                &client,
                &repo,
                git_ref.as_deref(),
                path.as_deref(),
                id.as_deref(),
                description.as_deref(),
                json,
            )
            .await?
        }
        Command::DraftFiles {
            draft_id,
            version,
            json,
        } => ops::draft_files(&client, &draft_id, version, json).await?,
        Command::DraftCreate {
            draft_id,
            description,
            template,
            json,
        } => ops::draft_create(&client, &draft_id, &description, template.as_deref(), json).await?,
        Command::DraftSave {
            draft_id,
            base_version,
            files,
            json,
        } => ops::draft_save(&client, &draft_id, base_version, &read_files(&files)?, json).await?,
        Command::DraftPull {
            draft_id,
            dir,
            version,
            json,
        } => ops::draft_pull(&client, &draft_id, &dir, version, json).await?,
        Command::DraftPush {
            draft_id,
            dir,
            base_version,
            json,
        } => ops::draft_push(&client, &draft_id, &dir, base_version, json).await?,
        Command::Secrets { json } => ops::secrets(&client, json).await?,
        Command::SecretBind {
            secret_id,
            playbook,
            repo,
            domain,
            env,
            file,
            declared_name,
            json,
        } => {
            let (scope_kind, scope_id) = match (&playbook, &repo, &domain) {
                (Some(id), None, None) => ("playbook", id.as_str()),
                (None, Some(id), None) => ("repo", id.as_str()),
                (None, None, Some(id)) => ("domain", id.as_str()),
                _ => bail!("secret-bind needs exactly one of --playbook, --repo, --domain"),
            };
            let (projection_kind, projection) = match (&env, &file) {
                (Some(name), None) => ("env", name.as_str()),
                (None, Some(path)) => ("file", path.as_str()),
                _ => bail!("secret-bind needs exactly one of --env, --file"),
            };
            ops::secret_bind(
                &client,
                ops::SecretBind {
                    secret_id: &secret_id,
                    scope_kind,
                    scope_id,
                    projection_kind,
                    projection,
                    declared_name: declared_name.as_deref(),
                },
                json,
            )
            .await?
        }
        Command::DraftPreview {
            draft_id,
            version,
            json,
        } => ops::draft_preview(&client, &draft_id, version, json).await?,
        Command::DraftLaunch {
            draft_id,
            params,
            max_cost,
            max_time,
            schema_digest,
            dispatch_target,
            provider,
            model,
            json,
        } => {
            let params: std::collections::BTreeMap<String, String> = serde_json::from_str(&params)
                .context(
                    "--params is not a JSON object of string to string; a draft launch takes form \
                     values as strings",
                )?;
            ops::draft_launch(
                &client,
                &draft_id,
                ops::DraftLaunch {
                    params,
                    max_cost,
                    max_time,
                    schema_digest,
                    dispatch_target,
                    provider,
                    model,
                },
                json,
            )
            .await?
        }
        Command::DraftDelete { draft_id } => ops::draft_delete(&client, &draft_id).await?,
        Command::DraftGraduate {
            draft_id,
            repo,
            path,
            json,
        } => ops::draft_graduate(&client, &draft_id, &repo, path.as_deref(), json).await?,
    };
    println!("{}", out.trim_end());
    Ok(())
}

/// `--body -` reads stdin, `--body PATH` reads a file, `--body-text` is inline.
///
/// A file or stdin rather than an argument by default: a scenario body is the prose a paid agent
/// turn will read, and anything worth adopting is longer than a shell argument wants to be.
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
/// to content: a save is the whole pack, so a shell cannot be asked to assemble it flag by flag.
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
    use super::*;
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
            Command::Issues {
                kind,
                status,
                limit,
                json,
            } => {
                assert_eq!(kind.as_deref(), Some("scenario"));
                assert_eq!(status, None);
                assert_eq!(limit, None);
                assert!(!json);
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
            Command::Issue { key, body_max, .. } => {
                assert_eq!(key, "owner/repo#12");
                assert_eq!(body_max, 6000);
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
            Command::Bump { priority, .. } => assert_eq!(priority, 100),
            _ => panic!("expected bump"),
        }
    }

    #[test]
    fn run_log_follow_excludes_json_and_a_window_limit() {
        let cli = Cli::try_parse_from(["crux", "run-log", "r1", "-f", "--interval", "2"])
            .expect("parses");
        match cli.command {
            Command::RunLog {
                follow, interval, ..
            } => {
                assert!(follow);
                assert_eq!(interval, 2);
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
            Command::SecretBind {
                secret_id,
                playbook,
                env,
                declared_name,
                ..
            } => {
                assert_eq!(secret_id, "01a0");
                assert_eq!(playbook.as_deref(), Some("docs-drift"));
                assert_eq!(env.as_deref(), Some("GH_TOKEN"));
                assert_eq!(declared_name, None);
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
            Command::Adopt {
                repos,
                git_ref,
                authoritative,
                ..
            } => {
                assert_eq!(repos, vec!["owner/one", "owner/two"]);
                assert_eq!(git_ref.as_deref(), Some("nv_dev"));
                assert!(!authoritative);
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
            Command::Graph {
                run_id, mermaid, ..
            } => {
                assert_eq!(run_id, "0192run");
                assert!(mermaid);
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
            Command::PlaybookImport {
                repo,
                git_ref,
                path,
                id,
                description,
                ..
            } => {
                assert_eq!(repo, "owner/packs");
                assert_eq!(git_ref.as_deref(), Some("main"));
                assert_eq!(path.as_deref(), Some("packs/calibrate"));
                assert_eq!(id.as_deref(), Some("calibrate"));
                assert_eq!(description.as_deref(), Some("EPP calibration"));
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
            Command::DraftSave {
                draft_id,
                base_version,
                files,
                ..
            } => {
                assert_eq!(draft_id, "calibrate");
                assert_eq!(base_version, 7);
                assert_eq!(files, "-");
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
        match Cli::try_parse_from(["crux", "draft-push", "calibrate", "./pack", "--base", "4"])
            .expect("parses")
            .command
        {
            Command::DraftPush {
                draft_id,
                dir,
                base_version,
                ..
            } => {
                assert_eq!(draft_id, "calibrate");
                assert_eq!(dir, std::path::PathBuf::from("./pack"));
                assert_eq!(base_version, 4);
            }
            _ => panic!("expected draft-push"),
        }
        match Cli::try_parse_from(["crux", "draft-pull", "calibrate", "./pack"])
            .expect("parses")
            .command
        {
            Command::DraftPull { version, dir, .. } => {
                assert_eq!(version, None, "a pull defaults to the newest save");
                assert_eq!(dir, std::path::PathBuf::from("./pack"));
            }
            _ => panic!("expected draft-pull"),
        }
    }

    #[test]
    fn draft_files_defaults_to_the_newest_version() {
        let cli = Cli::try_parse_from(["crux", "draft-files", "calibrate"]).expect("parses");
        match cli.command {
            Command::DraftFiles {
                draft_id, version, ..
            } => {
                assert_eq!(draft_id, "calibrate");
                assert_eq!(version, None);
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
            Command::DraftLaunch {
                draft_id,
                params,
                max_cost,
                max_time,
                schema_digest,
                ..
            } => {
                assert_eq!(draft_id, "calibrate");
                assert_eq!(params, r#"{"repo":"o/r"}"#);
                assert_eq!(max_cost, 4.0);
                assert_eq!(max_time, "30m");
                assert_eq!(schema_digest.as_deref(), Some("sha256:bb"));
            }
            _ => panic!("expected draft-launch"),
        }

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
            Command::DraftGraduate { repo, path, .. } => {
                assert_eq!(repo, "wren/packs");
                assert_eq!(path, None);
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
            Command::Launch {
                id,
                provider,
                model,
                dispatch_target,
                ..
            } => {
                assert_eq!(id, "survey");
                assert_eq!(provider.as_deref(), Some("openai"));
                assert_eq!(model.as_deref(), Some("gpt-5.6-luna"));
                assert_eq!(dispatch_target, None);
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
            Command::Launch {
                provider, model, ..
            } => {
                assert_eq!(provider.as_deref(), Some("openai"));
                assert_eq!(model, None, "the provider's default model");
            }
            _ => panic!("expected launch"),
        }
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
