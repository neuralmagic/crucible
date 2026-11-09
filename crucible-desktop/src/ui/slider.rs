//! A horizontal slider: drag or click the track, or step it with the arrow keys once focused.

use rune::{Drag, El, Tx, el, row};

use crate::App;
use crate::ui::Paint;

const HEIGHT: f32 = 22.0;
const TRACK: f32 = 4.0;
const THUMB: f32 = 14.0;

/// Where the slider sits, what it is called, and what moving it does: `set` gets the new
/// position, 0 to 1.
pub(crate) struct Slider {
    pub key: String,
    pub label: String,
    pub frac: f32,
    /// Stops between the ends; one arrow press moves one.
    pub steps: u32,
    pub set: fn(&mut App, f32),
}

impl Slider {
    pub(crate) fn build(self, p: &Paint) -> El {
        let Slider {
            key,
            label,
            frac,
            steps,
            set,
        } = self;
        let frac = frac.clamp(0.0, 1.0);
        let stop = 1.0 / steps.max(1) as f32;
        let track = row()
            .key(format!("{key}-track"))
            .absolute(0.0, (HEIGHT - TRACK) / 2.0)
            .w_full()
            .h(TRACK)
            .radius(TRACK / 2.0)
            .fill(p.c.layer_03)
            .clip_rounded(TRACK / 2.0)
            .child(el().w_frac(frac).h(TRACK).fill(p.accent));
        let thumb = row()
            .absolute(0.0, (HEIGHT - THUMB) / 2.0)
            .w_full()
            .child(el().w_frac(frac))
            .child(
                el().key(format!("{key}-thumb"))
                    .size(THUMB, THUMB)
                    .radius(THUMB / 2.0)
                    .fill(p.c.text_base)
                    .border(p.hair, p.c.border_strong)
                    .offset(-THUMB / 2.0, 0.0),
            );
        el().key(key)
            .role("slider")
            .label(label)
            .range_value(f64::from(frac), 0.0, 1.0)
            .range_step(f64::from(stop))
            .w_full()
            .h(HEIGHT)
            .on_drag(move |app: &mut App, g: &mut Drag, tx: &mut Tx| {
                tx.instant();
                if !g.cancelled() {
                    set(app, (g.x / g.w.max(1.0)).clamp(0.0, 1.0));
                }
            })
            .on_step(move |app: &mut App, delta: i32, _: &mut Tx| {
                set(app, (frac + delta as f32 * stop).clamp(0.0, 1.0));
            })
            .on_step_limit(move |app: &mut App, end: bool, _: &mut Tx| {
                set(app, if end { 1.0 } else { 0.0 });
            })
            .child(track)
            .child(thumb)
    }
}
