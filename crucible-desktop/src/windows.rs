use tauri::ipc::CapabilityBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

use crate::settings::Error;
use crate::{State, local};

pub const SETTINGS: &str = "settings";
const PREFIX: &str = "p-";
const STATUS_JS: &str = include_str!("status.js");

/// Where in a profile's UI a window opens.
pub enum Target {
    Home,
    /// A path relative to the UI root, already percent-encoded, with any query.
    Path(String),
    /// A playbook launch's page.
    Launch(String),
}

pub fn label(profile: &str) -> String {
    format!("{PREFIX}{profile}")
}

pub fn profile_of(label: &str) -> Option<&str> {
    label.strip_prefix(PREFIX)
}

/// Show a profile's window, creating it (and its local controller) on first use.
pub fn open(app: &AppHandle, name: &str, target: Target) -> Result<(), Error> {
    let profile = app
        .state::<State>()
        .settings
        .lock()
        .map_err(|_| "settings poisoned")?
        .profiles
        .get(name)
        .cloned()
        .ok_or_else(|| format!("no profile named {name}"))?;
    let base = profile.base()?;
    let url = resolve(&base, &target)?;
    let label = label(name);
    grant(app, &label, &profile.origin()?)?;

    if let Some(window) = app.get_webview_window(&label) {
        if !matches!(target, Target::Home) {
            window.navigate(url)?;
        }
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }

    let port = profile.local_port();
    let booting = port.is_some_and(|port| !local::healthy(port));
    let first = if booting {
        WebviewUrl::App("index.html".into())
    } else {
        WebviewUrl::External(url.clone())
    };
    let mut builder = WebviewWindowBuilder::new(app, &label, first)
        .title(format!("Crucible · {name}"))
        .inner_size(1400.0, 900.0)
        .initialization_script(STATUS_JS.replace("__CONTROLLER_ORIGIN__", &profile.origin()?));
    if let Some(proxy) = &profile.proxy {
        builder = builder.proxy_url(Url::parse(proxy)?);
    }
    let window = builder.build()?;
    allow_swipe_navigation(&window)?;

    if let Some(port) = port {
        local::ensure(app, port)?;
        if booting {
            std::thread::spawn(move || {
                if !local::wait_healthy(port, local::BOOT_TIMEOUT) {
                    eprintln!("crucible-desktop: the controller on :{port} never became healthy");
                    return;
                }
                if let Err(err) = window.navigate(url) {
                    eprintln!("crucible-desktop: navigate: {err}");
                }
            });
        }
    }
    Ok(())
}

pub fn open_settings(app: &AppHandle) -> Result<(), Error> {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }
    WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title("Crucible Settings")
        .inner_size(760.0, 640.0)
        .build()?;
    Ok(())
}

/// The window the user is looking at, for menu commands that act on "this page".
pub fn focused(app: &AppHandle) -> Option<WebviewWindow> {
    app.webview_windows()
        .into_values()
        .find(|w| w.is_focused().unwrap_or(false))
}

fn resolve(base: &Url, target: &Target) -> Result<Url, Error> {
    Ok(match target {
        Target::Home => base.clone(),
        Target::Path(path) => base.join(path.trim_start_matches('/'))?,
        Target::Launch(key) => {
            let mut url = base.clone();
            url.path_segments_mut()
                .map_err(|_| "the profile url cannot hold a path")?
                .pop_if_empty()
                .push("playbook-runs")
                .push(key);
            url
        }
    })
}

/// The controller's pages may call the status command and the notification shim; nothing else
/// the window navigates to (the identity provider included) gets IPC.
fn grant(app: &AppHandle, label: &str, origin: &str) -> Result<(), Error> {
    let state = app.state::<State>();
    let mut granted = state
        .capabilities
        .lock()
        .map_err(|_| "capabilities poisoned")?;
    let id = format!("{label} {origin}");
    if granted.contains(&id) {
        return Ok(());
    }
    app.add_capability(
        CapabilityBuilder::new(id.clone())
            .remote(format!("{origin}/*"))
            .window(label)
            .permission("notification:default")
            .permission("allow-report-status"),
    )?;
    granted.insert(id);
    Ok(())
}

fn allow_swipe_navigation(window: &WebviewWindow) -> tauri::Result<()> {
    window.with_webview(|webview| {
        let view = webview.inner().cast::<objc2::runtime::AnyObject>();
        // SAFETY: on macOS `inner` is the window's live WKWebView.
        if let Some(view) = unsafe { view.as_ref() } {
            let () =
                unsafe { objc2::msg_send![view, setAllowsBackForwardNavigationGestures: true] };
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::windows::*;

    fn base() -> Url {
        Url::parse("https://crucible.example.com/").unwrap()
    }

    #[test]
    fn labels_round_trip() {
        assert_eq!(profile_of(&label("mpp")), Some("mpp"));
        assert_eq!(profile_of(SETTINGS), None);
    }

    #[test]
    fn targets_resolve_under_the_profile() {
        assert_eq!(
            resolve(&base(), &Target::Home).unwrap().as_str(),
            "https://crucible.example.com/"
        );
        assert_eq!(
            resolve(&base(), &Target::Path("/playbook-runs?x=1".into()))
                .unwrap()
                .as_str(),
            "https://crucible.example.com/playbook-runs?x=1"
        );
        assert_eq!(
            resolve(&base(), &Target::Launch("playbook:cve-triage:01a1".into()))
                .unwrap()
                .as_str(),
            "https://crucible.example.com/playbook-runs/playbook:cve-triage:01a1"
        );
        assert_eq!(
            resolve(&base(), &Target::Launch("a/b c".into()))
                .unwrap()
                .as_str(),
            "https://crucible.example.com/playbook-runs/a%2Fb%20c"
        );
    }
}
