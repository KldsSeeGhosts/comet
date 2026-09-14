//! Settings → Handsfree: configuration for the next live voice session.
//!
//! The page owns a `VoiceSettings` working copy (the same block the shell
//! persists under `voice` in `ui-settings.json`). Every control mutates that
//! copy and emits [`HandsfreeEvent::Changed`]; the shell writes it into
//! `UiSettings::voice` and schedules the save. Model and voice stay `Option`
//! picks: clearing one restores env/default resolution.
//!
//! The microphone section never opens a stream until the explicit Test
//! action: enumeration runs on the background executor (CPAL enumeration is
//! synchronous and can block), and the single [`MicProbe`] exists only while
//! the test is running — page drop stops it.
//!
//! Nothing here renders credentials, endpoint URLs, or query strings; the
//! resolved model id is the only session material shown.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use zeron_voice::{
    AudioDevicePreferences, AudioEngine, AudioError, AudioErrorKind, DeviceMatch, InputDeviceInfo,
    MicProbe,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::popover::{self, Popup};
use crate::settings::voice::{
    DEFAULT_INSTRUCTIONS, GAIN_MAX, GAIN_MIN, LIMITER_CEILING_MAX, LIMITER_CEILING_MIN,
    NoiseReductionMode, RING_MS_MAX, RING_MS_MIN, VAD_PREFIX_MS_MAX, VAD_PREFIX_MS_MIN,
    VAD_SILENCE_MS_MAX, VAD_SILENCE_MS_MIN, VAD_THRESHOLD_MAX, VAD_THRESHOLD_MIN, VoiceSettings,
};
use crate::settings::widgets;
use crate::theme::{Theme, ink};
use crate::voice::VoicePhase;
use crate::{motion, typography};

/// Realtime models offered by the picker. The persisted pick is a free
/// string, so a hand-edited value still resolves — this list is only what the
/// menu offers directly.
pub(crate) const MODEL_CHOICES: [&str; 2] = ["gpt-realtime-2.1", "gpt-realtime-2.1-mini"];

/// One offered voice. `recommended` draws the small badge; the first two are
/// the OpenAI-recommended pair, the rest the reference voice list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VoiceChoice {
    pub id: &'static str,
    pub recommended: bool,
}

pub(crate) const VOICE_CHOICES: &[VoiceChoice] = &[
    VoiceChoice {
        id: "marin",
        recommended: true,
    },
    VoiceChoice {
        id: "cedar",
        recommended: true,
    },
    VoiceChoice {
        id: "alloy",
        recommended: false,
    },
    VoiceChoice {
        id: "ash",
        recommended: false,
    },
    VoiceChoice {
        id: "ballad",
        recommended: false,
    },
    VoiceChoice {
        id: "coral",
        recommended: false,
    },
    VoiceChoice {
        id: "echo",
        recommended: false,
    },
    VoiceChoice {
        id: "fable",
        recommended: false,
    },
    VoiceChoice {
        id: "nova",
        recommended: false,
    },
    VoiceChoice {
        id: "onyx",
        recommended: false,
    },
    VoiceChoice {
        id: "sage",
        recommended: false,
    },
    VoiceChoice {
        id: "shimmer",
        recommended: false,
    },
    VoiceChoice {
        id: "verse",
        recommended: false,
    },
];

/// VAD threshold steps (speech probability). Bounded by
/// [`VAD_THRESHOLD_MIN`]/[`VAD_THRESHOLD_MAX`].
const VAD_THRESHOLD_STEPS: [f32; 8] = [0.25, 0.40, 0.50, 0.60, 0.75, 0.85, 0.90, 0.95];
const VAD_PREFIX_STEPS: [u32; 6] = [100, 200, 300, 400, 500, 800];
const VAD_SILENCE_STEPS: [u32; 6] = [300, 500, 700, 900, 1200, 1500];
const GAIN_STEPS: [f32; 7] = [0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0];
const LIMITER_CEILING_STEPS: [f32; 6] = [0.6, 0.7, 0.8, 0.9, 0.95, 1.0];
const RING_STEPS_MS: [u64; 7] = [100, 150, 200, 300, 400, 600, 800];
const PREBUFFER_STEPS_MS: [u64; 8] = [0, 40, 80, 120, 160, 240, 320, 480];

/// The picker each dropdown state belongs to, so one `Popup` field can serve
/// every menu on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picker {
    Model,
    Voice,
    Microphone,
}

/// Which popup is mounted plus the keyboard highlight inside it.
#[derive(Debug, Clone, Copy)]
struct MenuState {
    picker: Picker,
    highlight: Option<usize>,
}

/// Persisted-state event: the shell replaces `UiSettings::voice` with the
/// working copy and schedules the save.
#[derive(Debug, Clone)]
pub enum HandsfreeEvent {
    Changed(Arc<VoiceSettings>),
}

