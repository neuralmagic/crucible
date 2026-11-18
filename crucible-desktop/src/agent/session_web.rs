//! The browser cannot run a sandbox, so a session there never starts.

use rune::Waker;

use crate::agent::{Spec, Update};
use crate::net::Handle;
use crate::profile::Target;

pub struct Session {
    draft: String,
}

impl Session {
    pub fn start(
        _runtime: Result<Handle, String>,
        _target: Target,
        spec: Spec,
        _waker: Option<Waker>,
    ) -> Session {
        Session { draft: spec.draft }
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn ask(&self, _prompt: String) {}

    pub fn stop(&self) {}

    pub fn take(&self, _seen: u64) -> Option<Update> {
        None
    }
}
