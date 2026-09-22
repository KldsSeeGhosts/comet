//! A session-bound Chat / CLI control. The engine owns process handoff;
//! this view never manufactures a resume command or writes a prompt.
use crate::{state::AppState, terminal::panel::TerminalPanel, theme::Theme};
use gpui::{prelude::*, *};
use std::time::Duration;
use zeron_proto::{SessionSurface, SessionSurfaceState};
use zeron_rpc::methods;

pub(crate) struct SessionSurfaceControl {
    pub chat: String,
    state: Entity<AppState>,
    remote: Option<SessionSurfaceState>,
    terminal: Option<Entity<TerminalPanel>>,
    terminal_id: Option<String>,
    switching: bool,
    epoch: u64,
    error: Option<String>,
    _poll: Task<()>,
    _switch: Option<Task<()>>,
}

impl SessionSurfaceControl {
    pub fn new(chat: String, state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Some((client, params, epoch)) = this
                    .update(cx, |this, cx| {
                        this.request(cx)
                            .map(|(client, params)| (client, params, this.epoch))
                    })
                    .ok()
                    .flatten()
                else {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.upgrade().is_none() {
                        break;
                    }
                    continue;
                };
                let result = client
                    .client()
                    .call_as::<SessionSurfaceState>(methods::GET_SESSION_SURFACE, params)
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.switching || this.epoch != epoch {
                            return;
                        }
                        match result {
                            Ok(state) => this.accept(state, cx),
                            Err(e) => {
                                let error = e.to_string();
                                if this.error.as_ref() != Some(&error) {
                                    this.error = Some(error);
                                    cx.notify();
                                }
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        Self {
            chat,
            state,
            remote: None,
            terminal: None,
            terminal_id: None,
            switching: false,
            epoch: 0,
            error: None,
            _poll: poll,
            _switch: None,
        }
    }

    fn request(&self, cx: &App) -> Option<(crate::state::EngineHandle, serde_json::Value)> {
        let state = self.state.read(cx);
        let engine = state.engine()?;
        let row = state.chats.iter().find(|chat| chat.id == self.chat)?;
        let mut params = serde_json::json!({"chatId": self.chat});
        if state.local_device_id.as_deref() != Some(&row.device_id) {
            params["targetDeviceId"] = row.device_id.clone().into();
        }
        Some((engine.clone(), params))
    }

    pub(crate) fn accept(&mut self, state: SessionSurfaceState, cx: &mut Context<Self>) {
        if let Some(session) = &state.terminal {
            if self.terminal_id.as_deref() != Some(&session.id) {
                let chat = self.chat.clone();
                let panel =
                    cx.new(|cx| TerminalPanel::new_for_chat(self.state.clone(), chat.clone(), cx));
                let target = self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat)
                    .map(|c| c.device_id.clone());
                panel.update(cx, |panel, cx| {
                    let key = panel.reserve_tab_for_chat(chat.clone(), "Session CLI", cx);
                    panel.attach_reserved_session(&chat, key, session.clone(), target, cx);
                });
                self.terminal = Some(panel);
                self.terminal_id = Some(session.id.clone());
            }
        } else if state.surface == SessionSurface::Chat {
            self.terminal = None;
            self.terminal_id = None;
        }
        if self.remote.as_ref() != Some(&state) {
            self.remote = Some(state);
            cx.notify();
        }
    }

    pub fn is_cli(&self) -> bool {
        self.switching
            || self
                .remote
                .as_ref()
                .is_some_and(|s| s.surface == SessionSurface::Cli)
    }

    pub fn body(&self, cx: &App) -> AnyElement {
        let theme = Theme::of(cx);
        let body = if self.switching {
            div().child("Switching session view…").into_any_element()
        } else if let Some(terminal) = &self.terminal {
            div().size_full().child(terminal.clone()).into_any_element()
        } else {
            div()
                .p(px(24.0))
                .child("The CLI has exited. Select Chat to load its messages and continue.")
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.bg)
            .text_color(theme.text_muted)
            .text_size(px(12.0))
            .when_some(self.error.clone(), |el, error| {
                el.child(div().p(px(8.0)).child(error))
            })
            .child(body)
            .into_any_element()
    }

    fn switch(&mut self, target: SessionSurface, cx: &mut Context<Self>) {
        if self.switching
            || self
                .remote
                .as_ref()
                .is_none_or(|s| !s.can_switch || s.surface == target)
        {
            return;
        }
        let Some((client, mut params)) = self.request(cx) else {
            return;
        };
        params["target"] = serde_json::to_value(target).unwrap();
        self.switching = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.error = None;
        cx.notify();
        self._switch = Some(cx.spawn(async move |this, cx| {
            let result = client
                .client()
                .call_as::<SessionSurfaceState>(methods::SWITCH_SESSION_SURFACE, params)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.switching = false;
                match result {
                    Ok(state) => {
                        this.accept(state, cx);
                        if let Some(terminal) = &this.terminal {
                            terminal.update(cx, |panel, cx| panel.request_focus(cx));
                        }
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }
}

struct SurfaceTooltip(String);
impl Render for SurfaceTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .max_w(px(340.0))
            .p(px(8.0))
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .text_color(theme.text)
            .text_size(px(11.0))
            .child(self.0.clone())
    }
}

impl Render for SessionSurfaceControl {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let active = self.remote.as_ref().map(|s| s.surface).unwrap_or_default();
        let enabled = !self.switching && self.remote.as_ref().is_some_and(|s| s.can_switch);
        let reason = self
            .error
            .clone()
            .or_else(|| self.remote.as_ref().and_then(|s| s.reason.clone()))
            .unwrap_or_else(|| {
                if self.switching {
                    "Switching session view…"
                } else {
                    "Use the same session in Chat or its native CLI"
                }
                .into()
            });
        div()
            .id("session-view-toggle")
            .flex_none()
            .flex()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .tooltip(move |_, cx| cx.new(|_| SurfaceTooltip(reason.clone())).into())
            .children(
                [(SessionSurface::Chat, "Chat"), (SessionSurface::Cli, "CLI")]
                    .into_iter()
                    .map(|(target, label)| {
                        let selected = active == target;
                        div()
                            .id(label)
                            .role(Role::Button)
                            .aria_label(format!("Switch session to {label}"))
                            .px(px(7.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .when(selected, |el| el.bg(theme.wash(0.1)))
                            .when(!selected && !enabled, |el| el.opacity(0.45))
                            .when(enabled && !selected, |el| {
                                el.cursor_pointer().hover(|s| s.bg(theme.wash(0.08)))
                            })
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.switch(target, cx);
                            }))
                            .child(label)
                    }),
            )
    }
}