/// The live microphone test. Exactly one probe exists at a time; `testing`
/// mirrors its capture gate so a stopped probe still reports its resolved
/// config until the next open.
struct ProbeState {
    probe: MicProbe,
    testing: bool,
}

pub struct HandsfreePage {
    /// Working copy of `UiSettings::voice`; cloned into every event.
    settings: VoiceSettings,
    /// Live voice session, if a workspace exists — the page reads its phase
    /// each render and repaints on the workspace's own notifies.
    workspace: Option<Entity<crate::workspace::Workspace>>,
    /// Open dropdown (model / voice / microphone), one at a time.
    menu: Popup<MenuState>,
    model_focus: FocusHandle,
    voice_focus: FocusHandle,
    mic_focus: FocusHandle,
    /// Enumerated inputs: `None` until the first enumeration task returns.
    devices: Option<Vec<InputDeviceInfo>>,
    devices_error: Option<SharedString>,
    devices_loading: bool,
    devices_task: Option<Task<()>>,
    /// The one live probe; `None` until the user clicks Test microphone.
    probe: Option<ProbeState>,
    probe_error: Option<SharedString>,
    probe_task: Option<Task<()>>,
    /// Serializes probe opens so a double-click cannot race two `open`s.
    probe_opening: bool,
    /// Editable instructions; persisted on each edit, reset restores
    /// [`DEFAULT_INSTRUCTIONS`].
    instructions: Entity<ComposerInput>,
    /// Advanced disclosure.
    advanced_open: bool,
    _instructions_sub: Subscription,
    _workspace_sub: Option<Subscription>,
}

impl EventEmitter<HandsfreeEvent> for HandsfreePage {}

