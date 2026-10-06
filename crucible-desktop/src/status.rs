use serde::Deserialize;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::window::{ProgressBarState, ProgressBarStatus};
use tauri::{AppHandle, Manager, WebviewWindow, Wry};
use tauri_plugin_notification::NotificationExt;

use crate::settings::Error;
use crate::{State, windows};

pub const TRAY: &str = "tray";

/// One active launch, as the page's status script summarizes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RunStatus {
    pub key: String,
    pub playbook: String,
    pub cost_usd: f64,
    pub elapsed: String,
    /// The larger of spend over the cost ceiling and elapsed over the time ceiling, in [0, 1].
    pub progress: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileStatus {
    pub approvals: u32,
    /// Open decision requests the user may answer; the page notifies for these itself.
    pub decisions: u32,
    pub runs: Vec<RunStatus>,
}

/// The launch whose budget the Dock icon shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Pinned {
    pub profile: String,
    pub key: String,
    pub playbook: String,
}

#[tauri::command]
pub fn report_status(
    app: AppHandle,
    window: WebviewWindow,
    approvals: u32,
    decisions: u32,
    runs: Vec<RunStatus>,
) -> Result<(), String> {
    let profile = windows::profile_of(window.label())
        .ok_or("not a profile window")?
        .to_string();
    let state = app.state::<State>();
    let (previous, total) = {
        let mut status = state.status.lock().map_err(|_| "status poisoned")?;
        let entry = status.entry(profile.clone()).or_default();
        let previous = std::mem::replace(&mut entry.approvals, approvals);
        entry.decisions = decisions;
        entry.runs = runs.clone();
        (
            previous,
            status
                .values()
                .map(|s| s.approvals + s.decisions)
                .sum::<u32>(),
        )
    };

    if let Err(err) = window.set_badge_count((total > 0).then_some(i64::from(total))) {
        eprintln!("crucible-desktop: badge: {err}");
    }
    if approvals > previous {
        notify(&app, &format!("{profile}: {approvals} awaiting approval"));
    }
    update_pin(&app, &window, &profile, &runs)?;
    refresh_tray(&app).map_err(|err| err.to_string())
}

pub fn toggle_pin(app: &AppHandle, profile: &str, key: &str) -> Result<(), Error> {
    let state = app.state::<State>();
    let run = state
        .status
        .lock()
        .map_err(|_| "status poisoned")?
        .get(profile)
        .and_then(|s| s.runs.iter().find(|r| r.key == key).cloned());
    {
        let mut pinned = state.pinned.lock().map_err(|_| "pin poisoned")?;
        let unpin = pinned
            .as_ref()
            .is_some_and(|p| p.profile == profile && p.key == key);
        *pinned = match (unpin, &run) {
            (false, Some(run)) => Some(Pinned {
                profile: profile.to_string(),
                key: key.to_string(),
                playbook: run.playbook.clone(),
            }),
            _ => None,
        };
    }
    if let Some(window) = app.get_webview_window(&windows::label(profile)) {
        update_pin(app, &window, profile, run.as_slice())?;
    }
    refresh_tray(app)
}

fn update_pin(
    app: &AppHandle,
    window: &WebviewWindow,
    profile: &str,
    runs: &[RunStatus],
) -> Result<(), String> {
    let state = app.state::<State>();
    let mut pinned = state.pinned.lock().map_err(|_| "pin poisoned")?;
    let Some(pin) = pinned.as_ref().filter(|p| p.profile == profile).cloned() else {
        if pinned.is_none() {
            set_progress(window, None);
        }
        return Ok(());
    };
    match runs.iter().find(|r| r.key == pin.key) {
        Some(run) => set_progress(window, Some(run.progress)),
        None => {
            *pinned = None;
            set_progress(window, None);
            notify(app, &format!("{profile}: {} finished", pin.playbook));
        }
    }
    Ok(())
}

