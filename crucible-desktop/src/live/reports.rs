//! Building a pack's views on the worker: the plugin, a run's captured files, and the launches a
//! runs view reads.

use crate::captured::{Body, Captured, Opened};
use crate::live::action::{Read, Reporting, stopped};
use crate::live::shape::captured_files;
use crate::net::{Connection, within};
use crate::report::{Gathered, Ready, WantedRuns};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, PoisonError};

/// The plugin at `path` in a pack, from its `text`, loaded through what `reporting` keeps.
async fn load_plugin(
    reporting: &Arc<Reporting>,
    path: String,
    text: String,
) -> Result<Arc<crate::report::Plugin>, String> {
    let reporting = Arc::clone(reporting);
    crate::net::blocking(move || {
        let mut plugins = reporting
            .plugins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        plugins.load(&path, text.as_bytes())
    })
    .await
    .ok_or_else(stopped)
}

/// A run's report: load the plugin, fetch the captured files it asks for, and run it. An event
/// is answered from the files the last report read, when it would read the same.
pub(in crate::live) async fn build_report(
    c: &Connection,
    files: Option<&Path>,
    reporting: &Arc<Reporting>,
    wanted: &crate::report::Wanted,
    plugin: String,
    listed: Vec<Captured>,
) -> Result<crucible_view::View, String> {
    let loaded = load_plugin(reporting, wanted.plugin.clone(), plugin).await?;
    let Ready { inputs, .. } = (*loaded).as_ref().map_err(String::clone)?;
    let kept = wanted.asked.event.as_ref().and_then(|_| {
        let read = reporting
            .read
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        read.as_ref()
            .filter(|read| read.wanted.reads_as(wanted) && read.inputs == *inputs)
            .map(|read| read.files.clone())
    });
    let read = match kept {
        Some(read) => read,
        None => {
            let read = read_run(c, files, wanted, inputs, &listed).await?;
            *reporting
                .read
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Read {
                wanted: wanted.clone(),
                inputs: inputs.clone(),
                files: read.clone(),
            });
            read
        }
    };
    let asked = wanted.asked.clone();
    crate::net::blocking(move || {
        let Ready { loaded, inputs, .. } = (*loaded).as_ref().map_err(String::clone)?;
        crate::report::run(loaded, inputs, read, &asked)
    })
    .await
    .ok_or_else(stopped)?
}

/// The files of `listed` at the paths `inputs` names, as `wanted`'s run captured them.
async fn read_run(
    c: &Connection,
    files: Option<&Path>,
    wanted: &crate::report::Wanted,
    inputs: &[String],
    listed: &[Captured],
) -> Result<Vec<crucible_view::File>, String> {
    let asked: Vec<&Captured> = listed
        .iter()
        .filter(|file| inputs.contains(&file.path))
        .collect();
    crate::report::admit(asked.iter().map(|file| (file.key.as_str(), file.size)))?;
    let mut read = Vec::new();
    for file in asked {
        let opened = captured_file(
            c,
            files,
            wanted.run.clone(),
            file.key.clone(),
            file.size,
            wanted.settled,
        )
        .await;
        let text = match opened.body {
            Ok(Body::Text(text)) => text,
            Ok(Body::Binary(_)) => return Err(format!("{} is not text.", file.key)),
            Err(error) => return Err(format!("reading {}: {error}", file.key)),
        };
        read.push(crucible_view::File {
            path: file.path.clone(),
            task: file.owner(),
            text,
        });
    }
    Ok(read)
}

/// How many launches a runs view reads at once.
const RUNS_AT_ONCE: usize = 6;

/// A playbook's runs view: load the plugin, read each launch `wanted` names, newest first, and
/// run it. `None` when the plugin draws no runs view. A launch the last build read and that has
/// stopped unchanged is not read again, and none is for an event that asks about the same
/// launches; the launches past the bytes a view reads are left out, and so is one that could
/// not be read.
pub(in crate::live) async fn build_runs_report(
    c: &Connection,
    files: Option<&Path>,
    reporting: &Arc<Reporting>,
    wanted: &WantedRuns,
    plugin: String,
) -> Option<crate::report::Report> {
    let failed = |error: String| crate::report::Report::pack(wanted.asked.clone(), Err(error));
    let loaded = match load_plugin(reporting, wanted.plugin.clone(), plugin).await {
        Ok(loaded) => loaded,
        Err(error) => return Some(failed(error)),
    };
    let inputs = match &*loaded {
        Ok(Ready {
            runs_inputs: Some(inputs),
            ..
        }) => inputs.clone(),
        Ok(Ready {
            runs_inputs: None, ..
        }) => return None,
        Err(error) => return Some(failed(error.clone())),
    };
    let earlier = Arc::clone(
        &reporting
            .gathered
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    let gathered = if wanted.asked.event.is_some() && earlier.read(&inputs, &wanted.launches) {
        earlier
    } else {
        let gathered = match gather(c, files, &earlier, inputs, &wanted.launches).await {
            Ok(gathered) => Arc::new(gathered),
            Err(error) => return Some(failed(error)),
        };
        *reporting
            .gathered
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::clone(&gathered);
        gathered
    };
    let request = wanted.asked.runs_request(
        gathered.runs.iter().map(|(_, run)| run.clone()).collect(),
        wanted.total,
    );
    let view = crate::net::blocking(move || {
        let Ready { loaded, .. } = (*loaded).as_ref().map_err(String::clone)?;
        crate::report::run_runs(loaded, &request)
    })
    .await
    .unwrap_or_else(|| Err(stopped()));
    Some(crate::report::Report {
        unread: gathered.unread.clone(),
        ..crate::report::Report::pack(wanted.asked.clone(), view)
    })
}

/// `launches` read for `inputs`, newest first, up to the bytes a view reads. A launch `earlier`
/// kept is not read again, and one that cannot be read is left out with why. The first reason,
/// when none can be.
async fn gather(
    c: &Connection,
    files: Option<&Path>,
    earlier: &Gathered,
    inputs: Vec<String>,
    launches: &[crate::report::Launched],
) -> Result<Gathered, String> {
    let mut runs = Vec::new();
    let mut unread = Vec::new();
    let mut held = 0_u64;
    'read: for batch in launches.chunks(RUNS_AT_ONCE) {
        let reading = batch.iter().map(|launch| async {
            match earlier.kept(&inputs, launch) {
                Some(run) => Ok(Some(run.clone())),
                None => read_launch(c, files, launch, &inputs).await,
            }
        });
        let read = futures_util::future::join_all(reading).await;
        for (launch, run) in batch.iter().zip(read) {
            let run = match run {
                Ok(Some(run)) => run,
                Ok(None) => break 'read,
                Err(why) => {
                    unread.push(crate::report::Unread {
                        launch: launch.key.clone(),
                        why,
                    });
                    continue;
                }
            };
            let Some(more) = crate::report::with_run(held, &run) else {
                break 'read;
            };
            held = more;
            runs.push((launch.clone(), run));
        }
    }
    if runs.is_empty()
        && let Some(first) = unread.first()
    {
        return Err(first.why.clone());
    }
    Ok(Gathered {
        inputs,
        launches: launches.to_vec(),
        runs,
        unread,
    })
}