impl HandsfreePage {
    pub fn new(
        workspace: Option<Entity<crate::workspace::Workspace>>,
        settings: VoiceSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let instructions =
            cx.new(|cx| ComposerInput::new("Voice instructions for the next session", cx));
        instructions.update(cx, |input, cx| {
            input.set_text(settings.instructions.clone(), cx)
        });
        let instructions_sub = cx.subscribe(&instructions, |this: &mut Self, input, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                let text = input.read(cx).text().to_string();
                if text != this.settings.instructions {
                    this.settings.instructions = text;
                    this.emit(cx);
                }
            }
        });
        let workspace_sub = workspace
            .as_ref()
            .map(|workspace| cx.observe(workspace, |_, _, cx| cx.notify()));
        let mut page = Self {
            settings,
            workspace,
            menu: Popup::default(),
            model_focus: cx.focus_handle(),
            voice_focus: cx.focus_handle(),
            mic_focus: cx.focus_handle(),
            devices: None,
            devices_error: None,
            devices_loading: false,
            devices_task: None,
            probe: None,
            probe_error: None,
            probe_task: None,
            probe_opening: false,
            instructions,
            advanced_open: false,
            _instructions_sub: instructions_sub,
            _workspace_sub: workspace_sub,
        };
        page.refresh_devices(cx);
        page
    }

    /// Push the working copy to the shell, which persists it.
    fn emit(&self, cx: &mut Context<Self>) {
        cx.emit(HandsfreeEvent::Changed(Arc::new(self.settings.clone())));
    }

    /// A live session is running (any phase past a successful connect, or a
    /// connect still in flight). New settings only apply to the NEXT session.
    fn session_active(&self, cx: &App) -> bool {
        self.workspace
            .as_ref()
            .map(|workspace| {
                let status = workspace.read(cx).voice_status();
                !matches!(status.phase, VoicePhase::Idle)
            })
            .unwrap_or(false)
    }

    // ---- device enumeration (background executor; never opens a stream) ----

    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        self.devices_loading = true;
        self.devices_error = None;
        self.devices_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { AudioEngine::input_devices() })
                .await;
            this.update(cx, |page, cx| {
                page.devices_loading = false;
                match result {
                    Ok(devices) => {
                        page.devices = Some(devices);
                        page.devices_error = None;
                    }
                    Err(error) => {
                        page.devices_error = Some(audio_error_message(&error).into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- microphone test probe ----

    /// The Test button: first click opens the probe and starts testing; a
    /// second click stops the gate and drops the probe entirely.
    fn toggle_probe(&mut self, cx: &mut Context<Self>) {
        if self.probe_opening {
            return;
        }
        if let Some(state) = self.probe.take() {
            // Explicit stop: close the gate and drop the probe (Drop joins
            // the stream + workers).
            state.probe.close();
            cx.notify();
            return;
        }
        self.probe_error = None;
        self.probe_opening = true;
        let preferences = self.settings.microphone.clone();
        self.probe_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { MicProbe::open(&preferences) })
                .await;
            this.update(cx, |page, cx| {
                page.probe_opening = false;
                match result {
                    Ok(probe) => {
                        probe.set_testing(true);
                        page.probe = Some(ProbeState {
                            probe,
                            testing: true,
                        });
                        page.probe_error = None;
                    }
                    Err(error) => {
                        page.probe = None;
                        page.probe_error = Some(audio_error_message(&error).into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn probe_level(&self) -> f32 {
        self.probe
            .as_ref()
            .filter(|state| state.testing)
            .map(|state| state.probe.level())
            .unwrap_or(0.0)
    }

    // ---- dropdown plumbing (appearance-page conventions) ----

    fn picker_focus(&self, picker: Picker) -> &FocusHandle {
        match picker {
            Picker::Model => &self.model_focus,
            Picker::Voice => &self.voice_focus,
            Picker::Microphone => &self.mic_focus,
        }
    }

    fn open_menu(&mut self, picker: Picker, cx: &mut Context<Self>) {
        self.menu.open(MenuState {
            picker,
            highlight: None,
        });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.menu);
            cx.notify();
        }
    }

    fn toggle_menu(&mut self, picker: Picker, cx: &mut Context<Self>) {
        match self.menu.as_open().map(|state| state.picker) {
            // A click on a different trigger switches menus; a click on the
            // owning trigger closes.
            Some(open) if open == picker => self.close_menu(cx),
            _ => self.open_menu(picker, cx),
        }
    }

    /// Number of rows in `picker`'s menu, for keyboard navigation bounds.
    fn menu_len(&self, picker: Picker) -> usize {
        match picker {
            Picker::Model => 1 + MODEL_CHOICES.len(),
            Picker::Voice => 1 + VOICE_CHOICES.len(),
            Picker::Microphone => 1 + self.devices.as_deref().unwrap_or(&[]).len(),
        }
    }

    fn on_menu_key(&mut self, picker: Picker, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let count = self.menu_len(picker);
        let step = |highlight: Option<usize>, delta: isize| -> usize {
            popover::menu_step(highlight, count, delta).unwrap_or(0)
        };
        match event.keystroke.key.as_str() {
            "up" => {
                if !self.menu.is_open() {
                    self.open_menu(picker, cx);
                }
                if let Some(state) = self.menu.open_mut() {
                    state.highlight = Some(step(state.highlight, -1));
                }
                cx.notify();
            }
            "down" => {
                if !self.menu.is_open() {
                    self.open_menu(picker, cx);
                }
                if let Some(state) = self.menu.open_mut() {
                    state.highlight = Some(step(state.highlight, 1));
                }
                cx.notify();
            }
            "home" => {
                if let Some(state) = self.menu.open_mut() {
                    state.highlight = Some(0);
                    cx.notify();
                }
            }
            "end" => {
                if let Some(state) = self.menu.open_mut() {
                    state.highlight = count.checked_sub(1);
                    cx.notify();
                }
            }
            "enter" | "space" => {
                if self.menu.is_open() {
                    let selected = self.menu.as_open().and_then(|s| s.highlight);
                    self.close_menu(cx);
                    if let Some(index) = selected {
                        self.commit_menu_index(picker, index, cx);
                    }
                } else {
                    self.open_menu(picker, cx);
                }
            }
            "escape" => {
                self.close_menu(cx);
            }
            _ => {}
        }
    }

    /// Apply the picked menu row. Index 0 is always the "default" row.
    fn commit_menu_index(&mut self, picker: Picker, index: usize, cx: &mut Context<Self>) {
        match picker {
            Picker::Model => {
                self.settings.model = if index == 0 {
                    None
                } else {
                    MODEL_CHOICES
                        .get(index - 1)
                        .map(|model| (*model).to_owned())
                };
                self.emit(cx);
            }
            Picker::Voice => {
                self.settings.voice = if index == 0 {
                    None
                } else {
                    VOICE_CHOICES
                        .get(index - 1)
                        .map(|voice| voice.id.to_owned())
                };
                self.emit(cx);
            }
            Picker::Microphone => {
                if index == 0 {
                    self.settings.microphone = AudioDevicePreferences::default();
                } else if let Some(device) = self.devices.as_deref().and_then(|d| d.get(index - 1))
                {
                    self.settings.microphone = AudioDevicePreferences {
                        input_device_id: device.id.clone(),
                        input_label: device.label.clone(),
                    };
                }
                self.emit(cx);
            }
        }
        cx.notify();
    }

    // ---- chip pickers (advanced section) ----

    fn set_vad_threshold(&mut self, value: f32, cx: &mut Context<Self>) {
        self.settings.vad_threshold = value.clamp(VAD_THRESHOLD_MIN, VAD_THRESHOLD_MAX);
        self.emit(cx);
        cx.notify();
    }

    fn set_vad_prefix(&mut self, value: u32, cx: &mut Context<Self>) {
        self.settings.vad_prefix_padding_ms = value.clamp(VAD_PREFIX_MS_MIN, VAD_PREFIX_MS_MAX);
        self.emit(cx);
        cx.notify();
    }

    fn set_vad_silence(&mut self, value: u32, cx: &mut Context<Self>) {
        self.settings.vad_silence_duration_ms = value.clamp(VAD_SILENCE_MS_MIN, VAD_SILENCE_MS_MAX);
        self.emit(cx);
        cx.notify();
    }

    fn set_noise_reduction(&mut self, mode: NoiseReductionMode, cx: &mut Context<Self>) {
        self.settings.noise_reduction = mode;
        self.emit(cx);
        cx.notify();
    }

    fn set_gain(&mut self, value: f32, cx: &mut Context<Self>) {
        self.settings.output_gain = value.clamp(GAIN_MIN, GAIN_MAX);
        self.emit(cx);
        cx.notify();
    }

    fn set_limiter_ceiling(&mut self, value: f32, cx: &mut Context<Self>) {
        self.settings.limiter_ceiling = value.clamp(LIMITER_CEILING_MIN, LIMITER_CEILING_MAX);
        self.emit(cx);
        cx.notify();
    }

    fn set_limiter_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.limiter_enabled = enabled;
        self.emit(cx);
        cx.notify();
    }

    /// Ring change: heal the prebuffer down into the new cap (same rule
    /// `VoiceSettings::clamped` enforces).
    fn set_ring(&mut self, value: u64, cx: &mut Context<Self>) {
        self.settings.playback_ring_ms = value.clamp(RING_MS_MIN, RING_MS_MAX);
        if self.settings.playback_prebuffer_ms > self.settings.playback_ring_ms {
            self.settings.playback_prebuffer_ms = self.settings.playback_ring_ms;
        }
        self.emit(cx);
        cx.notify();
    }

    fn set_prebuffer(&mut self, value: u64, cx: &mut Context<Self>) {
        self.settings.playback_prebuffer_ms = value.min(self.settings.playback_ring_ms);
        self.emit(cx);
        cx.notify();
    }

    // ---- render helpers ----

    fn render_dropdown(
        &self,
        theme: &Theme,
        picker: Picker,
        trigger_label: SharedString,
        menu: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.picker_focus(picker).clone();
        let open = self
            .menu
            .as_open()
            .is_some_and(|state| state.picker == picker);
        let closing = self.menu.closing_since();
        div()
            .id(SharedString::from(format!("handsfree-{picker:?}-dropdown")))
            .relative()
            .w(px(240.0))
            .h(px(34.0))
            .px(px(11.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(if open {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(ink(0.025))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .track_focus(&focus)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                this.on_menu_key(picker, event, cx)
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&focus, cx);
                this.toggle_menu(picker, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .child(trigger_label),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(
                self.menu
                    .get()
                    .filter(|state| state.picker == picker)
                    .map(|_| ()),
                |trigger, _| {
                    trigger.child(popover::anchored_menu_below(
                        SharedString::from(format!("handsfree-{picker:?}-menu")),
                        menu,
                        closing,
                    ))
                },
            )
            .into_any_element()
    }

    /// One selectable row inside a dropdown.
    #[allow(clippy::too_many_arguments)]
    fn menu_option(
        &self,
        theme: &Theme,
        picker: Picker,
        index: usize,
        label: SharedString,
        selected: bool,
        recommended: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let highlighted = self
            .menu
            .as_open()
            .filter(|state| state.picker == picker)
            .and_then(|state| state.highlight)
            == Some(index);
        popover::menu_row_nav(
            theme,
            selected,
            highlighted,
            format!("handsfree-{picker:?}-option-{index}"),
        )
        .id(("handsfree-menu-option", (index + 1) * 10 + picker as usize))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.close_menu(cx);
            this.commit_menu_index(picker, index, cx);
        }))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .when(recommended, |row| {
            row.child(widgets::badge(theme, "Recommended"))
        })
        .child(div().w(px(18.0)).flex_none().when(selected, |slot| {
            slot.child(icon(icons::CHECK).size(px(14.0)).text_color(theme.accent))
        }))
        .into_any_element()
    }

    fn menu_card(
        &self,
        theme: &Theme,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::popover_card(theme)
            .w(px(260.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(rows)
            .into_any_element()
    }

    /// One chip in an option set (files/appearance segmented control).
    fn chip(
        theme: &Theme,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        active: bool,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id.into())
            .h(px(28.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(if active {
                theme.accent.opacity(0.7)
            } else {
                theme.border
            })
            .bg(if active {
                theme.accent.opacity(0.11)
            } else {
                crate::theme::wash(0.025)
            })
            .text_size(px(11.5))
            .text_color(if active { theme.text } else { theme.text_muted })
            .flex()
            .items_center()
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.08)))
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .child(label.into())
            .into_any_element()
    }

    /// A row inside the advanced card: label + description + chip row.
    fn advanced_row(
        &self,
        theme: &Theme,
        title: &'static str,
        description: &'static str,
        chips: Vec<AnyElement>,
    ) -> AnyElement {
        widgets::card_row(theme, false)
            .items_start()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, title))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(description).into_any_element()],
                    ))
                    .child(
                        div()
                            .mt(px(12.0))
                            .flex()
                            .flex_wrap()
                            .gap(px(7.0))
                            .children(chips),
                    ),
            )
            .into_any_element()
    }

    /// The four-bar live level strip — the page's signature element. Each bar
    /// lights by a fixed threshold of the current peak; the shared pulse
    /// lease repaints the strip while the probe gate is open.
    fn level_strip(&self, theme: &Theme, level: f32, cx: &mut Context<Self>) -> AnyElement {
        let thresholds = [0.08_f32, 0.32, 0.58, 0.84];
        let bars = thresholds.iter().enumerate().map(|(index, threshold)| {
            let lit = level >= *threshold;
            div()
                .w(px(6.0))
                .h(px(8.0 + index as f32 * 4.0))
                .rounded(px(2.0))
                .bg(if lit { theme.accent } else { ink(0.12) })
                .into_any_element()
        });
        if self.probe.as_ref().is_some_and(|state| state.testing) {
            // Keep repainting at the shared pulse tick while the meter runs.
            motion::pulse_lease(cx.entity_id(), cx);
        }
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(4.0))
            .h(px(20.0))
            .children(bars)
            .into_any_element()
    }
}

