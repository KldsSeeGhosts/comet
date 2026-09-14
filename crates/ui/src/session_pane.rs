use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    Render, Subscription, Window, div, prelude::*, px,
};

use crate::composer::{Composer, ComposerEvent};
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::Transcript;
use crate::transcript::plan_hud;

/// The existing chat renderer with pane-local selection, draft and scroll state.
pub struct ChatView {
    pub state: Entity<AppState>,
    pub transcript: Entity<Transcript>,
    pub composer: Entity<Composer>,
    selected: Option<String>,
    active: bool,
    width: f32,
    composer_height: f32,
    _state: Subscription,
    _composer: Subscription,
}

pub enum ChatViewEvent {
    Focused,
    Selected(Option<String>),
    HumanSubmitted(String, crate::input_origin::HumanInput),
    /// The pane composer's mic button; the workspace owns the one session.
    VoiceToggled,
}

impl EventEmitter<ChatViewEvent> for ChatView {}

impl ChatView {
    pub fn new(source: &Entity<AppState>, chat: Option<String>, cx: &mut Context<Self>) -> Self {
        let state = AppState::fork_pane(source, cx);
        state.update(cx, |state, cx| state.select_chat(chat.clone(), cx));
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        let observation = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if this.selected != selected {
                this.selected = selected.clone();
                cx.emit(ChatViewEvent::Selected(selected));
            }
            cx.notify();
        });
        let events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_: &mut Self, _, event, cx| {
                if let ComposerEvent::HumanSubmitted { chat_id, proof } = event {
                    cx.emit(ChatViewEvent::HumanSubmitted(chat_id.clone(), *proof));
                    return;
                }
                if matches!(event, ComposerEvent::VoiceToggled) {
                    cx.emit(ChatViewEvent::VoiceToggled);
                    return;
                }
                transcript.update(cx, |transcript, cx| match event {
                    ComposerEvent::HumanSubmitted { .. } | ComposerEvent::VoiceToggled => {}
                    ComposerEvent::Sent { chat_id, message_id } => {
                        transcript.on_own_send(chat_id.clone(), message_id.clone(), cx);
                    }
                    ComposerEvent::Queued { chat_id, message_id } => {
                        transcript.on_own_queued_send(chat_id.clone(), message_id.clone(), cx);
                    }
                });
            }
        });
        Self {
            state,
            transcript,
            composer,
            selected: chat,
            active: true,
            width: 0.0,
            composer_height: 120.0,
            _state: observation,
            _composer: events,
        }
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            self.composer.read(cx).input.clone().update(cx, |input, cx| input.set_pane_active(active, cx));
            cx.notify();
        }
    }

    /// The workspace's shared voice status, mirrored into this pane's
    /// composer so every visible mic button shows the same state.
    pub fn set_voice_status(&mut self, status: crate::voice::VoiceStatus, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.set_voice_status(status, cx));
    }

    pub fn select(&mut self, chat: Option<String>, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat != chat {
            self.state.update(cx, |state, cx| state.select_chat(chat, cx));
        }
    }
}

impl Focusable for ChatView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }
}

impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        self.composer.update(cx, |composer, cx| {
            composer.set_available_width(self.width, cx);
        });
        // The ActivePlanHud's state: the selected chat's most recent Todo
        // list, derived pure from the transcript snapshot. Hidden while no
        // plan exists; mounting/unmounting the constant-height strip is an
        // instant one-frame layout change.
        let plan = plan_hud::plan_progress(self.state.read(cx).transcript.as_slice());
        let hud_height = plan.as_ref().map_or(0.0, |_| plan_hud::HUD_HEIGHT);
        self.transcript.update(cx, |transcript, cx| {
            transcript.set_bottom_clearance(self.composer_height + hud_height, cx);
            transcript.set_rail_enabled(crate::rail::rail_visible(self.width), cx);
        });
        let pane = cx.weak_entity();
        let footer = cx.weak_entity();
        div()
            .id("session-chat")
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .when(!self.active, |element| element.opacity(0.88))
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
                cx.emit(ChatViewEvent::Focused);
            }))
            .child(
                gpui::canvas(move |bounds, _, cx| {
                    let width = f32::from(bounds.size.width);
                    let _ = pane.update(cx, |pane, cx| {
                        if (pane.width - width).abs() > 0.5 {
                            pane.width = width;
                            cx.notify();
                        }
                    });
                }, |_, _, _, _| {}).absolute().inset_0(),
            )
            .child(div().absolute().inset_0().child(self.transcript.clone()))
            .child(div().flex_1().min_h_0())
            // ActivePlanHud: plan/todo progress strip docking the composer
            // (absent entirely while the chat has no plan).
            .when_some(plan, |element, plan| {
                element.child(plan_hud::render(&plan, &theme))
            })
            .child(
                div().relative().flex_none().pb(px(4.0))
                    .bg(theme.bg)
                    .child(gpui::canvas(move |bounds, _, cx| {
                        let height = f32::from(bounds.size.height);
                        let _ = footer.update(cx, |pane, cx| {
                            if (pane.composer_height - height).abs() > 0.5 {
                                pane.composer_height = height;
                                cx.notify();
                            }
                        });
                    }, |_, _, _, _| {}).absolute().inset_0())
                    .child(self.composer.clone()),
            )
    }
}

#[cfg(test)]
mod chat_view_tests {
    use super::*;
    use gpui::{TestAppContext, point, px, size};
    use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
    use zeron_proto::{ToolCall, TodoItem};

    use crate::transcript::plan_hud::PlanProgress;

