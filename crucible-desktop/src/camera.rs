//! The Graph pane's camera: how far its diagram is zoomed and moved.

use rune::{Drag, Live, Phase};

/// The least and most the diagram is drawn at.
const ZOOM: (f32, f32) = (0.2, 4.0);

/// One step of the zoom buttons.
pub const STEP: f32 = 1.25;

/// The diagram drawn `zoom` times its size about the middle of the pane's top edge, then moved
/// by `pan`. Points are the pane's, from that middle.
pub struct Camera {
    pan: [f32; 2],
    zoom: f32,
    live_pan: Live<[f32; 2]>,
    live_zoom: Live<f32>,
    /// `pan` when the drag in flight began.
    grabbed: [f32; 2],
}

impl Default for Camera {
    fn default() -> Camera {
        Camera {
            pan: [0.0, 0.0],
            zoom: 1.0,
            live_pan: Live::new([0.0, 0.0]),
            live_zoom: Live::new(1.0),
            grabbed: [0.0, 0.0],
        }
    }
}

impl Camera {
    pub fn pan(&self) -> &Live<[f32; 2]> {
        &self.live_pan
    }

    pub fn zoom(&self) -> &Live<f32> {
        &self.live_zoom
    }

    fn set(&mut self, pan: [f32; 2], zoom: f32) {
        (self.pan, self.zoom) = (pan, zoom);
        self.live_pan.set(pan);
        self.live_zoom.set(zoom);
    }

    pub fn pan_by(&mut self, [dx, dy]: [f32; 2]) {
        self.set([self.pan[0] + dx, self.pan[1] + dy], self.zoom);
    }

    /// Zooms by `factor`, keeping what shows at `at` where it is.
    pub fn zoom_by(&mut self, factor: f32, at: [f32; 2]) {
        let zoom = (self.zoom * factor).clamp(ZOOM.0, ZOOM.1);
        if !zoom.is_finite() {
            return;
        }
        let by = zoom / self.zoom;
        let keep = |at: f32, pan: f32| at - (at - pan) * by;
        self.set([keep(at[0], self.pan[0]), keep(at[1], self.pan[1])], zoom);
    }

    /// Moves with a drag, and back to where it began when the drag is cancelled.
    pub fn drag(&mut self, drag: &Drag) {
        if drag.phase == Phase::Down {
            self.grabbed = self.pan;
        }
        let pan = if drag.cancelled() {
            self.grabbed
        } else {
            [self.grabbed[0] + drag.dx, self.grabbed[1] + drag.dy]
        };
        self.set(pan, self.zoom);
    }

    pub fn reset(&mut self) {
        self.set([0.0, 0.0], 1.0);
    }
}

#[cfg(test)]
mod tests {
    use crate::camera::{Camera, ZOOM};

    /// Where the diagram point `q` shows.
    fn shown(camera: &Camera, q: [f32; 2]) -> [f32; 2] {
        [
            camera.pan[0] + q[0] * camera.zoom,
            camera.pan[1] + q[1] * camera.zoom,
        ]
    }

    fn near(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    #[test]
    fn a_zoom_keeps_the_point_under_it_still_and_publishes_the_view() {
        let mut camera = Camera::default();
        camera.pan_by([30.0, -12.0]);
        let at = [140.0, 260.0];
        let under = [
            (at[0] - camera.pan[0]) / camera.zoom,
            (at[1] - camera.pan[1]) / camera.zoom,
        ];
        for factor in [1.6, 0.5, 1.25, 0.8] {
            camera.zoom_by(factor, at);
            assert!(near(shown(&camera, under), at), "{factor}");
        }
        assert!((camera.zoom - 0.8).abs() < 1e-4);
        assert_eq!(camera.zoom().get(), camera.zoom);
        assert_eq!(camera.pan().get(), camera.pan);
    }

    #[test]
    fn zoom_stays_in_range_and_ignores_a_factor_that_is_not_a_number() {
        let mut camera = Camera::default();
        camera.zoom_by(1e9, [0.0, 0.0]);
        assert_eq!(camera.zoom, ZOOM.1);
        camera.zoom_by(0.0, [50.0, 50.0]);
        assert_eq!(camera.zoom, ZOOM.0);
        let before = (camera.pan, camera.zoom);
        camera.zoom_by(f32::NAN, [50.0, 50.0]);
        assert_eq!((camera.pan, camera.zoom), before);
    }

    #[test]
    fn reset_returns_to_the_first_view() {
        let mut camera = Camera::default();
        camera.pan_by([9.0, 9.0]);
        camera.zoom_by(2.0, [10.0, 10.0]);
        camera.reset();
        assert_eq!((camera.pan, camera.zoom), ([0.0, 0.0], 1.0));
        assert_eq!(camera.zoom().get(), 1.0);
    }
}
