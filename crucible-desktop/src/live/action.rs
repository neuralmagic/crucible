//! What the window asks the live worker to do, and how each ask is carried out.

use crate::captured::{Captured, Opened};
use crate::draft::Working;
use crate::live::client::Dirs;
use crate::live::reports::{build_report, build_runs_report, captured_file, file_on_disk};
use crate::live::shape::{epoch, landed_label};
use crate::live::{DropOutcome, Form, Target, draft_id};
use crate::net::{Connection, within};
use crate::report::{Built, Gathered, Plugins, WantedRuns};
use crate::revisions::{self, Revisions, Wanted};
use jiff::tz::TimeZone;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What the window, the session streams and finished actions tell the worker.
pub(in crate::live) enum Msg {
    /// Which launch to keep current; `None` stops reading details.
    Select(Option<String>),
    /// A session stream delivered lines.
    Streamed,
    /// Read the launch list now.
    Refresh,
    /// Drop the connection and who it named, and read crux's config afresh.
    Reconnect,
    /// Which launch to read as the base of a comparison; `None` stops.
    Compare(Option<String>),
    /// Which draft save to read the pack of; `None` stops.
    Draft(Option<(String, i64)>),
    NodeLog(Option<crate::live::calls::NodeAsk>),
}

/// What the window asks for that runs as a task of its own.
pub(in crate::live) enum Action {
    /// Read a target's params schema and the admin caps, for a launch form.
    Prepare(Target),
    Launch {
        target: Target,
        body: Value,
        uploads: Vec<crate::form::Upload>,
    },
    /// What rerunning a finished run from these tasks would reuse.
    Rerun {
        run: String,
        from: Vec<String>,
    },
    /// Pull a draft's newest save into its working copy, creating the draft from `template`
    /// first when it does not exist.
    Pull {
        draft: String,
        template: Option<String>,
    },
    /// Save a working copy as its draft's next version.
    Save(Working),
    /// Push a pack folder as the next version of the draft named after it, creating the draft on
    /// first drop, and test-fire it under these ceilings.
    Drop {
        dir: PathBuf,
        max_cost: f64,
        max_time: String,
    },
    /// Fetch one file a run captured, by its key. `size` is what the listing says it holds, and
    /// `settled` that its run has stopped, so its copy on disk can stand in for it.
    File {
        run: String,
        key: String,
        size: u64,
        settled: bool,
    },
    Revisions(Wanted),
    /// Build a run's report from its pack's plugin, as the text the pack holds, and the files it
    /// captured.
    Report {
        wanted: crate::report::Wanted,
        plugin: String,
        listed: Vec<Captured>,
    },
    /// Build a playbook's runs view from its newest launch's plugin, as text, and what each
    /// launch's newest run captured.
    RunsReport {
        wanted: WantedRuns,
        plugin: String,
    },
}

/// What an [`Action`] came to.
pub(in crate::live) enum Reply {
    Prepared {
        target: Target,
        result: Result<Form, String>,
    },
    Launched {
        playbook: String,
        result: Result<String, String>,
    },
    Reran {
        run: String,
        from: Vec<String>,
        result: Result<crux::dto::RerunPreview, String>,
    },
    Synced {
        draft: String,
        result: Result<(Working, Option<String>), String>,
    },
    Dropped {
        dir: PathBuf,
        draft: String,
        result: Result<DropOutcome, String>,
    },
    File(Opened),
    Revisions(Revisions),
    Report(Built<crate::report::Wanted>),
    RunsReport(Built<WantedRuns>),
}

/// What the worker keeps between report builds: the plugin it last loaded, what the last run
/// report read, and what the last runs view read of each launch.
#[derive(Default)]
pub(in crate::live) struct Reporting {
    pub(in crate::live) plugins: Mutex<Plugins>,
    pub(in crate::live) read: Mutex<Option<Read>>,
    pub(in crate::live) gathered: Mutex<Arc<Gathered>>,
}

/// The files a run report read for the `inputs` its plugin names, kept so an event reads none
/// again.
pub(in crate::live) struct Read {
    pub(in crate::live) wanted: crate::report::Wanted,
    pub(in crate::live) inputs: Vec<String>,
    pub(in crate::live) files: Vec<crucible_view::File>,
}

impl Action {
    /// Whether the launch list changes when this lands.
    pub(in crate::live) fn launches(&self) -> bool {
        matches!(self, Action::Launch { .. } | Action::Drop { .. })
    }