    /// The workspace tests' boot recipe, minus the workspace.
    fn setup(cx: &mut TestAppContext, directory: &std::path::Path) {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), directory, cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            crate::composer::init(cx, Default::default());
        });
    }

    /// One window with a chat view over a fresh pane fork, at a known size,
    /// pinned to chat "c". The visual test context is handed to `run` — the
    /// per-window context only exists inside this call.
    fn with_chat_view<R>(
        cx: &mut TestAppContext,
        run: impl FnOnce(Entity<ChatView>, Entity<AppState>, &mut gpui::VisualTestContext) -> R,
    ) -> R {
        let directory = tempfile::tempdir().unwrap();
        setup(cx, directory.path());
        let (view, cx) = cx.add_window_view(|_, cx| {
            let source = cx.new(|_| AppState::new());
            ChatView::new(&source, Some("c".into()), cx)
        });
        cx.simulate_resize(size(px(800.0), px(600.0)));
        cx.run_until_parked();
        let state = view.read_with(cx, |view, _| view.state.clone());
        state.update(cx, |state, cx| {
            state.selected_chat = Some("c".into());
            cx.notify();
        });
        run(view, state, cx)
    }

    fn todo_entry(id: &str, items: &[(&str, bool)]) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: format!("{id}-part"),
                call: ToolCall::Todo {
                    items: items
                        .iter()
                        .map(|(text, done)| TodoItem {
                            text: (*text).into(),
                            done: *done,
                        })
                        .collect(),
                },
                is_error: false,
                resolved: true,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn prose_entry(id: &str, paragraphs: usize) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: format!("{id}-part"),
                text: std::iter::repeat_n("A filler paragraph long enough to wrap.", paragraphs)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn apply(
        state: &Entity<AppState>,
        cx: &mut gpui::VisualTestContext,
        entries: Vec<SessionMessageEntry>,
    ) {
        // Entity updates do not notify on their own in gpui; the engine path
        // notifies through `receive_transcript_frame`, so the test does too.
        state.update(cx, |state, cx| {
            state.apply_transcript(entries);
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_plan_hud_mounts_only_while_a_plan_exists(cx: &mut TestAppContext) {
        with_chat_view(cx, |_, state, cx| {
            // No plan → no strip.
            apply(&state, cx, vec![prose_entry("m0", 3)]);
            assert!(
                cx.debug_bounds("plan-hud").is_none(),
                "no todo list, no HUD"
            );

            // A todo list mounts the constant-height strip.
            apply(
                &state,
                cx,
                vec![todo_entry("m1", &[("first", true), ("second", false)])],
            );
            let bounds = cx.debug_bounds("plan-hud").expect("HUD mounts with a plan");
            assert_eq!(bounds.size.height, px(plan_hud::HUD_HEIGHT));

            // Checking the last step keeps the strip in its complete state.
            apply(
                &state,
                cx,
                vec![todo_entry("m1", &[("first", true), ("second", true)])],
            );
            let bounds = cx
                .debug_bounds("plan-hud")
                .expect("complete plans stay mounted");
            assert_eq!(bounds.size.height, px(plan_hud::HUD_HEIGHT));
            let progress = state.read_with(cx, |state, _| {
                plan_hud::plan_progress(state.transcript.as_slice())
            });
            assert_eq!(
                progress,
                Some(PlanProgress {
                    total: 2,
                    done: 2,
                    current: None,
                })
            );
        });
    }

    #[gpui::test]
    fn hovering_the_transcript_reveals_the_overlay_scrollbar(cx: &mut TestAppContext) {
        with_chat_view(cx, |_, state, cx| {
            // Long enough content to overflow the viewport.
            apply(&state, cx, vec![prose_entry("m0", 120)]);
            assert!(
                cx.debug_bounds("transcript-scrollbar").is_none(),
                "the rail is hidden while the transcript is not hovered"
            );

            // Hovering the transcript arms the rail.
            cx.simulate_event(gpui::MouseMoveEvent {
                position: point(px(400.0), px(300.0)),
                ..Default::default()
            });
            cx.run_until_parked();
            let rail = cx
                .debug_bounds("transcript-scrollbar")
                .expect("the rail appears on hover");
            assert!(
                f32::from(rail.size.width) <= 12.0,
                "the hit strip stays thin: {:?}",
                rail.size.width
            );
        });
    }

    #[gpui::test]
    fn clicking_the_scrollbar_track_jumps_off_the_bottom(cx: &mut TestAppContext) {
        with_chat_view(cx, |view, state, cx| {
            let transcript = view.read_with(cx, |view, _| view.transcript.clone());

            // Long enough content to overflow; the view opens pinned at the end.
            apply(&state, cx, vec![prose_entry("m0", 120)]);
            let before = transcript.read_with(cx, |t, _| t.distance_from_bottom());
            assert!(before <= 2.0, "fresh view rests at the bottom: {before}");

            // Hover to arm the rail, then click a quarter of the way down it.
            cx.simulate_event(gpui::MouseMoveEvent {
                position: point(px(400.0), px(300.0)),
                ..Default::default()
            });
            cx.run_until_parked();
            let rail = cx
                .debug_bounds("transcript-scrollbar")
                .expect("the rail appears on hover");
            let track = point(
                rail.center().x,
                rail.top() + px(f32::from(rail.size.height) * 0.25),
            );
            cx.simulate_event(gpui::MouseDownEvent {
                position: track,
                button: gpui::MouseButton::Left,
                ..Default::default()
            });
            cx.simulate_event(gpui::MouseUpEvent {
                position: track,
                button: gpui::MouseButton::Left,
                ..Default::default()
            });
            cx.run_until_parked();

            // The click-track jump broke the bottom pin and moved the viewport.
            let after = transcript.read_with(cx, |t, _| t.distance_from_bottom());
            assert!(
                after > 200.0,
                "a track click jumps the viewport: before {before}, after {after}"
            );
            assert_eq!(
                transcript.read_with(cx, |t, _| t.jump_button_shown()),
                after > crate::transcript::SCROLL_BUTTON_THRESHOLD_PX,
                "the jump pill follows the new distance"
            );
        });
    }
}
