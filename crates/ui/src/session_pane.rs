use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    Render, Subscription, Window, div, prelude::*, px,
};

use crate::composer::{Composer, ComposerEvent};
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::Transcript;

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
                transcript.update(cx, |transcript, cx| match event {
                    ComposerEvent::HumanSubmitted { .. } => {}
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
        self.transcript.update(cx, |transcript, cx| {
            transcript.set_bottom_clearance(self.composer_height, cx);
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