    pub(in crate::live) async fn run(
        self,
        c: Connection,
        dirs: &Dirs,
        reporting: &Arc<Reporting>,
    ) -> Reply {
        match self {
            Action::Prepare(target) => {
                let result = prepare(&c, &target).await;
                Reply::Prepared { target, result }
            }
            Action::Launch {
                target,
                body,
                uploads,
            } => {
                let result = send_launch(&c, &target, body, &uploads).await;
                Reply::Launched {
                    playbook: target.id().to_string(),
                    result,
                }
            }
            Action::Rerun { run, from } => {
                let result = within("the rerun preview", c.client.rerun_preview(&run, &from)).await;
                Reply::Reran { run, from, result }
            }
            Action::Pull { draft, template } => {
                let result = pull(&c, dirs.drafts.as_deref(), &draft, template.as_deref()).await;
                Reply::Synced { draft, result }
            }
            Action::Save(working) => {
                let draft = working.draft.clone();
                let result = save(&c, dirs.drafts.as_deref(), working).await;
                Reply::Synced { draft, result }
            }
            Action::Drop {
                dir,
                max_cost,
                max_time,
            } => {
                let draft = draft_id(&dir).unwrap_or_default();
                let result = drop_pack(&c, &dir, &draft, max_cost, &max_time).await;
                Reply::Dropped { dir, draft, result }
            }
            Action::File {
                run,
                key,
                size,
                settled,
            } => {
                Reply::File(captured_file(&c, dirs.files.as_deref(), run, key, size, settled).await)
            }
            Action::Revisions(wanted) => Reply::Revisions(Revisions {
                result: read_revisions(&c, &wanted).await,
                wanted,
            }),
            Action::Report {
                wanted,
                plugin,
                listed,
            } => {
                let files = dirs.files.as_deref();
                let view = build_report(&c, files, reporting, &wanted, plugin, listed).await;
                let report = crate::report::Report::pack(wanted.asked.clone(), view);
                Reply::Report(Built::new(wanted, Some(report)))
            }
            Action::RunsReport { wanted, plugin } => {
                let files = dirs.files.as_deref();
                let report = build_runs_report(&c, files, reporting, &wanted, plugin).await;
                Reply::RunsReport(Built::new(wanted, report))
            }
        }
    }

    /// The reply when there is no connection to run on.
    pub(in crate::live) fn failed(self, why: String, dirs: &Dirs) -> Reply {
        match self {
            Action::Prepare(target) => Reply::Prepared {
                target,
                result: Err(why),
            },
            Action::Launch { target, .. } => Reply::Launched {
                playbook: target.id().to_string(),
                result: Err(why),
            },
            Action::Rerun { run, from } => Reply::Reran {
                run,
                from,
                result: Err(why),
            },
            Action::Pull { draft, .. } => Reply::Synced {
                draft,
                result: Err(why),
            },
            Action::Save(working) => Reply::Synced {
                draft: working.draft,
                result: Err(why),
            },
            Action::Drop { dir, .. } => Reply::Dropped {
                draft: draft_id(&dir).unwrap_or_default(),
                dir,
                result: Err(why),
            },
            Action::File { run, key, .. } => {
                Reply::File(match file_on_disk(dirs.files.as_deref(), &run, &key) {
                    Some((opened, _)) => opened,
                    None => Opened {
                        body: Err(why),
                        run,
                        key,
                        disk: None,
                    },
                })
            }
            Action::Revisions(wanted) => Reply::Revisions(Revisions {
                wanted,
                result: Err(why),
            }),
            Action::Report { wanted, .. } => {
                let report = crate::report::Report::pack(wanted.asked.clone(), Err(why));
                Reply::Report(Built::new(wanted, Some(report)))
            }
            Action::RunsReport { wanted, .. } => {
                let report = crate::report::Report::pack(wanted.asked.clone(), Err(why));
                Reply::RunsReport(Built::new(wanted, Some(report)))
            }
        }
    }
}