fn set_progress(window: &WebviewWindow, progress: Option<f64>) {
    let state = match progress {
        Some(fraction) => ProgressBarState {
            status: Some(ProgressBarStatus::Normal),
            progress: Some(percent(fraction)),
        },
        None => ProgressBarState {
            status: Some(ProgressBarStatus::None),
            progress: None,
        },
    };
    if let Err(err) = window.set_progress_bar(state) {
        eprintln!("crucible-desktop: dock progress: {err}");
    }
}

fn percent(fraction: f64) -> u64 {
    if fraction.is_finite() {
        (fraction.clamp(0.0, 1.0) * 100.0).round() as u64
    } else {
        0
    }
}

pub fn notify(app: &AppHandle, body: &str) {
    if let Err(err) = app
        .notification()
        .builder()
        .title("Crucible")
        .body(body)
        .show()
    {
        eprintln!("crucible-desktop: notification: {err}");
    }
}

/// Menu ids carry their arguments after tabs, which no profile name or launch key contains.
pub fn menu_id(parts: &[&str]) -> String {
    parts.join("\t")
}

pub fn refresh_tray(app: &AppHandle) -> Result<(), Error> {
    let Some(tray) = app.tray_by_id(TRAY) else {
        return Ok(());
    };
    let state = app.state::<State>();
    let profiles: Vec<String> = state
        .settings
        .lock()
        .map_err(|_| "settings poisoned")?
        .profiles
        .keys()
        .cloned()
        .collect();
    let status = state.status.lock().map_err(|_| "status poisoned")?.clone();
    let pinned = state.pinned.lock().map_err(|_| "pin poisoned")?.clone();

    let menu = Menu::new(app)?;
    for name in &profiles {
        let open = MenuItem::with_id(
            app,
            menu_id(&["profile", name]),
            format!("Open {name}"),
            true,
            None::<&str>,
        )?;
        menu.append(&open)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let mut any = false;
    for (profile, s) in &status {
        for run in &s.runs {
            any = true;
            menu.append(&run_submenu(app, profile, run, pinned.as_ref())?)?;
        }
    }
    if !any {
        menu.append(&MenuItem::new(
            app,
            "No running playbooks",
            false,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "settings",
        "Settings…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit Crucible",
        true,
        None::<&str>,
    )?)?;
    tray.set_menu(Some(menu))?;

    let total: u32 = status.values().map(|s| s.approvals + s.decisions).sum();
    tray.set_tooltip(Some(format!("Crucible: {total} awaiting approval")))?;
    Ok(())
}

fn run_submenu(
    app: &AppHandle,
    profile: &str,
    run: &RunStatus,
    pinned: Option<&Pinned>,
) -> Result<Submenu<Wry>, Error> {
    let shown = pinned.is_some_and(|p| p.profile == profile && p.key == run.key);
    let title = format!(
        "{} · {profile} · ${:.2} · {} · {}%",
        run.playbook,
        run.cost_usd,
        run.elapsed,
        percent(run.progress)
    );
    Ok(Submenu::with_items(
        app,
        title,
        true,
        &[
            &MenuItem::with_id(
                app,
                menu_id(&["run", profile, &run.key]),
                "Open",
                true,
                None::<&str>,
            )?,
            &CheckMenuItem::with_id(
                app,
                menu_id(&["pin", profile, &run.key]),
                "Show budget on Dock",
                true,
                shown,
                None::<&str>,
            )?,
        ],
    )?)
}

#[cfg(test)]
mod tests {
    use crate::status::*;

    #[test]
    fn percent_clamps_and_survives_nan() {
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(0.426), 43);
        assert_eq!(percent(1.7), 100);
        assert_eq!(percent(-1.0), 0);
        assert_eq!(percent(f64::NAN), 0);
        assert_eq!(percent(f64::INFINITY), 0);
    }

    #[test]
    fn menu_ids_split_back_into_their_parts() {
        let id = menu_id(&["run", "mpp", "playbook:cve-triage:01a1"]);
        let parts: Vec<&str> = id.splitn(3, '\t').collect();
        assert_eq!(parts, ["run", "mpp", "playbook:cve-triage:01a1"]);
    }
}
