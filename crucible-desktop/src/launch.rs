use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::settings::Error;
use crate::status::notify;
use crate::windows::{self, Target};
use crate::{State, local};

/// Push a pack directory dropped on the Dock icon as the next version of the draft named after
/// it, creating the draft on first drop, and test-fire it on the drop profile.
pub fn dropped(app: &AppHandle, dir: PathBuf) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let name = dir.display().to_string();
        match launch(&app, &dir).await {
            Ok((profile, draft, key)) => {
                notify(&app, &format!("{profile}: launched {draft}"));
                if let Err(err) = windows::open(&app, &profile, Target::Launch(key)) {
                    eprintln!("crucible-desktop: opening the launch: {err}");
                }
            }
            Err(err) => notify(&app, &format!("Launching {name} failed: {err}")),
        }
    });
}

async fn launch(app: &AppHandle, dir: &Path) -> Result<(String, String, String), Error> {
    let draft = draft_id(dir).ok_or("the folder name holds no letters or digits")?;
    let (profile_name, profile, drop) = {
        let settings = app
            .state::<State>()
            .settings
            .lock()
            .map_err(|_| "settings poisoned")?
            .clone();
        let name = settings
            .drop_name()
            .ok_or("no profile to launch on")?
            .to_string();
        let profile = settings
            .profiles
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("no profile named {name}"))?;
        (name, profile, settings.drop)
    };

    if let Some(port) = profile.local_port() {
        local::ensure(app, port)?;
        let ready = tauri::async_runtime::spawn_blocking(move || {
            local::wait_healthy(port, local::BOOT_TIMEOUT)
        })
        .await?;
        if !ready {
            return Err(format!("the controller on :{port} never became healthy").into());
        }
    }

    let client = crux::client::Client::connect(&profile.api_config()?).map_err(stringify)?;
    let version = match client
        .draft_files::<crux::dto::DraftFiles>(&draft, None)
        .await
    {
        Ok(files) => files.version,
        Err(_) => {
            client
                .create_draft::<crux::dto::DraftCompile>(
                    &draft,
                    &format!("Dropped on Crucible from {}", dir.display()),
                    None,
                )
                .await
                .map_err(stringify)?
                .version
        }
    };
    crux::ops::draft_push(&client, &draft, dir, version, true)
        .await
        .map_err(stringify)?;
    let ack: crux::dto::LaunchAck = client
        .launch_draft(
            &draft,
            &serde_json::json!({
                "params": {},
                "max_cost": drop.max_cost,
                "max_time": drop.max_time,
            }),
        )
        .await
        .map_err(stringify)?;
    Ok((profile_name, draft, ack.key))
}

fn stringify(err: anyhow::Error) -> Error {
    format!("{err:#}").into()
}

/// The folder's name as a draft id: lowercase letters, digits and single dashes.
fn draft_id(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_string_lossy().to_lowercase();
    let mut id = String::new();
    for c in name.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            id.push(c);
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    let id = id.trim_end_matches('-').to_string();
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use crate::launch::*;

    #[test]
    fn draft_ids_from_folder_names() {
        let id = |p: &str| draft_id(Path::new(p));
        assert_eq!(id("/x/cve-triage-llm-d"), Some("cve-triage-llm-d".into()));
        assert_eq!(id("/x/My Pack_v2"), Some("my-pack-v2".into()));
        assert_eq!(id("/x/--weird--"), Some("weird".into()));
        assert_eq!(id("/x/___"), None);
        assert_eq!(id("/"), None);
    }
}