/// One launch as a runs view is handed it: what it was launched with, and the files of
/// `inputs` its newest run captured, text only and each within the bytes a view reads. `None`
/// when those files are listed as holding more together than a view reads.
async fn read_launch(
    c: &Connection,
    files: Option<&Path>,
    launch: &crate::report::Launched,
    inputs: &[String],
) -> Result<Option<crucible_view::Run>, String> {
    let detail: crux::dto::PlaybookLaunch =
        within("the launch", c.client.playbook_run(&launch.key)).await?;
    let mut read = Vec::new();
    if let Some(run) = detail.runs.first() {
        let listed = captured_files(c, &run.run_id).await?;
        let named: Vec<&Captured> = listed
            .iter()
            .filter(|file| inputs.contains(&file.path) && file.size <= crate::report::FILE_BYTES)
            .collect();
        if crate::report::admit(named.iter().map(|file| (file.key.as_str(), file.size))).is_err() {
            return Ok(None);
        }
        for file in named {
            let opened = captured_file(
                c,
                files,
                run.run_id.clone(),
                file.key.clone(),
                file.size,
                launch.settled(),
            )
            .await;
            match opened.body {
                Ok(Body::Text(text)) => read.push(crucible_view::File {
                    path: file.path.clone(),
                    task: file.owner(),
                    text,
                }),
                Ok(Body::Binary(_)) => {}
                Err(error) => return Err(format!("reading {}: {error}", file.key)),
            }
        }
    }
    let params = match detail.launch.params {
        Value::Object(params) => params.into_iter().collect(),
        _ => Default::default(),
    };
    Ok(Some(crucible_view::Run {
        id: launch.key.clone(),
        status: launch.status,
        started: launch.started as f64,
        cost_usd: launch.cost,
        origin: launch.origin.clone(),
        pack_version: detail.launch.draft_version,
        params,
        files: read,
    }))
}

/// The file `run` captured under `key`: its copy on disk when the run has `settled` and the copy
/// is the `size` the listing gives, else fetched.
pub(in crate::live) async fn captured_file(
    c: &Connection,
    files: Option<&Path>,
    run: String,
    key: String,
    size: u64,
    settled: bool,
) -> Opened {
    let kept = file_on_disk(files, &run, &key).filter(|(_, bytes)| settled && *bytes == size);
    match kept {
        Some((opened, _)) => opened,
        None => fetch_file(c, files, run, key).await,
    }
}

/// A captured file's copy on disk and its length, when it has been fetched before.
pub(in crate::live) fn file_on_disk(
    files: Option<&Path>,
    run: &str,
    key: &str,
) -> Option<(Opened, u64)> {
    let disk = crate::captured::cache_path(files?, run, key);
    let bytes = std::fs::read(&disk).ok()?;
    let length = bytes.len() as u64;
    let opened = Opened {
        body: Ok(Body::from_bytes(bytes)),
        run: run.to_string(),
        key: key.to_string(),
        disk: Some(disk),
    };
    Some((opened, length))
}

/// Fetch one captured file and keep a copy under `files`.
async fn fetch_file(c: &Connection, files: Option<&Path>, run: String, key: String) -> Opened {
    let bytes = within("the file", c.client.run_file(&run, &key)).await;
    let path = files.map(|files| crate::captured::cache_path(files, &run, &key));
    let disk = match (&bytes, path) {
        (Ok(bytes), Some(path)) => path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, bytes))
            .ok()
            .map(|()| path),
        _ => None,
    };
    Opened {
        body: bytes.map(Body::from_bytes),
        run,
        key,
        disk,
    }
}
