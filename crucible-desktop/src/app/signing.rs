//! Signing in to the profile in use.

use crate::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::signin;
use rune::TextEdit;

impl App {
    /// Sign in through the controller's own page, which sets the session cookie and comes back
    /// to this one.
    #[cfg(target_arch = "wasm32")]
    pub fn sign_in(&mut self) {
        let here = web_sys::window()
            .and_then(|window| window.location().pathname().ok())
            .unwrap_or_else(|| "/".into());
        if let Err(why) = rune::web::open_url_same_tab(&format!("/auth/login?rd={here}")) {
            self.remote.sign_in_error = Some(why);
        }
    }

    /// Sign in to the profile in use through the browser, on a thread of its own since it waits
    /// for the person.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sign_in(&mut self) {
        if self.remote.signing_in {
            return;
        }
        let target = self.remote.target.clone();
        let waker = self.waker.clone();
        let runtime = self.net.handle();
        let (tx, rx) = std::sync::mpsc::channel();
        let (cancel, stop) = signin::Cancel::new();
        self.remote.signing_in = true;
        self.remote.sign_in_error = None;
        self.remote.sign_in_done = Some(rx);
        self.remote.sign_in_cancel = Some(cancel);
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-sign-in".into())
            .spawn(move || {
                let _ = tx.send(signin::sign_in(&target, runtime, &stop));
                if let Some(waker) = waker {
                    waker();
                }
            });
        if let Err(e) = spawned {
            self.cancel_sign_in();
            self.remote.sign_in_error = Some(format!("starting the sign-in: {e}"));
        }
    }

    /// Stop waiting on the browser.
    pub fn cancel_sign_in(&mut self) {
        self.remote.signing_in = false;
        self.remote.sign_in_done = None;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.remote.sign_in_cancel = None;
        }
    }

    /// Take a finished sign-in's answer; a new key means connecting again.
    pub(crate) fn finish_sign_in(&mut self) -> bool {
        let Some(answer) = self
            .remote
            .sign_in_done
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        else {
            return false;
        };
        self.cancel_sign_in();
        match answer {
            Ok(()) => self.reconnect(),
            Err(why) => self.remote.sign_in_error = Some(why),
        }
        true
    }

    /// Keep the pasted key as the profile's, and connect with it.
    pub fn save_pasted_key(&mut self) {
        let key = self.remote.key_paste.text.trim().to_string();
        if key.is_empty() {
            return;
        }
        match self.remote.target.save_key(key) {
            Ok(()) => {
                self.remote.key_paste = TextEdit::new("");
                self.remote.sign_in_error = None;
                self.reconnect();
            }
            Err(why) => self.remote.sign_in_error = Some(why),
        }
    }

    fn reconnect(&mut self) {
        self.remote.live_client.reconnect();
        self.remote.admin_client.reconnect();
    }
}
