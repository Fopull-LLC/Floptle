//! Shared timeline primitives — the px↔time transform, fps snapping, the ruler,
//! and the timeline color language. Extracted from the animation dope sheet so
//! every timeline-based editor (Animating tab, the Particles tab) draws the same
//! ruler, scrubs the same way, and snaps to the same grid.

use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke};

/// Selection / hover highlight.
pub(crate) const ACCENT: Color32 = Color32::from_rgb(120, 190, 255);
/// Keyframe diamonds.
pub(crate) const KEY_COLOR: Color32 = Color32::from_rgb(235, 200, 90);
/// Event flags / burst markers.
pub(crate) const EVENT_COLOR: Color32 = Color32::from_rgb(240, 140, 150);
/// The playhead line.
pub(crate) const PLAYHEAD: Color32 = Color32::from_rgb(240, 90, 90);

/// Quantize `t` onto an fps grid; `fps <= 0` = free (no snapping).
pub(crate) fn snap_time(t: f32, fps: f32) -> f32 {
    if fps > 0.0 { (t * fps).round() / fps } else { t }
}

/// The px↔time transform of one timeline strip: `left` is the x of `t = 0`,
/// `px_per_s` the zoom, `duration` the clamp for pointer→time conversions.
#[derive(Clone, Copy)]
pub(crate) struct TimelineView {
    pub left: f32,
    pub px_per_s: f32,
    pub duration: f32,
}

impl TimelineView {
    pub fn time_to_x(&self, t: f32) -> f32 {
        self.left + t * self.px_per_s
    }

    pub fn x_to_time(&self, x: f32) -> f32 {
        ((x - self.left) / self.px_per_s).clamp(0.0, self.duration)
    }
}

/// A "nice" tick step (1-2-5 × 10^n) at least `raw` seconds, so labels land on round
/// numbers whatever the zoom.
pub(crate) fn nice_step(raw: f32) -> f32 {
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
    }
    let pow = 10f32.powf(raw.log10().floor());
    let m = raw / pow;
    let nice = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    (nice * pow).max(1e-3)
}

/// Ruler ticks + labels + end marker + playhead over `rect`. The tick step adapts to
/// the zoom (targeting ~70 px between labels) so the ruler reads at any scale — from
/// a 600 s effect zoomed out to sub-frame keying zoomed in.
pub(crate) fn draw_ruler(
    painter: &egui::Painter,
    rect: Rect,
    dur: f32,
    playhead: f32,
    px_per_s: f32,
) {
    let weak = Color32::from_gray(140);
    let step = nice_step(70.0 / px_per_s.max(1e-3));
    // How many decimals the labels need at this step (0 for ≥1 s, more when finer).
    let decimals = if step >= 1.0 {
        0
    } else if step >= 0.1 {
        1
    } else if step >= 0.01 {
        2
    } else {
        3
    };
    // Draw a whole number of steps across [0, dur].
    let n = (dur / step).floor() as i64;
    for k in 0..=n {
        let t = k as f32 * step;
        let x = rect.left() + t * px_per_s;
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.top() + 8.0)],
            Stroke::new(1.0, weak),
        );
        painter.text(
            Pos2::new(x + 3.0, rect.top() + 4.0),
            Align2::LEFT_CENTER,
            format!("{t:.decimals$}s"),
            FontId::proportional(9.0),
            weak,
        );
        // Four minor ticks between labels, when there's room for them.
        if step * px_per_s > 55.0 {
            for i in 1..5 {
                let tt = t + i as f32 * step / 5.0;
                if tt > dur {
                    break;
                }
                let xx = rect.left() + tt * px_per_s;
                painter.line_segment(
                    [Pos2::new(xx, rect.top()), Pos2::new(xx, rect.top() + 4.0)],
                    Stroke::new(0.5, weak.gamma_multiply(0.6)),
                );
            }
        }
    }
    // End-of-clip marker + playhead.
    let xe = rect.left() + dur * px_per_s;
    painter.line_segment(
        [Pos2::new(xe, rect.top()), Pos2::new(xe, rect.bottom())],
        Stroke::new(1.0, Color32::from_rgb(150, 150, 170)),
    );
    let xp = rect.left() + playhead * px_per_s;
    painter.line_segment(
        [Pos2::new(xp, rect.top()), Pos2::new(xp, rect.bottom())],
        Stroke::new(1.5, PLAYHEAD),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_round_trips_px_and_time() {
        let v = TimelineView { left: 130.0, px_per_s: 120.0, duration: 2.0 };
        assert_eq!(v.time_to_x(0.0), 130.0);
        assert_eq!(v.time_to_x(1.5), 130.0 + 180.0);
        assert!((v.x_to_time(v.time_to_x(0.7)) - 0.7).abs() < 1e-5);
        // Pointer positions outside the strip clamp into [0, duration].
        assert_eq!(v.x_to_time(0.0), 0.0);
        assert_eq!(v.x_to_time(1e6), 2.0);
    }

    #[test]
    fn snap_quantizes_only_when_fps_positive() {
        assert_eq!(snap_time(0.126, 24.0), 3.0 / 24.0);
        assert_eq!(snap_time(0.126, 0.0), 0.126);
    }

    #[test]
    fn nice_step_rounds_to_1_2_5_decades() {
        let close = |a: f32, b: f32| (a - b).abs() < b.abs() * 1e-4 + 1e-6;
        assert!(close(nice_step(0.9), 1.0));
        assert!(close(nice_step(1.1), 2.0));
        assert!(close(nice_step(2.1), 5.0));
        assert!(close(nice_step(6.0), 10.0));
        assert!(close(nice_step(0.03), 0.05));
        assert!(close(nice_step(30.0), 50.0));
        // Degenerate inputs never divide-by-zero or NaN.
        assert_eq!(nice_step(0.0), 1.0);
        assert_eq!(nice_step(-5.0), 1.0);
        assert_eq!(nice_step(f32::NAN), 1.0);
    }
}
