//! The green glow on a call frame whose target run just passed: lit for one frame, then fading
//! until it is out.
//!
//! ```text
//!   passed ──▶ Lit ──tick──▶ Fading until `until` ──tick at `until`──▶ gone
//! ```

use std::collections::HashMap;

use crucible_contract::calls::{CallId, CallStatus, CallView};

/// Seconds a passed frame's glow takes to fade.
pub const FADE: f64 = 3.0;

/// How a passed frame's glow draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Glow {
    /// Full, for one frame, so the next one has something to fade from.
    Lit,
    /// Fading out.
    Fading,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Lit,
    Fading { until: f64 },
}

/// Every call frame still glowing, by call.
#[derive(Debug, Default)]
pub struct Afterglows {
    calls: HashMap<CallId, Step>,
}

impl Afterglows {
    /// Lights each call `after` shows passed that `before` showed still going.
    pub fn saw(&mut self, before: &[CallView], after: &[CallView]) {
        for call in after
            .iter()
            .filter(|call| call.state.status == CallStatus::Passed)
        {
            let was_going = before
                .iter()
                .find(|old| old.state.call == call.state.call)
                .is_some_and(|old| !old.state.status.ended());
            if was_going {
                self.calls.insert(call.state.call.clone(), Step::Lit);
            }
        }
    }

    /// Steps every glow on the window clock; true when what they draw changed.
    pub fn tick(&mut self, clock: f64) -> bool {
        let mut changed = false;
        self.calls.retain(|_, step| match *step {
            Step::Lit => {
                *step = Step::Fading {
                    until: clock + FADE,
                };
                changed = true;
                true
            }
            Step::Fading { until } if clock >= until => {
                changed = true;
                false
            }
            Step::Fading { .. } => true,
        });
        changed
    }

    /// When the poll must run again: the next frame while one is lit, else when the first
    /// fading one goes out.
    pub fn due(&self, clock: f64) -> Option<f64> {
        self.calls
            .values()
            .map(|step| match step {
                Step::Lit => clock,
                Step::Fading { until } => *until,
            })
            .min_by(f64::total_cmp)
    }

    /// The glow on `call`'s frame, if it has one.
    pub fn glow(&self, call: &CallId) -> Option<Glow> {
        self.calls.get(call).map(|step| match step {
            Step::Lit => Glow::Lit,
            Step::Fading { .. } => Glow::Fading,
        })
    }
}

#[cfg(test)]
mod tests {
    use crucible_contract::calls::{CallId, CallView};
    use serde_json::json;

    use crate::afterglow::{Afterglows, FADE, Glow};

    fn call(id: &str, status: &str) -> CallView {
        serde_json::from_value(json!({
            "task": "ship",
            "attempt": 1,
            "alias": "release",
            "kind": "playbook",
            "state": {
                "call": id,
                "status": status,
                "handle": {"kind": "playbook", "run": "r"},
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_call_seen_passing_glows_for_a_frame_then_fades_out() {
        let mut glows = Afterglows::default();
        let id = CallId("c-1".into());
        glows.saw(&[call("c-1", "running")], &[call("c-1", "passed")]);
        assert_eq!(glows.glow(&id), Some(Glow::Lit));
        assert_eq!(glows.due(5.0), Some(5.0), "lit asks for the next frame");

        assert!(glows.tick(5.0));
        assert_eq!(glows.glow(&id), Some(Glow::Fading));
        assert_eq!(glows.due(5.1), Some(5.0 + FADE));
        assert!(!glows.tick(5.0 + FADE / 2.0), "still fading");

        assert!(glows.tick(5.0 + FADE));
        assert_eq!(glows.glow(&id), None);
        assert_eq!(glows.due(9.0), None);
        assert!(!glows.tick(9.0));
    }

    #[test]
    fn only_a_pass_the_window_saw_happen_glows() {
        let mut glows = Afterglows::default();
        glows.saw(&[], &[call("c-1", "passed")]);
        glows.saw(&[call("c-2", "passed")], &[call("c-2", "passed")]);
        glows.saw(&[call("c-3", "running")], &[call("c-3", "failed")]);
        glows.saw(&[call("c-4", "pending")], &[call("c-5", "passed")]);
        for id in ["c-1", "c-2", "c-3", "c-5"] {
            assert_eq!(glows.glow(&CallId(id.into())), None, "{id}");
        }
        glows.saw(&[call("c-4", "pending")], &[call("c-4", "passed")]);
        assert_eq!(glows.glow(&CallId("c-4".into())), Some(Glow::Lit));
    }
}