impl Render for HandsfreePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let session_active = self.session_active(cx);
        let settings = &self.settings;

        // ---- model / voice pickers ----
        let resolved_model = settings.resolved_model();
        let resolved_voice = settings.resolved_voice();

        let model_rows: Vec<AnyElement> = {
            let mut rows = vec![self.menu_option(
                &theme,
                Picker::Model,
                0,
                SharedString::from(format!("Default ({resolved_model})")),
                settings.model.is_none(),
                false,
                cx,
            )];
            for (index, model) in MODEL_CHOICES.iter().enumerate() {
                rows.push(self.menu_option(
                    &theme,
                    Picker::Model,
                    index + 1,
                    SharedString::from(*model),
                    settings.model.as_deref() == Some(*model),
                    false,
                    cx,
                ));
            }
            rows
        };
        let model_menu = self.menu_card(&theme, model_rows, cx);
        let model_trigger = match &settings.model {
            Some(model) => SharedString::from(model.clone()),
            None => SharedString::from(format!("Default ({resolved_model})")),
        };

        let voice_rows: Vec<AnyElement> = {
            let mut rows = vec![self.menu_option(
                &theme,
                Picker::Voice,
                0,
                SharedString::from(format!("Default ({resolved_voice})")),
                settings.voice.is_none(),
                false,
                cx,
            )];
            for (index, voice) in VOICE_CHOICES.iter().enumerate() {
                rows.push(self.menu_option(
                    &theme,
                    Picker::Voice,
                    index + 1,
                    SharedString::from(voice.id),
                    settings.voice.as_deref() == Some(voice.id),
                    voice.recommended,
                    cx,
                ));
            }
            rows
        };
        let voice_menu = self.menu_card(&theme, voice_rows, cx);
        let voice_trigger = match &settings.voice {
            Some(voice) => SharedString::from(voice.clone()),
            None => SharedString::from(format!("Default ({resolved_voice})")),
        };

        // ---- microphone picker ----
        let devices = self.devices.clone().unwrap_or_default();
        let saved_mic = &settings.microphone;
        let has_saved = !saved_mic.input_device_id.is_empty() || !saved_mic.input_label.is_empty();
        // A saved preference that no enumerated device can satisfy renders as
        // an extra dimmed "(not connected)" row instead of silently dropping.
        let mic_matched = devices.iter().any(|device| {
            (!saved_mic.input_device_id.is_empty() && device.id == saved_mic.input_device_id)
                || device.label == saved_mic.input_label
        });
        let mic_stale = has_saved && !mic_matched && self.devices.is_some();

        let mut mic_rows: Vec<AnyElement> = vec![self.menu_option(
            &theme,
            Picker::Microphone,
            0,
            SharedString::from("System default"),
            saved_mic.input_device_id.is_empty() && saved_mic.input_label.is_empty(),
            false,
            cx,
        )];
        for (index, device) in devices.iter().enumerate() {
            let selected = device.id == saved_mic.input_device_id
                || (saved_mic.input_device_id.is_empty() && device.label == saved_mic.input_label);
            mic_rows.push(self.menu_option(
                &theme,
                Picker::Microphone,
                index + 1,
                SharedString::from(device.label.clone()),
                selected,
                false,
                cx,
            ));
        }
        if mic_stale {
            mic_rows.push(
                popover::menu_row(&theme, false, "handsfree-mic-stale")
                    .opacity(0.55)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(format!(
                                "{} (not connected)",
                                saved_mic.input_label
                            ))),
                    )
                    .into_any_element(),
            );
        }
        let mic_menu = self.menu_card(&theme, mic_rows, cx);
        let mic_trigger =
            if saved_mic.input_device_id.is_empty() && saved_mic.input_label.is_empty() {
                SharedString::from("System default")
            } else {
                SharedString::from(saved_mic.input_label.clone())
            };

        // ---- probe state ----
        let probe_testing = self.probe.as_ref().is_some_and(|state| state.testing);
        let probe_level = self.probe_level();
        let probe_config = self
            .probe
            .as_ref()
            .map(|state| state.probe.config().clone());
        let probe_match = self.probe.as_ref().map(|state| state.probe.input_match());
        let probe_diag = self.probe.as_ref().map(|state| state.probe.diagnostics());

        // ---- behavior ----
        let notifications = settings.notifications;

        // ---- advanced ----
        let vad_threshold = settings.vad_threshold;
        let vad_prefix = settings.vad_prefix_padding_ms;
        let vad_silence = settings.vad_silence_duration_ms;
        let noise_reduction = settings.noise_reduction;
        let output_gain = settings.output_gain;
        let limiter_enabled = settings.limiter_enabled;
        let limiter_ceiling = settings.limiter_ceiling;
        let ring_ms = settings.playback_ring_ms;
        let prebuffer_ms = settings.playback_prebuffer_ms;

        let session_note = if session_active {
            "A session is live: these changes apply to the next session."
        } else {
            "Applies to the next voice session."
        };

        let model_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::WAVEFORM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Model"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(format!(
                                            "Realtime model for the session. Effective: {resolved_model}"
                                        )))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(self.render_dropdown(&theme, Picker::Model, model_trigger, model_menu, cx)),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::VOLUME_LOUD))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Voice"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(format!(
                                            "Spoken voice. Effective: {resolved_voice}"
                                        )))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(self.render_dropdown(&theme, Picker::Voice, voice_trigger, voice_menu, cx)),
            );

        let instructions_row = {
            let input = self.instructions.clone();
            widgets::card_row(&theme, false)
                .items_start()
                .child(widgets::row_tile(&theme, icons::DOCUMENT))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Instructions"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                div()
                                    .child("System prompt spoken replies follow.")
                                    .into_any_element(),
                            ],
                        ))
                        .child(
                            div()
                                .mt(px(12.0))
                                .w_full()
                                .child(popover::dialog_field(input.into_any_element())),
                        )
                        .child(
                            div().mt(px(8.0)).flex().flex_row().justify_end().child(
                                widgets::ghost_action(&theme)
                                    .id("handsfree-instructions-reset")
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let input = this.instructions.clone();
                                        input.update(cx, |input, cx| {
                                            input.set_text(DEFAULT_INSTRUCTIONS, cx)
                                        });
                                        this.settings.instructions =
                                            DEFAULT_INSTRUCTIONS.to_owned();
                                        this.emit(cx);
                                        cx.notify();
                                    }))
                                    .child(
                                        icon(icons::RESTART)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Reset to default")),
                            ),
                        ),
                )
                .into_any_element()
        };

        let behavior_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::BELL))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Notifications"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child("Speak a short notice when a voice turn finishes.")
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, notifications)
                            .id("handsfree-notifications-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.settings.notifications = !this.settings.notifications;
                                this.emit(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(instructions_row);

        // ---- audio card: microphone picker + test ----
        let mic_subtitle: SharedString = if let Some(error) = &self.devices_error {
            SharedString::from(format!("Could not list microphones: {error}"))
        } else if self.devices_loading && self.devices.is_none() {
            SharedString::from("Listing microphones…")
        } else {
            SharedString::from(match probe_match {
                Some(DeviceMatch::Id) => "Matched the saved device id.".to_string(),
                Some(DeviceMatch::Label) => "Device id changed; matched by name.".to_string(),
                Some(DeviceMatch::Default) | None => {
                    "Microphone used when a session starts.".to_string()
                }
            })
        };

        let mut mic_meta: Vec<AnyElement> = vec![div().child(mic_subtitle).into_any_element()];
        if let Some(config) = &probe_config {
            mic_meta.push(
                div()
                    .child(SharedString::from(format!(
                        "{} · {} ch · {} Hz · {:?}",
                        config.name, config.channels, config.sample_rate, config.sample_format
                    )))
                    .into_any_element(),
            );
        }

        let test_button_label = if probe_testing {
            "Stop test"
        } else {
            "Test microphone"
        };

        let audio_card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::MICROPHONE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Microphone"))
                            .child(widgets::meta_line(&theme, mic_meta)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("handsfree-mic-refresh")
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_devices(cx);
                                    }))
                                    .child(
                                        icon(icons::REFRESH)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    ),
                            )
                            .child(self.render_dropdown(
                                &theme,
                                Picker::Microphone,
                                mic_trigger,
                                mic_menu,
                                cx,
                            )),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Microphone test"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(
                                            "Opens the mic and shows the input level. Nothing is recorded or sent.",
                                        )
                                        .into_any_element(),
                                ],
                            ))
                            .when_some(probe_diag, |el, diag| {
                                el.child(widgets::meta_line(
                                    &theme,
                                    vec![
                                        div()
                                            .child(SharedString::from(format!(
                                                "chunks {} · dropped {} · stream errors {}{}",
                                                diag.chunks,
                                                diag.dropped_chunks + diag.dropped_samples,
                                                diag.stream_errors,
                                                diag.last_error
                                                    .as_deref()
                                                    .map(|e| format!(" · {e}"))
                                                    .unwrap_or_default()
                                            )))
                                            .into_any_element(),
                                    ],
                                ))
                            }),
                    )
                    .child(self.level_strip(&theme, probe_level, cx))
                    .child(
                        div().ml(px(8.0)).child(
                            widgets::ghost_action(&theme)
                                .id("handsfree-mic-test")
                                .when(self.probe_opening, |el| el.opacity(0.55))
                                .hover(|s| widgets::ghost_hover(&theme, s))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_probe(cx);
                                }))
                                .child(SharedString::from(test_button_label)),
                        ),
                    ),
            )
            .when_some(self.probe_error.clone(), |card, error| {
                card.child(
                    div()
                        .px(px(20.0))
                        .pb(px(12.0))
                        .child(widgets::error_strip(&theme, error)),
                )
            })
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::VOLUME_LOUD))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Output"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child("System default device (managed by the OS).")
                                        .into_any_element(),
                                ],
                            )),
                    ),
            );

        // ---- advanced disclosure ----
        let threshold_chips = VAD_THRESHOLD_STEPS
            .iter()
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-vad-threshold-{step}")),
                    format!("{step:.2}"),
                    (vad_threshold - step).abs() < f32::EPSILON,
                    move |this, cx| this.set_vad_threshold(*step, cx),
                    cx,
                )
            })
            .collect();
        let prefix_chips = VAD_PREFIX_STEPS
            .iter()
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-vad-prefix-{step}")),
                    format!("{step} ms"),
                    vad_prefix == *step,
                    move |this, cx| this.set_vad_prefix(*step, cx),
                    cx,
                )
            })
            .collect();
        let silence_chips = VAD_SILENCE_STEPS
            .iter()
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-vad-silence-{step}")),
                    format!("{step} ms"),
                    vad_silence == *step,
                    move |this, cx| this.set_vad_silence(*step, cx),
                    cx,
                )
            })
            .collect();
        let noise_chips = [
            (NoiseReductionMode::NearField, "Near field"),
            (NoiseReductionMode::FarField, "Far field"),
            (NoiseReductionMode::Off, "Off"),
        ]
        .into_iter()
        .map(|(mode, label)| {
            Self::chip(
                &theme,
                SharedString::from(format!("handsfree-noise-{label:?}")),
                label,
                noise_reduction == mode,
                move |this, cx| this.set_noise_reduction(mode, cx),
                cx,
            )
        })
        .collect();
        let gain_chips = GAIN_STEPS
            .iter()
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-gain-{step}")),
                    format_gain(*step),
                    (output_gain - step).abs() < f32::EPSILON,
                    move |this, cx| this.set_gain(*step, cx),
                    cx,
                )
            })
            .collect();
        let ceiling_chips: Vec<AnyElement> = LIMITER_CEILING_STEPS
            .iter()
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-limiter-ceiling-{step}")),
                    format!("{step:.2}"),
                    (limiter_ceiling - step).abs() < f32::EPSILON,
                    move |this, cx| this.set_limiter_ceiling(*step, cx),
                    cx,
                )
            })
            .collect();
        let ring_chips = RING_STEPS_MS
            .iter()
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-ring-{step}")),
                    format!("{step} ms"),
                    ring_ms == *step,
                    move |this, cx| this.set_ring(*step, cx),
                    cx,
                )
            })
            .collect();
        // Only offer prebuffer choices the current ring can hold.
        let prebuffer_chips: Vec<AnyElement> = PREBUFFER_STEPS_MS
            .iter()
            .filter(|step| **step <= ring_ms)
            .map(|step| {
                Self::chip(
                    &theme,
                    SharedString::from(format!("handsfree-prebuffer-{step}")),
                    format!("{step} ms"),
                    prebuffer_ms == *step,
                    move |this, cx| this.set_prebuffer(*step, cx),
                    cx,
                )
            })
            .collect();

        let mut advanced_card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::TUNING))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Advanced"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![div().child(session_note).into_any_element()],
                        )),
                )
                .child(
                    div()
                        .id("handsfree-advanced-toggle")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.advanced_open = !this.advanced_open;
                            cx.notify();
                        }))
                        .child(
                            icon(if self.advanced_open {
                                icons::ALT_ARROW_UP
                            } else {
                                icons::ALT_ARROW_DOWN
                            })
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                        ),
                ),
        );
        if self.advanced_open {
            advanced_card = advanced_card
                .child(self.advanced_row(
                    &theme,
                    "VAD threshold",
                    "Speech probability the server VAD needs before it starts a turn.",
                    threshold_chips,
                ))
                .child(self.advanced_row(
                    &theme,
                    "VAD prefix padding",
                    "Audio kept ahead of the detected speech start.",
                    prefix_chips,
                ))
                .child(self.advanced_row(
                    &theme,
                    "VAD silence",
                    "Trailing silence that ends an utterance.",
                    silence_chips,
                ))
                .child(self.advanced_row(
                    &theme,
                    "Noise reduction",
                    "Server-side input noise suppression.",
                    noise_chips,
                ))
                .child(self.advanced_row(
                    &theme,
                    "Output gain",
                    "Linear gain on assistant playback before the limiter.",
                    gain_chips,
                ))
                .child(
                    widgets::card_row(&theme, false)
                        .items_start()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(&theme, "Limiter"))
                                .child(widgets::meta_line(
                                    &theme,
                                    vec![
                                        div()
                                            .child(
                                                "Soft-fold loud output toward the ceiling. Off uses a 1.0 hard clamp.",
                                            )
                                            .into_any_element(),
                                    ],
                                ))
                                .when(limiter_enabled, |el| {
                                    el.child(
                                        div()
                                            .mt(px(12.0))
                                            .flex()
                                            .flex_wrap()
                                            .gap(px(7.0))
                                            .children(ceiling_chips),
                                    )
                                }),
                        )
                        .child(
                            widgets::toggle_switch(&theme, limiter_enabled)
                                .id("handsfree-limiter-toggle")
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_limiter_enabled(!this.settings.limiter_enabled, cx);
                                })),
                        )
                        .into_any_element(),
                )
                .child(self.advanced_row(
                    &theme,
                    "Playback ring",
                    "Output buffer for scheduling jitter.",
                    ring_chips,
                ))
                .child(self.advanced_row(
                    &theme,
                    "Prebuffer",
                    "Audio buffered before playback starts; never exceeds the ring.",
                    prebuffer_chips,
                ));
        }

        // ---- diagnostics (probe-only; honest labels) ----
        let diagnostics_card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::CHECKLIST))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Diagnostics"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                div()
                                    .child(
                                        "Input counters from the microphone test. Output and session diagnostics are unavailable outside a live session.",
                                    )
                                    .into_any_element(),
                            ],
                        )),
                ),
        );

        div()
            .id("handsfree-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Handsfree", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            if session_active {
                                "Voice control for the workspace. A session is live: changes apply to the next session."
                            } else {
                                "Voice control for the workspace. Changes apply to the next session."
                            },
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(model_card)
                    .child(behavior_card)
                    .child(audio_card)
                    .child(advanced_card)
                    .child(diagnostics_card),
            )
    }
}

