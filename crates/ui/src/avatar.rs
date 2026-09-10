//! Session companions. Identity (shape/colour) is stable per chat; lifecycle is
//! expressed by gaze, blink, eye shape and body squash, not a status dot. Every
//! avatar rides the shell's shared pulse clock.
use gpui::{IntoElement, PathBuilder, Pixels, Point, Window, canvas, point, prelude::*, px, rgb};
use std::f32::consts::TAU;
use zeron_proto::ChatIndicator;

const COLOURS: [u32; 8] = [
    0x00a46d, 0x85858c, 0x147be8, 0xf3a500, 0xe53951, 0x9069dc, 0x12a6aa, 0xeb8055,
];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pose {
    squash: f32,
    tilt: f32,
    gaze_x: f32,
    gaze_y: f32,
    eye_open: f32,
}

fn pose(id: usize, state: ChatIndicator, t: f32, motion: bool) -> Pose {
    let phase = (id % 17) as f32 * 0.71;
    let t = if motion && t.is_finite() { t } else { 0.0 };
    let blink_t = (t + phase).rem_euclid(4.7);
    let blink = if motion && blink_t < 0.18 {
        ((blink_t / 0.18 * 2.0 - 1.0).abs()).max(0.08)
    } else {
        1.0
    };
    let busy = state == ChatIndicator::Working && motion;
    Pose {
        squash: if busy {
            1.0 + 0.055 * (t * TAU / 1.9 + phase).sin()
        } else {
            1.0
        },
        tilt: if busy {
            0.10 * (t * TAU / 3.8 + phase).sin()
        } else {
            -0.08
        },
        gaze_x: if motion {
            (t * 0.78 + phase).sin() * 0.027
        } else {
            0.0
        },
        gaze_y: if matches!(state, ChatIndicator::AwaitingInput | ChatIndicator::Errored) {
            -0.035
        } else {
            0.0
        },
        eye_open: if state == ChatIndicator::Idle {
            0.12
        } else {
            blink
        },
    }
}

fn identity(id: usize) -> (usize, u32) {
    let i = id.saturating_sub(1);
    (i % 6, COLOURS[i % COLOURS.len()])
}

