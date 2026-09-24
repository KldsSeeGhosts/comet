use std::time::Instant;

use gpui::{
    App, AppContext, Context, Entity, Hsla, IntoElement, ParentElement,
    Render, RenderOnce, SharedString, Styled, Window, div, img, prelude::FluentBuilder as _, px,
};

use crate::motion::{self, EASE, EASE_IN_OUT, MotionSpec};

const BLINK: MotionSpec = MotionSpec::new(5200, EASE);
const WORKING_BOB: MotionSpec = MotionSpec::new(2100, EASE_IN_OUT);
const ERROR_SHAKE: MotionSpec = MotionSpec::new(3600, EASE_IN_OUT);
const HOVER_WIGGLE: MotionSpec = MotionSpec::new(500, EASE_IN_OUT);
const STATUS_PULSE: MotionSpec = MotionSpec::new(1600, EASE_IN_OUT);

/// Keep session avatars identical wherever their identity is shown.
pub(crate) fn avatar_for_session(session: &str) -> (&'static str, &'static str) {
    use crate::icons;
    const AVATARS: [(&str, &str); 9] = [
        (icons::BOT_ORBIT, icons::BOT_ORBIT_BLINK),
        (icons::BOT_VISOR, icons::BOT_VISOR_BLINK),
        (icons::BOT_DOME, icons::BOT_DOME_BLINK),
        (icons::BOT_BOX, icons::BOT_BOX_BLINK),
        (icons::BOT_EARS, icons::BOT_EARS_BLINK),
        (icons::BOT_HALO, icons::BOT_HALO_BLINK),
        (icons::BOT_SPROUT, icons::BOT_SPROUT_BLINK),
        (icons::BOT_BOLT, icons::BOT_BOLT_BLINK),
        (icons::BOT_BASIC, icons::BOT_BASIC_BLINK),
    ];
    let variant = session.bytes().fold(0_u8, |hash, byte| hash.wrapping_mul(31).wrapping_add(byte));
    AVATARS[usize::from(variant) % AVATARS.len()]
}

pub(crate) fn status_color(
    status: zeron_proto::ChatIndicator,
    queued: bool,
    undelivered: bool,
    theme: &crate::theme::Theme,
) -> Hsla {
    if undelivered { return theme.danger; }
    if queued { return theme.warning; }
    match status {
        zeron_proto::ChatIndicator::Working => theme.busy,
        zeron_proto::ChatIndicator::AwaitingInput => theme.warning,
        zeron_proto::ChatIndicator::Errored => theme.danger,
        zeron_proto::ChatIndicator::Completed => theme.success,
        zeron_proto::ChatIndicator::Idle => theme.text_muted.opacity(0.45),
    }
}

pub(crate) fn buddy(
    key: impl Into<SharedString>,
    image: &'static str,
    blink_image: &'static str,
    status: zeron_proto::ChatIndicator,
    hovered: bool,
    status_color: Hsla,
    ring_color: Hsla,
) -> SidebarBuddy {
    let phase = image.bytes().fold(0_u16, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u16::from(byte))
    }) as f32
        / u16::MAX as f32;
    SidebarBuddy {
        key: key.into(),
        image,
        blink_image,
        status,
        hovered,
        status_color,
        ring_color,
        phase,
        size: 36.0,
    }
}

#[derive(IntoElement)]
pub(crate) struct SidebarBuddy {
    key: SharedString,
    image: &'static str,
    blink_image: &'static str,
    status: zeron_proto::ChatIndicator,
    hovered: bool,
    status_color: Hsla,
    ring_color: Hsla,
    phase: f32,
    size: f32,
}

impl SidebarBuddy {
    pub(crate) fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }
}

impl RenderOnce for SidebarBuddy {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let view = window.with_global_id(self.key.into(), |id, window| {
            window.with_element_state(id, |previous: Option<Entity<SidebarBuddyView>>, _| {
                let view = previous.unwrap_or_else(|| {
                    cx.new(|_| SidebarBuddyView {
                        image: self.image,
                        blink_image: self.blink_image,
                        status: self.status,
                        hovered: self.hovered,
                        status_color: self.status_color,
                        ring_color: self.ring_color,
                        phase: self.phase,
                        size: self.size,
                        hover_started: self.hovered.then(Instant::now),
                    })
                });
                view.update(cx, |view, cx| {
                    let hover_started = self.hovered && !view.hovered;
                    let changed = view.image != self.image
                        || view.blink_image != self.blink_image
                        || view.status != self.status
                        || view.hovered != self.hovered
                        || view.status_color != self.status_color
                        || view.ring_color != self.ring_color
                        || view.phase != self.phase
                        || view.size != self.size;
                    if changed {
                        view.image = self.image;
                        view.blink_image = self.blink_image;
                        view.status = self.status;
                        view.hovered = self.hovered;
                        view.status_color = self.status_color;
                        view.ring_color = self.ring_color;
                        view.phase = self.phase;
                        view.size = self.size;
                        if hover_started {
                            view.hover_started = Some(Instant::now());
                        }
                        cx.notify();
                    }
                });
                (view.clone(), view)
            })
        });
        view.cached(gpui::StyleRefinement::default().w(px(self.size)).h(px(self.size)))
    }
}

