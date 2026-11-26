//! How a call frame lights up, through the compositor: a running target pulses in the colour of
//! where it runs, a passed one glows green and fades out, a failed one holds a red glow, and the
//! pointer over a frame brightens it.

use crucible_contract::calls::{CallKind, CallStatus};
use rune::{Color, El, el};

use crate::afterglow::{Afterglows, FADE, Glow};
use crate::gfx;
use crate::live::calls::LiveCall;
use crate::ui::{Paint, fx};

/// A backend's accent as the compositor and the graph take it.
pub(crate) fn accent(kind: CallKind) -> Option<Color> {
    kind.presentation().accent.map(|rgb| Color {
        r: f32::from(rgb.0) / 255.0,
        g: f32::from(rgb.1) / 255.0,
        b: f32::from(rgb.2) / 255.0,
        a: 1.0,
    })
}

/// How much brighter a frame under the pointer draws.
const HOVER: f32 = 1.8;

/// A frame's corner radius.
const RADIUS: f32 = 6.0;

/// What a call frame shows of its target run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Look {
    Running(CallKind),
    /// Passed, and glowing while the window has just seen it pass.
    Passed(Option<Glow>),
    Failed,
    Quiet,
}

/// The look of a frame whose target run is `call`.
pub(crate) fn look(call: Option<&LiveCall>, glows: &Afterglows) -> Look {
    let Some(call) = call else {
        return Look::Quiet;
    };
    match call.view.state.status {
        CallStatus::Running => Look::Running(call.view.kind),
        CallStatus::Passed => Look::Passed(glows.glow(&call.view.state.call)),
        CallStatus::Failed => Look::Failed,
        CallStatus::Pending | CallStatus::Cancelled => Look::Quiet,
    }
}

/// The effect slot that draws `look` over a frame, if it draws one.
pub(crate) fn effect(look: Look, hovered: bool, p: &Paint) -> Option<El> {
    let lift = if hovered { HOVER } else { 1.0 };
    let slot = |kind: u32, color: Color, amount: f32, bleed: f32| {
        fx(kind, gfx::data(color, amount * lift), bleed).radius(RADIUS)
    };
    match look {
        Look::Running(kind) => {
            let color = accent(kind).unwrap_or(p.live);
            Some(slot(gfx::PULSE, color, p.tune.pulse, p.tune.pulse_bleed))
        }
        Look::Failed => Some(slot(gfx::HALO, p.fail, p.tune.glow, p.tune.glow_bleed)),
        Look::Passed(Some(glow)) => {
            let burn = rune::Spring::new(FADE as f32, 0.0);
            let lit = glow == Glow::Lit;
            Some(
                el().key("call-glow")
                    .absolute(0.0, 0.0)
                    .w_full()
                    .h_full()
                    .opacity(if lit { 1.0 } else { 0.0 })
                    .opacity_motion(burn)
                    .motion_intent(rune::MotionIntent::PULSE)
                    .a11y_hidden()
                    .child(slot(gfx::HALO, p.pass, p.tune.glow, p.tune.glow_bleed)),
            )
        }
        Look::Passed(None) if hovered => Some(slot(
            gfx::HALO,
            p.pass,
            p.tune.glow * 0.5,
            p.tune.glow_bleed,
        )),
        Look::Passed(None) | Look::Quiet => None,
    }
}

#[cfg(test)]
mod tests {
    use crucible_contract::calls::{CallId, CallKind, CallView};
    use serde_json::json;

    use crate::afterglow::{Afterglows, Glow};
    use crate::live::calls::LiveCall;
    use crate::ui::graph_fx::{Look, look};

    fn call(kind: &str, status: &str) -> LiveCall {
        let handle = match kind {
            "playbook" => json!({"kind": "playbook", "run": "r"}),
            _ => json!({"kind": "github_workflow", "repo": "acme/app", "run_id": 1}),
        };
        let view: CallView = serde_json::from_value(json!({
            "task": "ship",
            "attempt": 1,
            "alias": "release",
            "kind": kind,
            "state": {"call": "c-1", "status": status, "handle": handle},
        }))
        .unwrap();
        LiveCall { view, launch: None }
    }

    #[test]
    fn a_frame_looks_like_where_and_how_its_target_runs() {
        let glows = Afterglows::default();
        assert_eq!(look(None, &glows), Look::Quiet);
        assert_eq!(
            look(Some(&call("playbook", "running")), &glows),
            Look::Running(CallKind::Playbook)
        );
        assert_eq!(
            look(Some(&call("github_workflow", "running")), &glows),
            Look::Running(CallKind::GithubWorkflow)
        );
        assert_eq!(
            look(Some(&call("playbook", "pending")), &glows),
            Look::Quiet
        );
        assert_eq!(
            look(Some(&call("playbook", "failed")), &glows),
            Look::Failed
        );
        assert_eq!(
            look(Some(&call("playbook", "cancelled")), &glows),
            Look::Quiet
        );
        assert_eq!(
            look(Some(&call("playbook", "passed")), &glows),
            Look::Passed(None)
        );
    }

    #[test]
    fn a_frame_seen_passing_glows_until_its_glow_goes_out() {
        let mut glows = Afterglows::default();
        let running = call("playbook", "running");
        let passed = call("playbook", "passed");
        glows.saw(
            std::slice::from_ref(&running.view),
            std::slice::from_ref(&passed.view),
        );
        assert_eq!(look(Some(&passed), &glows), Look::Passed(Some(Glow::Lit)));
        glows.tick(1.0);
        assert_eq!(
            look(Some(&passed), &glows),
            Look::Passed(Some(Glow::Fading))
        );
        glows.tick(10.0);
        assert_eq!(look(Some(&passed), &glows), Look::Passed(None));
        assert_eq!(passed.view.state.call, CallId("c-1".into()));
    }
}