/// A draft's saves and test-fires, and the diff of the range asked for, or of the one the pane
/// opens on when none was picked or the picked one names a save the draft no longer holds.
async fn read_revisions(c: &Connection, wanted: &Wanted) -> Result<revisions::Read, String> {
    let draft = wanted.draft.as_str();
    let (history, runs) = tokio::join!(
        within("the draft's history", c.client.draft_history(draft)),
        within(
            "the draft's test-fires",
            c.client.playbook_runs(None, Some(draft))
        ),
    );
    let history: Vec<crux::dto::DraftHistoryEntry> = history?;
    let fires = revisions::fires(runs.unwrap_or_default());
    let range = wanted
        .range
        .and_then(|range| revisions::held(&history, range))
        .or_else(|| revisions::opening(&history, &fires, wanted.ran))
        .ok_or_else(|| format!("draft {draft} has no saves"))?;
    let diff = within(
        "the diff",
        c.client.draft_diff(draft, range.from, Some(range.to)),
    )
    .await?;
    let zone = TimeZone::system();
    let landed = history
        .iter()
        .map(|entry| revisions::Landed {
            at: epoch(&entry.created_at),
            label: landed_label(&entry.created_at, &zone),
        })
        .collect();
    Ok(revisions::Read {
        history,
        landed,
        fires,
        range,
        diff,
    })
}

pub(in crate::live) async fn prepare(c: &Connection, target: &Target) -> Result<Form, String> {
    let client = &c.client;
    let registered = match target {
        Target::Playbook(id) => Some(id.as_str()),
        Target::Draft(_) => None,
    };
    let schema = async {
        match target {
            Target::Playbook(id) => {
                let schema =
                    within("the params schema", client.playbook_schema::<Value>(id)).await?;
                Ok::<_, String>((schema, None))
            }
            Target::Draft(id) => {
                let head: crux::dto::DraftCompile =
                    within("the draft's schema", client.draft_preview(id, None)).await?;
                Ok((
                    head.params_schema.unwrap_or(Value::Null),
                    head.schema_digest,
                ))
            }
        }
    };
    let (schema, caps, targets, providers) = tokio::join!(
        schema,
        within("the launch caps", client.playbook_caps()),
        within("the dispatch targets", client.dispatch_targets(registered)),
        within("the providers", client.providers()),
    );
    let (schema, digest) = schema?;
    let caps: crux::dto::PlaybookCaps = caps?;
    Ok(Form {
        schema,
        digest,
        max_cost: caps.max_cost,
        max_time: caps.max_time,
        targets: targets?,
        providers: providers?,
    })
}

/// Launch `target` with `body`, for the new launch's key.
async fn send_launch(
    c: &Connection,
    target: &Target,
    mut body: Value,
    uploads: &[crate::form::Upload],
) -> Result<String, String> {
    for upload in uploads {
        let value = send_upload(c, upload).await?;
        match body.get_mut("params").and_then(Value::as_object_mut) {
            Some(params) => {
                params.insert(upload.param.clone(), value);
            }
            None => return Err("the launch body holds no params".to_string()),
        }
    }
    let ack: crux::dto::LaunchAck = match target {
        Target::Playbook(id) => within("the launch", c.client.launch(id, &body)).await?,
        Target::Draft(id) => within("the launch", c.client.launch_draft(id, &body)).await?,
    };
    Ok(ack.key)
}

async fn send_upload(c: &Connection, upload: &crate::form::Upload) -> Result<Value, String> {
    let bytes = std::fs::read(&upload.path)
        .map_err(|error| format!("{}: {error}", upload.path.display()))?;
    crate::form::too_large(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        upload.max_bytes,
    )
    .map_err(|why| format!("{}: {why}", upload.param))?;
    let digest = crucible_contract::artifact::content_digest(&bytes);
    let ack: crux::dto::UploadAck =
        within(&upload.param, c.client.put_param_file(&digest, bytes)).await?;
    let mut value = serde_json::json!({ "digest": ack.digest });
    if let Some(name) = upload.path.file_name().and_then(|name| name.to_str()) {
        value["name"] = Value::from(name);
    }
    if let Some(media_type) = &upload.media_type {
        value["media_type"] = Value::from(media_type.as_str());
    }
    Ok(value)
}

