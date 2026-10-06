mod launch;
mod local;
mod settings;
mod status;
mod windows;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Child;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use url::Url;

use crate::settings::{Error, Settings};
use crate::status::{Pinned, ProfileStatus, menu_id};
use crate::windows::Target;

pub struct State {
    pub settings: Mutex<Settings>,
    pub controllers: Mutex<HashMap<u16, Child>>,
    pub capabilities: Mutex<HashSet<String>>,
    pub status: Mutex<BTreeMap<String, ProfileStatus>>,
    pub pinned: Mutex<Option<Pinned>>,
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, State>) -> Result<Settings, String> {
    state
        .settings
        .lock()
        .map(|s| s.clone())
        .map_err(|_| "settings poisoned".to_string())
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    settings.save().map_err(|err| err.to_string())?;
    {
        let state = app.state::<State>();
        let mut current = state.settings.lock().map_err(|_| "settings poisoned")?;
        *current = settings;
    }
    rebuild_menus(&app).map_err(|err| err.to_string())
}

fn rebuild_menus(app: &AppHandle) -> Result<(), Error> {
    let profiles: Vec<String> = app
        .state::<State>()
        .settings
        .lock()
        .map_err(|_| "settings poisoned")?
        .profiles
        .keys()
        .cloned()
        .collect();
    let profile_menu = Submenu::new(app, "Profile", true)?;
    for (index, name) in profiles.iter().enumerate() {
        let accelerator = (index < 9).then(|| format!("CmdOrCtrl+{}", index + 1));
        profile_menu.append(&MenuItem::with_id(
            app,
            menu_id(&["profile", name]),
            name,
            true,
            accelerator.as_deref(),
        )?)?;
    }
    profile_menu.append(&PredefinedMenuItem::separator(app)?)?;
    profile_menu.append(&MenuItem::with_id(
        app,
        "settings",
        "Settings…",
        true,
        Some("CmdOrCtrl+,"),
    )?)?;
    let history = Submenu::with_items(
        app,
        "History",
        true,
        &[
            &MenuItem::with_id(app, "back", "Back", true, Some("CmdOrCtrl+["))?,
            &MenuItem::with_id(app, "forward", "Forward", true, Some("CmdOrCtrl+]"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "reload", "Reload", true, Some("CmdOrCtrl+R"))?,
        ],
    )?;
    let menu = Menu::default(app)?;
    menu.append(&profile_menu)?;
    menu.append(&history)?;
    app.set_menu(menu)?;
    status::refresh_tray(app)
}

fn on_menu(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref().to_string();
    let parts: Vec<&str> = id.splitn(3, '\t').collect();
    let result = match parts.as_slice() {
        ["back"] => page_script(app, "history.back()"),
        ["forward"] => page_script(app, "history.forward()"),
        ["reload"] => page_script(app, "location.reload()"),
        ["settings"] => windows::open_settings(app),
        ["quit"] => {
            app.exit(0);
            Ok(())
        }
        ["profile", name] => windows::open(app, name, Target::Home),
        ["run", profile, key] => windows::open(app, profile, Target::Launch(key.to_string())),
        ["pin", profile, key] => status::toggle_pin(app, profile, key),
        _ => Ok(()),
    };
    if let Err(err) = result {
        eprintln!("crucible-desktop: {id}: {err}");
    }
}

fn page_script(app: &AppHandle, script: &str) -> Result<(), Error> {
    if let Some(window) = windows::focused(app) {
        window.eval(script)?;
    }
    Ok(())
}

fn open_link(app: &AppHandle, link: &Url) -> Result<(), Error> {
    let settings = app
        .state::<State>()
        .settings
        .lock()
        .map_err(|_| "settings poisoned")?
        .clone();
    let (profile, path) = link_target(&settings, link)?;
    windows::open(app, &profile, Target::Path(path))
}

/// The profile and path a `crucible://` link opens. `crucible://<profile>/<path>` opens that page
/// in the profile; a host that names no profile opens the path in the default one.
/// `crucible://open?url=<ui url>` opens a UI link in the profile whose origin it is on, and
/// refuses a URL on any other origin.
fn link_target(settings: &Settings, link: &Url) -> Result<(String, String), Error> {
    let host = link.host_str().unwrap_or_default();
    let with_query = |path: &str, query: Option<&str>| match query {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    if !settings.profiles.contains_key(host) && host == "open" {
        let target = link
            .query_pairs()
            .find(|(k, _)| k == "url")
            .map(|(_, v)| Url::parse(&v))
            .transpose()?
            .ok_or("crucible://open needs ?url=")?;
        let origin = target.origin().ascii_serialization();
        let profile = settings
            .by_origin(&origin)
            .ok_or_else(|| format!("no profile is on {origin}"))?;
        return Ok((
            profile.to_string(),
            with_query(target.path(), target.query()),
        ));
    }
    if settings.profiles.contains_key(host) {
        return Ok((host.to_string(), with_query(link.path(), link.query())));
    }
    let default = settings.default_name().ok_or("no profiles")?.to_string();
    Ok((
        default,
        with_query(&format!("{host}{}", link.path()), link.query()),
    ))
}

fn on_opened(app: &AppHandle, urls: Vec<Url>) {
    for url in urls {
        let result = match url.scheme() {
            "crucible" => open_link(app, &url),
            "file" => match url.to_file_path() {
                Ok(path) if path.is_dir() => {
                    launch::dropped(app, path);
                    Ok(())
                }
                _ => Err(format!("{url} is not a folder").into()),
            },
            other => Err(format!("unhandled {other} link").into()),
        };
        if let Err(err) = result {
            status::notify(app, &err.to_string());
        }
    }
}

fn open_default(app: &AppHandle) -> Result<(), Error> {
    let name = app
        .state::<State>()
        .settings
        .lock()
        .map_err(|_| "settings poisoned")?
        .default_name()
        .map(str::to_string);
    match name {
        Some(name) => windows::open(app, &name, Target::Home),
        None => windows::open_settings(app),
    }
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    setup_app(app).map_err(|err| -> Box<dyn std::error::Error> { err })
}

fn setup_app(app: &mut tauri::App) -> Result<(), Error> {
    let mut tray = TrayIconBuilder::with_id(status::TRAY)
        .tooltip("Crucible")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
                && let Err(err) = open_default(tray.app_handle())
            {
                eprintln!("crucible-desktop: {err}");
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    rebuild_menus(app.handle())?;
    open_default(app.handle())?;
    Ok(())
}

fn main() -> Result<(), Error> {
    let settings = Settings::load()?;
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(State {
            settings: Mutex::new(settings),
            controllers: Mutex::default(),
            capabilities: Mutex::default(),
            status: Mutex::default(),
            pinned: Mutex::default(),
        })
        .invoke_handler(tauri::generate_handler![
            status::report_status,
            get_settings,
            save_settings
        ])
        .on_menu_event(on_menu)
        .setup(setup)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && windows::profile_of(window.label()).is_some()
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?;
    app.run(|app, event| match event {
        RunEvent::Exit => local::stop_all(app),
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            if let Err(err) = open_default(app) {
                eprintln!("crucible-desktop: {err}");
            }
        }
        RunEvent::Opened { urls } => on_opened(app, urls),
        _ => {}
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::*;

    fn settings() -> Settings {
        toml::from_str(
            r#"
default_profile = "mpp"

[profiles.local]
url = "http://127.0.0.1:8870"

[profiles.mpp]
url = "https://crucible.example.com"
"#,
        )
        .unwrap()
    }

    fn target(link: &str) -> Result<(String, String), Error> {
        link_target(&settings(), &Url::parse(link).unwrap())
    }

    #[test]
    fn a_profile_link_opens_its_path_in_that_profile() {
        assert_eq!(
            target("crucible://local/decisions/abc?x=1").unwrap(),
            ("local".to_string(), "/decisions/abc?x=1".to_string())
        );
    }

    #[test]
    fn a_link_naming_no_profile_opens_in_the_default() {
        assert_eq!(
            target("crucible://approvals").unwrap(),
            ("mpp".to_string(), "approvals".to_string())
        );
        assert_eq!(
            target("crucible://playbook-runs/k?y=2").unwrap(),
            ("mpp".to_string(), "playbook-runs/k?y=2".to_string())
        );
    }

    #[test]
    fn an_open_link_lands_in_the_profile_on_its_origin() {
        let link =
            "crucible://open?url=https%3A%2F%2Fcrucible.example.com%2Fdecisions%2Fd1%3Fa%3Db";
        assert_eq!(
            target(link).unwrap(),
            ("mpp".to_string(), "/decisions/d1?a=b".to_string())
        );
        assert_eq!(
            target("crucible://open?url=http%3A%2F%2F127.0.0.1%3A8870%2Fruns").unwrap(),
            ("local".to_string(), "/runs".to_string())
        );
    }

    #[test]
    fn an_open_link_off_every_profile_origin_is_refused() {
        for link in [
            "crucible://open?url=https%3A%2F%2Fevil.example.com%2Fdecisions",
            "crucible://open?url=http%3A%2F%2Fcrucible.example.com%2F",
            "crucible://open?url=https%3A%2F%2Fcrucible.example.com%3A8443%2F",
            "crucible://open?url=not%20a%20url",
            "crucible://open",
        ] {
            assert!(target(link).is_err(), "{link}");
        }
    }
}
