# crucible-desktop

macOS app around the controller UI, on Tauri 2. It is its own Cargo workspace, so the root
workspace does not build it. `just desktop-check` formats, lints and tests it; the Desktop workflow
runs the same on macOS when anything under `crucible-desktop/` changes.

```sh
just desktop-install
```

That builds `Crucible.app` and installs it in `~/Applications`. For a dev loop, `cargo run` in this
directory.

Each release also carries `Crucible-<tag>-aarch64-apple-darwin.zip`. The app is signed ad hoc, so
macOS blocks a downloaded copy on first launch: allow it from System Settings, Privacy & Security,
Open Anyway, or run `xattr -d com.apple.quarantine /Applications/Crucible.app`.

Settings live in `~/.config/crucible-desktop/config.toml` (under `$XDG_CONFIG_HOME` when set),
edited from Settings (Cmd-,). With no profiles in it, the app makes one named `default` from crux's
resolution: `CONTROLLER_URL`, then `~/.config/crux/config.toml`, then `http://127.0.0.1:8870`.

```toml
default_profile = "mpp"
repo = "/Users/you/git/crucible"       # default: the checkout the app was built in

[profiles.local]
url = "http://127.0.0.1:8870"

[profiles.mpp]
url = "https://crucible.example.com"   # the UI host; sign-in completes only there
api = "crux"                           # calls the app makes use crux's URL and key

[profiles.jump]
url = "https://crucible.internal.example.com"
proxy = "socks5://127.0.0.1:1080"      # every request the window makes goes through it

[drop]
profile = "local"   # default: the first local profile
max_cost = 1.0
max_time = "30m"
```

- Each profile opens in its own window (Profile menu, Cmd-1 to Cmd-9). A loopback profile starts
  `just controller-local <port>` in `repo`, with your login shell's PATH; that controller lives
  exactly as long as the app process, including a crash or SIGKILL.
- A remote profile signs in through its normal SSO flow inside the window. The session persists
  in the app's own WebKit store, apart from your browser's.
- History menu (Cmd-[, Cmd-], Cmd-R) and two-finger swipe move through a window's history.
- The tray menu lists each profile's running playbooks with spend, elapsed time, and budget used.
  "Show budget on Dock" puts that launch's budget (the larger of spend over max cost and elapsed
  over max time) on the Dock icon, and a notification says when it finishes.
- The Dock badge counts pack approvals, pending imports and open decision requests across
  profiles. A rising approval count raises a notification; the UI raises its own for decision
  requests, through the web `Notification` API, which the app backs with native notifications.
- `crucible://<profile>/<path>` opens that page in that profile; a host naming no profile opens
  the path in the default one. `crucible://open?url=<ui url>` opens a UI link in the profile on
  its origin, and refuses a URL on any other origin.
- Dropping a pack folder on the Dock icon pushes it as the next version of the draft named after
  the folder (created on first drop), test-fires it on the drop profile under the `[drop]`
  ceilings, and opens the launch.
- Closing a window hides it; the tray icon and the Dock reopen it.
- Only the controller's own pages get the app's commands (the status report and notifications);
  any other page a window navigates to, the identity provider included, gets none.
