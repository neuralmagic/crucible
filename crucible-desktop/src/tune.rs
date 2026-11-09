//! Values tuned live with the knobs panel (⌘⇧K in a `--features knobs` build). Without the
//! feature every knob reads its default, so a shipped build runs on these constants.

use rune_knobs::{Group, Knob, Knobs};

pub const GRAIN: Knob = Knob::new("Film grain", 0.0, 0.15, 0.035).about("Grain over the window");
pub const PULSE: Knob = Knob::new("Pulse", 0.0, 1.5, 0.6).about("The ring behind running marks");
pub const PULSE_BLEED: Knob = Knob::new("Pulse reach", 0.0, 32.0, 14.0).step(1.0);
pub const SHIMMER: Knob =
    Knob::new("Card shimmer", 0.0, 0.4, 0.08).about("The sweep over a running task card");
pub const BAR_SHIMMER: Knob =
    Knob::new("Bar shimmer", 0.0, 1.0, 0.35).about("The sweep over a running timeline bar");
pub const GLOW: Knob = Knob::new("Drop glow", 0.0, 1.5, 0.6).about("Where a dragged pane lands");
pub const GLOW_BLEED: Knob = Knob::new("Drop glow reach", 0.0, 40.0, 16.0).step(1.0);
pub const SIDEBAR: Knob = Knob::new("Sidebar width", 220.0, 420.0, 288.0).step(1.0);
pub const TIMELINE_ROW: Knob = Knob::new("Timeline row", 18.0, 40.0, 26.0).step(1.0);
pub const TIMELINE_LABEL: Knob = Knob::new("Timeline labels", 90.0, 260.0, 150.0).step(1.0);

pub const GROUPS: &[Group] = &[
    Group::new(
        "Effects",
        "window",
        &[
            GRAIN,
            PULSE,
            PULSE_BLEED,
            SHIMMER,
            BAR_SHIMMER,
            GLOW,
            GLOW_BLEED,
        ],
    ),
    Group::new("Sidebar", "sidebar", &[SIDEBAR]),
    Group::new("Timeline", "timeline", &[TIMELINE_ROW, TIMELINE_LABEL]),
];

/// The knobs' values as one frame reads them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tune {
    pub grain: f32,
    pub pulse: f32,
    pub pulse_bleed: f32,
    pub shimmer: f32,
    pub bar_shimmer: f32,
    pub glow: f32,
    pub glow_bleed: f32,
    pub sidebar: f32,
    pub timeline_row: f32,
    pub timeline_label: f32,
}

impl Tune {
    pub fn read(k: &Knobs) -> Tune {
        Tune {
            grain: k.get(&GRAIN),
            pulse: k.get(&PULSE),
            pulse_bleed: k.get(&PULSE_BLEED),
            shimmer: k.get(&SHIMMER),
            bar_shimmer: k.get(&BAR_SHIMMER),
            glow: k.get(&GLOW),
            glow_bleed: k.get(&GLOW_BLEED),
            sidebar: k.get(&SIDEBAR),
            timeline_row: k.get(&TIMELINE_ROW),
            timeline_label: k.get(&TIMELINE_LABEL),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tune::*;

    #[test]
    fn untouched_knobs_read_the_shipped_constants() {
        let tune = Tune::read(&Knobs::default());
        assert_eq!(tune.grain, 0.035);
        assert_eq!(tune.sidebar, 288.0);
        assert_eq!(tune.timeline_row, 26.0);
        assert_eq!(tune.timeline_label, 150.0);
    }

    #[test]
    fn every_knob_default_sits_inside_its_range_and_names_are_unique() {
        let mut names = std::collections::HashSet::new();
        for group in GROUPS {
            for knob in group.knobs {
                assert!(
                    knob.min <= knob.default && knob.default <= knob.max,
                    "{}",
                    knob.name
                );
                assert!(names.insert(knob.name), "{} twice", knob.name);
            }
        }
    }
}