struct SidebarBuddyView {
    image: &'static str,
    blink_image: &'static str,
    status: zeron_proto::ChatIndicator,
    hovered: bool,
    status_color: Hsla,
    ring_color: Hsla,
    phase: f32,
    size: f32,
    hover_started: Option<Instant>,
}

impl Render for SidebarBuddyView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reduced_motion = cx.reduce_motion();
        let blink_phase = (motion::pulse_delta(&BLINK, cx.entity_id(), cx) + self.phase).fract();
        let blink = !reduced_motion
            && ((0.885..0.925).contains(&blink_phase) || (0.95..0.975).contains(&blink_phase));

        let (mut top, mut left) = match self.status {
            zeron_proto::ChatIndicator::Working if !reduced_motion => {
                let phase =
                    (motion::pulse_delta(&WORKING_BOB, cx.entity_id(), cx) + self.phase).fract();
                let wave = (phase * std::f32::consts::TAU).cos();
                (-0.35 + 1.05 * wave, 0.0)
            }
            zeron_proto::ChatIndicator::Errored if !reduced_motion => {
                let phase = motion::pulse_delta(&ERROR_SHAKE, cx.entity_id(), cx);
                let shake = if phase > 0.86 {
                    ((phase - 0.86) / 0.14 * std::f32::consts::TAU * 2.0).sin()
                        * (1.0 - (phase - 0.86) / 0.14)
                        * 1.7
                } else {
                    0.0
                };
                (0.0, shake)
            }
            _ => (0.0, 0.0),
        };

        if self.hovered
            && !reduced_motion
            && let Some(started) = self.hover_started
        {
            let phase =
                (started.elapsed().as_secs_f32() / HOVER_WIGGLE.total().as_secs_f32()).min(1.0);
            if phase < 1.0 {
                motion::pulse_lease(cx.entity_id(), cx);
                let damp = 1.0 - phase;
                left += (phase * std::f32::consts::TAU * 2.0).sin() * 1.4 * damp;
                top -= (phase * std::f32::consts::TAU).sin().abs() * 0.9;
            }
        }

        let dot_opacity = if self.status == zeron_proto::ChatIndicator::Working && !reduced_motion {
            let phase = motion::pulse_delta(&STATUS_PULSE, cx.entity_id(), cx);
            0.35 + 0.65 * ((phase * std::f32::consts::TAU).cos() + 1.0) * 0.5
        } else {
            1.0
        };

        let scale = self.size / 36.0;
        // Idle is the resting state, not a status: no badge, so a list of
        // settled sessions reads clean instead of dotted with grey specks.
        let show_badge = self.status != zeron_proto::ChatIndicator::Idle;
        div()
            .relative()
            .size(px(self.size))
            .child(
                img(if blink { self.blink_image } else { self.image })
                    .size(px(self.size))
                    .relative()
                    .top(px(top * scale))
                    .left(px(left * scale)),
            )
            .when(show_badge, |el| el.child(
                div()
                    .absolute()
                    .right(px(-scale))
                    .bottom(px(-scale))
                    .size(px(13.0 * scale))
                    .rounded_full()
                    .bg(self.ring_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size(px(9.0 * scale))
                            .rounded_full()
                            .bg(self.status_color)
                            .opacity(dot_opacity),
                    ),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_failures_and_queue_override_working_color() {
        let theme = crate::theme::Theme::default();
        let working = zeron_proto::ChatIndicator::Working;
        assert_eq!(status_color(working, false, false, &theme), theme.busy);
        assert_eq!(status_color(working, true, false, &theme), theme.warning);
        assert_eq!(status_color(working, true, true, &theme), theme.danger);
    }

    #[test]
    fn avatar_for_session_regression_chat_0_and_browser_fixture() {
        assert_eq!(
            avatar_for_session("chat-0"),
            (crate::icons::BOT_BASIC, crate::icons::BOT_BASIC_BLINK)
        );
        assert_eq!(
            avatar_for_session("browser-fixture"),
            (crate::icons::BOT_VISOR, crate::icons::BOT_VISOR_BLINK)
        );
    }
}