/// Pull `draft`'s newest save into its working copy under `drafts`, creating it from `template`
/// first when the controller has no such draft.
pub(in crate::live) async fn pull(
    c: &Connection,
    drafts: Option<&Path>,
    draft: &str,
    template: Option<&str>,
) -> Result<(Working, Option<String>), String> {
    let root = drafts.ok_or("no home directory for working copies")?;
    let newest = within(
        "the draft",
        c.client.draft_files::<crux::dto::DraftFiles>(draft, None),
    )
    .await;
    let (files, note) = match (newest, template) {
        (Ok(files), _) => (files, None),
        (Err(_), Some(template)) => {
            let _: crux::dto::DraftCompile = within(
                "creating the draft",
                c.client
                    .create_draft(draft, &format!("Drafted from {template}"), Some(template)),
            )
            .await?;
            let files: crux::dto::DraftFiles =
                within("the new draft", c.client.draft_files(draft, None)).await?;
            (
                files,
                Some(format!("Created draft {draft} from {template}")),
            )
        }
        (Err(error), None) => return Err(error),
    };
    let working = Working::write(root, draft, files.version, &files.files)?;
    Ok((working, note))
}

/// Push pack folder `dir` as the next version of `draft`, creating the draft on first drop, and
/// test-fire that version under `max_cost` and `max_time` where the draft's newest run went.
async fn drop_pack(
    c: &Connection,
    dir: &Path,
    draft: &str,
    max_cost: f64,
    max_time: &str,
) -> Result<DropOutcome, String> {
    if draft.is_empty() {
        return Err(format!(
            "{} names no draft: its name has no letters or digits",
            dir.display()
        ));
    }
    let (files, _) = crate::draft::read(dir)?;
    if files.is_empty() {
        return Err(format!("{} holds no pack files", dir.display()));
    }
    let newest = within(
        "the draft",
        c.client.draft_files::<crux::dto::DraftFiles>(draft, None),
    )
    .await;
    let base = match newest {
        Ok(newest) => newest.version,
        Err(_) => {
            let created: crux::dto::DraftCompile = within(
                "creating the draft",
                c.client.create_draft(
                    draft,
                    &format!("Dropped on Crucible Playbooks from {}", dir.display()),
                    None,
                ),
            )
            .await?;
            created.version
        }
    };
    match within("the save", c.client.save_draft(draft, &files, base)).await? {
        crux::client::DraftSave::Saved(_) => {}
        crux::client::DraftSave::Stale(stale) => return Err(stale.error),
    }
    let launches: Vec<crux::dto::PlaybookRun> = within(
        "the draft's launches",
        c.client.playbook_runs(None, Some(draft)),
    )
    .await?;
    let Some(newest) = crate::runs_on::newest_dispatched(&launches) else {
        return Ok(DropOutcome::Saved);
    };
    let newest: crux::dto::PlaybookLaunch =
        within("the draft's newest launch", c.client.playbook_run(newest)).await?;
    let Ok(runs_on) = crate::runs_on::RunsOn::try_from(&newest) else {
        return Ok(DropOutcome::Saved);
    };
    let mut body = serde_json::json!({
        "params": {},
        "max_cost": max_cost,
        "max_time": max_time,
    });
    runs_on.pin(&mut body);
    let ack: crux::dto::LaunchAck =
        within("the launch", c.client.launch_draft(draft, &body)).await?;
    Ok(DropOutcome::Launched(ack.key))
}

/// Save a working copy as its draft's next version.
async fn save(
    c: &Connection,
    drafts: Option<&Path>,
    mut working: Working,
) -> Result<(Working, Option<String>), String> {
    let root = drafts.ok_or("no home directory for working copies")?;
    let (files, digest) = crate::draft::read(&working.dir)?;
    if files.is_empty() {
        return Err("The working copy holds no files".to_string());
    }
    let saved = within(
        "the save",
        c.client.save_draft(&working.draft, &files, working.base),
    )
    .await?;
    match saved {
        crux::client::DraftSave::Saved(raw) => {
            let saved: crux::dto::DraftCompile =
                serde_json::from_value(raw).map_err(|e| format!("reading the save: {e}"))?;
            working.saved(root, saved.version, digest)?;
            let errors: Vec<&str> = saved
                .diagnostics
                .iter()
                .map(|d| d.message.as_str())
                .collect();
            let note = if saved.schema_digest.is_none() || !errors.is_empty() {
                format!(
                    "Saved version {}, which does not compile: {}",
                    saved.version,
                    errors.join("; ")
                )
            } else {
                format!("Saved version {}", saved.version)
            };
            Ok((working, Some(note)))
        }
        crux::client::DraftSave::Stale(stale) => Err(stale.error),
    }
}

pub(in crate::live) fn stopped() -> String {
    "building the view stopped early".to_string()
}