/// `seed` is any stable per-chat number (a hash of the chat id works) — it
/// picks the shape/colour and the animation phase. `state` drives the pose,
/// `t` is elapsed seconds from [`crate::motion::pulse_seconds`], `motion` the
/// reduced-motion flag, `extent` the square side in px.
pub fn render(
    seed: usize,
    state: ChatIndicator,
    t: f32,
    motion: bool,
    extent: f32,
) -> impl IntoElement {
    let (shape, colour) = identity(seed);
    let pose = pose(seed, state, t, motion);
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let map = |x: f32, y: f32| -> Point<Pixels> {
                let x = (x - 0.5) * pose.squash;
                let y = (y - 0.5) / pose.squash;
                let (sin, cos) = pose.tilt.sin_cos();
                bounds.origin
                    + point(
                        px((x * cos - y * sin + 0.5) * w),
                        px((x * sin + y * cos + 0.5) * h),
                    )
            };
            let points: &[(f32, f32)] = match shape {
                0 => &[
                    (0.50, 0.07),
                    (0.87, 0.28),
                    (0.87, 0.72),
                    (0.50, 0.93),
                    (0.13, 0.72),
                    (0.13, 0.28),
                ],
                1 => &[
                    (0.12, 0.70),
                    (0.07, 0.52),
                    (0.20, 0.37),
                    (0.19, 0.22),
                    (0.39, 0.12),
                    (0.55, 0.20),
                    (0.73, 0.18),
                    (0.84, 0.34),
                    (0.82, 0.46),
                    (0.94, 0.63),
                    (0.86, 0.78),
                    (0.65, 0.83),
                    (0.49, 0.77),
                    (0.32, 0.86),
                ],
                2 => &[
                    (0.50, 0.035),
                    (0.66, 0.24),
                    (0.82, 0.46),
                    (0.90, 0.64),
                    (0.76, 0.89),
                    (0.48, 0.95),
                    (0.21, 0.85),
                    (0.10, 0.62),
                    (0.20, 0.41),
                    (0.35, 0.21),
                ],
                3 => &[
                    (0.37, 0.09),
                    (0.65, 0.12),
                    (0.88, 0.29),
                    (0.91, 0.54),
                    (0.81, 0.78),
                    (0.62, 0.91),
                    (0.33, 0.92),
                    (0.11, 0.77),
                    (0.07, 0.50),
                    (0.17, 0.23),
                ],
                4 => &[
                    (0.23, 0.16),
                    (0.59, 0.07),
                    (0.82, 0.17),
                    (0.94, 0.46),
                    (0.81, 0.76),
                    (0.60, 0.93),
                    (0.30, 0.85),
                    (0.08, 0.57),
                    (0.11, 0.34),
                ],
                _ => &[
                    (0.48, 0.08),
                    (0.73, 0.16),
                    (0.91, 0.36),
                    (0.89, 0.65),
                    (0.71, 0.87),
                    (0.41, 0.93),
                    (0.16, 0.79),
                    (0.08, 0.50),
                    (0.19, 0.23),
                ],
            };
            let mut body = PathBuilder::fill();
            let len = points.len();
            let mid = |a: (f32, f32), b: (f32, f32)| map((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
            body.move_to(mid(points[len - 1], points[0]));
            for i in 0..len {
                body.curve_to(
                    mid(points[i], points[(i + 1) % len]),
                    map(points[i].0, points[i].1),
                );
            }
            body.close();
            if let Ok(path) = body.build() {
                window.paint_path(path, rgb(colour));
            }
            let eye_colour = rgb(0x10201d);
            for x in [0.43, 0.65] {
                let x = x + pose.gaze_x;
                let y = 0.43 + pose.gaze_y;
                if state == ChatIndicator::Completed {
                    let mut eye = PathBuilder::stroke(px(w * 0.045));
                    eye.move_to(map(x - 0.035, y + 0.018));
                    eye.curve_to(map(x + 0.035, y + 0.018), map(x, y - 0.055));
                    if let Ok(path) = eye.build() {
                        window.paint_path(path, eye_colour);
                    }
                } else {
                    ellipse(window, &map, x, y, 0.032, 0.087 * pose.eye_open, eye_colour);
                }
            }
        },
    )
    .w(px(extent))
    .h(px(extent))
}

fn ellipse(
    window: &mut Window,
    map: &impl Fn(f32, f32) -> Point<Pixels>,
    x: f32,
    y: f32,
    rx: f32,
    ry: f32,
    colour: gpui::Rgba,
) {
    let mut path = PathBuilder::fill();
    let k = 0.552_284_8;
    path.move_to(map(x + rx, y));
    path.cubic_bezier_to(
        map(x, y + ry),
        map(x + rx, y + k * ry),
        map(x + k * rx, y + ry),
    );
    path.cubic_bezier_to(
        map(x - rx, y),
        map(x - k * rx, y + ry),
        map(x - rx, y + k * ry),
    );
    path.cubic_bezier_to(
        map(x, y - ry),
        map(x - rx, y - k * ry),
        map(x - k * rx, y - ry),
    );
    path.cubic_bezier_to(
        map(x + rx, y),
        map(x + k * rx, y - ry),
        map(x + rx, y - k * ry),
    );
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, colour);
    }
}

/// Stable per-chat seed from the chat id string — picks shape and colour.
pub fn seed(chat_id: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    chat_id.hash(&mut h);
    (h.finish() % usize::MAX as u64) as usize + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_stable_and_varied() {
        for id in 1..=24 {
            assert_eq!(identity(id), identity(id));
            assert_ne!(identity(id), identity(id + 1));
        }
    }
    #[test]
    fn calm_is_time_independent() {
        for state in [
            ChatIndicator::Working,
            ChatIndicator::AwaitingInput,
            ChatIndicator::Completed,
            ChatIndicator::Idle,
        ] {
            assert_eq!(pose(3, state, 0.0, false), pose(3, state, 100.0, false));
        }
    }
    #[test]
    fn working_deforms_body_and_eyes_independently() {
        let a = pose(1, ChatIndicator::Working, 0.0, true);
        let b = pose(1, ChatIndicator::Working, 1.0, true);
        assert_ne!(a.squash, b.squash);
        assert_ne!(a.gaze_x, b.gaze_x);
        assert!(a.squash > 0.9 && a.squash < 1.1);
    }
}
