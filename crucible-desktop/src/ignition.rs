//! The flare on a run just launched: lit for one frame, then spent while its ring grows and
//! fades, then out.
//!
//! ```text
//! Out ──launched──▶ Lit ──tick──▶ Spent ──tick at `until`──▶ Out
//!                    ▲                │
//!                    └────launched────┘
//! ```

/// Seconds the spent ring takes to grow and fade.
pub const BURN: f64 = 0.9;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Ignition {
    #[default]
    Out,
    /// Drawn small and bright for one frame, so the next one has something to spring from.
    Lit { playbook: String },
    /// Springing outward and fading until `until` on the window clock.
    Spent { playbook: String, until: f64 },
}

/// How the flare draws on a playbook's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spark {
    Lit,
    Spent,
}

impl Ignition {
    /// A launch of `playbook` landed.
    pub fn launched(&mut self, playbook: String) {
        *self = Ignition::Lit { playbook };
    }

    /// Steps the flare on the window clock; true when what it draws changed.
    pub fn tick(&mut self, clock: f64) -> bool {
        match self {
            Ignition::Out => false,
            Ignition::Lit { playbook } => {
                *self = Ignition::Spent {
                    playbook: std::mem::take(playbook),
                    until: clock + BURN,
                };
                true
            }
            Ignition::Spent { until, .. } if clock >= *until => {
                *self = Ignition::Out;
                true
            }
            Ignition::Spent { .. } => false,
        }
    }

    /// When the poll must run again for the next step: the next frame while lit, the end of the
    /// burn while spent.
    pub fn due(&self, clock: f64) -> Option<f64> {
        match self {
            Ignition::Out => None,
            Ignition::Lit { .. } => Some(clock),
            Ignition::Spent { until, .. } => Some(*until),
        }
    }

    /// The flare on `playbook`'s row, if it has one.
    pub fn spark(&self, playbook: &str) -> Option<Spark> {
        match self {
            Ignition::Lit { playbook: p } if p == playbook => Some(Spark::Lit),
            Ignition::Spent { playbook: p, .. } if p == playbook => Some(Spark::Spent),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ignition::{BURN, Ignition, Spark};

    #[test]
    fn a_launch_lights_one_frame_then_burns_out() {
        let mut flare = Ignition::default();
        assert!(!flare.tick(1.0));
        assert_eq!(flare.due(1.0), None);

        flare.launched("triage".into());
        assert_eq!(flare.spark("triage"), Some(Spark::Lit));
        assert_eq!(flare.due(2.0), Some(2.0), "lit asks for the next frame");

        assert!(flare.tick(2.0));
        assert_eq!(flare.spark("triage"), Some(Spark::Spent));
        assert_eq!(flare.due(2.1), Some(2.0 + BURN));

        assert!(!flare.tick(2.0 + BURN / 2.0), "still burning");
        assert_eq!(flare.spark("triage"), Some(Spark::Spent));

        assert!(flare.tick(2.0 + BURN));
        assert_eq!(flare, Ignition::Out);
        assert_eq!(flare.spark("triage"), None);
    }

    #[test]
    fn only_the_launched_playbook_sparks() {
        let mut flare = Ignition::default();
        flare.launched("triage".into());
        assert_eq!(flare.spark("backport"), None);
        flare.tick(0.0);
        assert_eq!(flare.spark("backport"), None);
    }

    #[test]
    fn a_second_launch_relights_mid_burn() {
        let mut flare = Ignition::default();
        flare.launched("triage".into());
        flare.tick(0.0);
        flare.launched("backport".into());
        assert_eq!(flare.spark("triage"), None);
        assert_eq!(flare.spark("backport"), Some(Spark::Lit));
    }
}