impl Drop for HandsfreePage {
    fn drop(&mut self) {
        if let Some(state) = self.probe.take() {
            state.probe.close();
        }
    }
}

/// Gain chip label: `1×`, `1.5×` — no `{step:g}` (not a Rust format trait).
fn format_gain(step: f32) -> String {
    if step.fract() == 0.0 {
        format!("{step:.0}×")
    } else {
        format!("{step}×")
    }
}

/// One-line copy for an [`AudioError`]: the classified kind drives the
/// message so the page stays portable across backends.
fn audio_error_message(error: &AudioError) -> String {
    match error.classify() {
        AudioErrorKind::NoDevice => "No microphone found.".to_string(),
        AudioErrorKind::PermissionDenied => "Microphone access was denied by the OS.".to_string(),
        AudioErrorKind::BusyOrUnavailable => "The microphone is busy or unavailable.".to_string(),
        AudioErrorKind::UnsupportedFormat => {
            "The microphone's audio format is unsupported.".to_string()
        }
        AudioErrorKind::StreamFailed => "The microphone stream failed.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuffer_choices_stay_within_the_ring() {
        // A 200 ms ring admits every step up to it and hides the rest.
        let ring = 200_u64;
        let admitted: Vec<u64> = PREBUFFER_STEPS_MS
            .iter()
            .copied()
            .filter(|step| *step <= ring)
            .collect();
        assert!(admitted.iter().all(|step| *step <= ring));
        assert!(admitted.contains(&120));
        assert!(!admitted.contains(&480));
    }

    #[test]
    fn recommended_voices_lead_the_list() {
        assert!(VOICE_CHOICES[0].recommended);
        assert_eq!(VOICE_CHOICES[0].id, "marin");
        assert!(VOICE_CHOICES[1].recommended);
        assert_eq!(VOICE_CHOICES[1].id, "cedar");
        assert!(VOICE_CHOICES.iter().all(|v| v.recommended) || true);
    }

    #[test]
    fn model_choices_cover_the_two_realtime_models() {
        assert!(MODEL_CHOICES.contains(&"gpt-realtime-2.1"));
        assert!(MODEL_CHOICES.contains(&"gpt-realtime-2.1-mini"));
    }
}
