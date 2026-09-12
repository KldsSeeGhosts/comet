//! Independent input state for opt-in Noches development windows.
//!
//! GPUI still has one logical focus/drag context per application. Agent input
//! is therefore admitted only while the ordinary seat is outside this process.
//! It never borrows ordinary serials, IME, clipboard, pointer or activation.
use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Clone, Copy, Debug)]
pub(super) struct AgentDevice(pub u32);

pub(super) struct AgentSeat {
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_surface: Option<ObjectId>,
    keyboard_surface: Option<ObjectId>,
    keymap: Option<xkb::State>,
    modifiers: Modifiers,
    capslock: Capslock,
    position: Point<Pixels>,
    button: Option<MouseButton>,
    click: ClickState,
    scroll: Point<f32>,
    held_keys: Vec<(u32, Keystroke)>,
}
impl Default for AgentSeat {
    fn default() -> Self {
        Self {
            pointer: None, keyboard: None, pointer_surface: None, keyboard_surface: None,
            keymap: None, modifiers: Modifiers::default(), capslock: Capslock { on: false },
            position: Point::default(), button: None,
            click: ClickState { last_mouse_button: None, last_click: Instant::now(),
                last_location: Point::default(), current_count: 0 },
            scroll: Point::default(), held_keys: Vec::new(),
        }
    }
}
impl Drop for AgentSeat {
    fn drop(&mut self) {
        if let Some(pointer) = self.pointer.take() { pointer.release(); }
        if let Some(keyboard) = self.keyboard.take() { keyboard.release(); }
    }
}

#[derive(Clone)]
pub(crate) struct AgentDispatch {
    pub surface: ObjectId,
    pub position: Point<Pixels>,
    pub modifiers: Modifiers,
    pub capslock: Capslock,
}
struct DispatchGuard(WaylandClientStatePtr);
impl Drop for DispatchGuard {
    fn drop(&mut self) { self.0.get_client().borrow_mut().agent_dispatch = None; }
}

/// A process-generation-bound compatibility declaration, not input authority.
/// Only the dev app_id enables this path; the compositor still binds a live
/// surface and owns the input grant and held-key cleanup.
pub(super) struct AgentRegistration { path: PathBuf, identity: String }
impl AgentRegistration {
    pub fn create() -> std::io::Result<Self> {
        let pid = std::process::id();
        let uid = unsafe { libc::geteuid() };
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
        let root = runtime.join("noches-gpui-input");
        std::fs::DirBuilder::new().mode(0o700).create(&root).or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists { Ok(()) } else { Err(e) }
        })?;
        let meta = std::fs::symlink_metadata(&root)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(std::io::Error::other("unsafe GPUI compatibility directory"));
        }
        let stat = std::fs::read_to_string("/proc/self/stat")?;
        let start = stat.rsplit_once(')').and_then(|(_, tail)| tail.split_whitespace().nth(19))
            .ok_or_else(|| std::io::Error::other("missing process generation"))?;
        let exe = std::fs::metadata("/proc/self/exe")?;
        let record = Self { path: root.join(pid.to_string()),
            identity: format!("noches-gpui-agent-seat-v1\n{pid}\n{start}\n{}\n{}\n", exe.dev(), exe.ino()) };
        record.publish(true)?;
        Ok(record)
    }
    pub fn publish(&self, busy: bool) -> std::io::Result<()> {
        use std::io::Write;
        let temporary = self.path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        let result = (|| {
            writeln!(file, "{}{}", self.identity, if busy { "primary_client_busy" } else { "ready" })?;
            std::fs::rename(&temporary, &self.path)
        })();
        if result.is_err() { let _ = std::fs::remove_file(&temporary); }
        result
    }
}
impl Drop for AgentRegistration {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); }
}
use std::os::unix::fs::DirBuilderExt;

impl WaylandClientState {
    pub(super) fn agent_primary_busy(&self) -> bool {
        self.mouse_focused_window.is_some() || self.keyboard_focused_window.is_some()
    }
    pub(super) fn agent_publish(&self) {
        if let Some(registration) = &self.agent_registration {
            if let Err(error) = registration.publish(self.agent_primary_busy()) {
                // A stale ready record must never survive a failed busy update.
                let _ = std::fs::remove_file(&registration.path);
                log::error!("cannot publish GPUI input compatibility: {error}");
            }
        }
    }
    pub(super) fn agent_refresh(&mut self, qh: &QueueHandle<WaylandClientStatePtr>) {
        if self.agent_registration.is_none() { return; }
        let eligible = self.noches_seats.agents();
        self.agent_seats.retain(|id, _| eligible.iter().any(|(live, _, _)| id == live));
        for (id, seat, bits) in eligible {
            let agent = self.agent_seats.entry(id).or_default();
            let caps = wl_seat::Capability::from_bits_truncate(bits);
            if caps.contains(wl_seat::Capability::Pointer) {
                if agent.pointer.is_none() { agent.pointer = Some(seat.get_pointer(qh, AgentDevice(id))); }
            } else if let Some(pointer) = agent.pointer.take() {
                pointer.release(); agent.pointer_surface = None; agent.button = None;
                agent.scroll = Point::default();
            }
            if caps.contains(wl_seat::Capability::Keyboard) {
                if agent.keyboard.is_none() { agent.keyboard = Some(seat.get_keyboard(qh, AgentDevice(id))); }
            } else if let Some(keyboard) = agent.keyboard.take() {
                keyboard.release(); agent.keyboard_surface = None; agent.keymap = None;
                agent.modifiers = Modifiers::default(); agent.capslock = Capslock { on: false };
            }
        }
    }
    fn agent_target(&self, id: u32, surface: &ObjectId) -> Option<WaylandWindowStatePtr> {
        if self.agent_primary_busy() || !self.agent_windows.contains(surface) { return None; }
        // GPUI's logical drag state is app-wide, so two lanes cannot overlap.
        if self.agent_seats.iter().any(|(other, agent)| *other != id
            && (agent.pointer_surface.is_some() || agent.keyboard_surface.is_some())) { return None; }
        self.windows.get(surface).cloned()
    }
}
impl WaylandClientStatePtr {
    pub(crate) fn agent_input_active(&self) -> bool {
        self.get_client().borrow().agent_dispatch.is_some()
    }
    pub(super) fn agent_cancel_all(&self) {
        self.agent_cancel(None);
    }
    pub(super) fn agent_cancel(&self, selected: Option<u32>) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        let mut effects = Vec::new();
        let windows = state.windows.clone();
        for (id, agent) in &mut state.agent_seats {
            if selected.is_some_and(|selected| selected != *id) { continue; }
            if let Some(surface) = agent.pointer_surface.take() {
                if let Some(window) = windows.get(&surface) {
                    let context = AgentDispatch { surface, position: agent.position,
                        modifiers: Modifiers::default(), capslock: Capslock { on: false } };
                    // GPUI's FileDrop::Exited clears its app-wide drag without
                    // executing a drop at the last pointer position.
                    effects.push((window.clone(), context.clone(), PlatformInput::FileDrop(FileDropEvent::Exited)));
                    effects.push((window.clone(), context, PlatformInput::MouseExited(MouseExitEvent {
                        position: agent.position, pressed_button: None, modifiers: Modifiers::default() })));
                }
            }
            if let Some(surface) = agent.keyboard_surface.take() {
                if let Some(window) = windows.get(&surface) {
                    let context = AgentDispatch { surface, position: agent.position,
                        modifiers: Modifiers::default(), capslock: Capslock { on: false } };
                    for (_, keystroke) in agent.held_keys.drain(..) {
                        effects.push((window.clone(), context.clone(), PlatformInput::KeyUp(KeyUpEvent { keystroke })));
                    }
                    effects.push((window.clone(), context, PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                        modifiers: Modifiers::default(), capslock: Capslock { on: false } })));
                }
            }
            agent.button = None; agent.scroll = Point::default(); agent.held_keys.clear();
            agent.modifiers = Modifiers::default(); agent.capslock = Capslock { on: false };
        }
        drop(state);
        for (window, context, input) in effects { self.agent_deliver(window, context, input); }
    }
    fn agent_deliver(&self, target: WaylandWindowStatePtr, context: AgentDispatch, input: PlatformInput) {
        let _origin = crate::InputOriginGuard::enter(crate::InputOrigin::Synthetic);
        self.get_client().borrow_mut().agent_dispatch = Some(context);
        let _guard = DispatchGuard(self.clone());
        target.handle_input(input);
    }
}

impl Dispatch<wl_pointer::WlPointer, AgentDevice> for WaylandClientStatePtr {
    fn event(this: &mut Self, pointer: &wl_pointer::WlPointer, event: wl_pointer::Event,
        device: &AgentDevice, _: &Connection, _: &QueueHandle<Self>) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let id = device.0;
        let Some(agent) = state.agent_seats.get(&id) else { return; };
        if agent.pointer.as_ref() != Some(pointer) { return; }
        if matches!(&event, wl_pointer::Event::Leave { .. }) {
            drop(state);
            this.agent_cancel(Some(id));
            return;
        }
        let surface = match &event {
            wl_pointer::Event::Enter { surface, .. } => Some(surface.id()),
            _ => agent.pointer_surface.clone(),
        };
        let target = surface.as_ref().and_then(|surface| state.agent_target(id, surface));
        if std::env::var_os("NOCHES_AGENT_INPUT_TRACE").is_some() {
            eprintln!("agent pointer seat={id} event={event:?} target={} busy={}", target.is_some(), state.agent_primary_busy());
        }
        let agent = state.agent_seats.get_mut(&id).unwrap();
        // Leaves must clear held state even when the physical user took over.
        if target.is_none() {
            agent.pointer_surface = None; agent.button = None; agent.scroll = Point::default();
            return;
        }
        let input = match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                agent.pointer_surface = Some(surface.id());
                agent.position = point(px(surface_x as f32), px(surface_y as f32));
                agent.button = None;
                Some(PlatformInput::MouseMove(MouseMoveEvent { position: agent.position,
                    pressed_button: None, modifiers: agent.modifiers }))
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                agent.position = point(px(surface_x as f32), px(surface_y as f32));
                Some(PlatformInput::MouseMove(MouseMoveEvent { position: agent.position,
                    pressed_button: agent.button, modifiers: agent.modifiers }))
            }
            wl_pointer::Event::Button { button, state: WEnum::Value(button_state), .. } => {
                let Some(button) = linux_button_to_gpui(button) else { return; };
                match button_state {
                    wl_pointer::ButtonState::Pressed => {
                        if agent.click.last_click.elapsed() < DOUBLE_CLICK_INTERVAL
                            && agent.click.last_mouse_button == Some(button)
                            && is_within_click_distance(agent.click.last_location, agent.position) {
                            agent.click.current_count += 1;
                        } else { agent.click.current_count = 1; }
                        agent.click.last_click = Instant::now(); agent.click.last_mouse_button = Some(button);
                        agent.click.last_location = agent.position; agent.button = Some(button);
                        Some(PlatformInput::MouseDown(MouseDownEvent { button, position: agent.position,
                            modifiers: agent.modifiers, click_count: agent.click.current_count, first_mouse: false }))
                    }
                    wl_pointer::ButtonState::Released => {
                        if agent.button.take() != Some(button) { return; }
                        Some(PlatformInput::MouseUp(MouseUpEvent { button, position: agent.position,
                            modifiers: agent.modifiers, click_count: agent.click.current_count }))
                    }
                    _ => None,
                }
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(axis), value, .. } => {
                // Plugin v3 sends continuous axis values in surface units.
                match axis { wl_pointer::Axis::VerticalScroll => agent.scroll.y -= value as f32,
                    wl_pointer::Axis::HorizontalScroll => agent.scroll.x -= value as f32, _ => {} }
                None
            }
            wl_pointer::Event::Frame => {
                let delta = std::mem::take(&mut agent.scroll);
                (delta != Point::default()).then(|| PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: agent.position, delta: ScrollDelta::Pixels(point(px(delta.x), px(delta.y))),
                    modifiers: agent.modifiers, touch_phase: TouchPhase::Moved }))
            }
            wl_pointer::Event::Leave { .. } => {
                agent.pointer_surface = None; agent.button = None; agent.scroll = Point::default();
                Some(PlatformInput::MouseExited(MouseExitEvent { position: agent.position,
                    pressed_button: None, modifiers: agent.modifiers }))
            }
            _ => None,
        };
        let context = AgentDispatch { surface: surface.unwrap(), position: agent.position,
            modifiers: agent.modifiers, capslock: agent.capslock };
        drop(state);
        if let Some(input) = input { this.agent_deliver(target.unwrap(), context, input); }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, AgentDevice> for WaylandClientStatePtr {
    fn event(this: &mut Self, keyboard: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event,
        device: &AgentDevice, _: &Connection, _: &QueueHandle<Self>) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let id = device.0;
        let Some(agent) = state.agent_seats.get(&id) else { return; };
        if agent.keyboard.as_ref() != Some(keyboard) { return; }
        if matches!(&event, wl_keyboard::Event::Leave { .. }) {
            drop(state);
            this.agent_cancel(Some(id));
            return;
        }
        let surface = match &event {
            wl_keyboard::Event::Enter { surface, .. } => Some(surface.id()),
            _ => agent.keyboard_surface.clone(),
        };
        let target = surface.as_ref().and_then(|surface| state.agent_target(id, surface));
        if std::env::var_os("NOCHES_AGENT_INPUT_TRACE").is_some() {
            eprintln!("agent keyboard seat={id} event={:?} target={} busy={}", std::mem::discriminant(&event), target.is_some(), state.agent_primary_busy());
        }
        let agent = state.agent_seats.get_mut(&id).unwrap();
        let input = match event {
            wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size, .. } => {
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                agent.keymap = unsafe { xkb::Keymap::new_from_fd(&context, fd, size as usize,
                    XKB_KEYMAP_FORMAT_TEXT_V1, KEYMAP_COMPILE_NO_FLAGS) }.ok().flatten().map(|keymap| xkb::State::new(&keymap));
                None
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                agent.keyboard_surface = target.as_ref().map(|_| surface.id()); None
            }
            wl_keyboard::Event::Leave { .. } => {
                agent.keyboard_surface = None; agent.modifiers = Modifiers::default();
                agent.capslock = Capslock { on: false }; None
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                let Some(keymap) = agent.keymap.as_mut() else { return; };
                keymap.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                agent.modifiers = modifiers_from_xkb(keymap); agent.capslock = capslock_from_xkb(keymap);
                Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent { modifiers: agent.modifiers, capslock: agent.capslock }))
            }
            wl_keyboard::Event::Key { key, state: WEnum::Value(key_state), .. } if target.is_some() => {
                let Some(keymap) = agent.keymap.as_ref() else { return; };
                let keycode = Keycode::from(key + MIN_KEYCODE);
                if keymap.key_get_one_sym(keycode).is_modifier_key() { return; }
                let keystroke = keystroke_from_xkb(keymap, agent.modifiers, keycode);
                match key_state {
                    wl_keyboard::KeyState::Pressed => {
                        if !agent.held_keys.iter().any(|(held, _)| *held == key) {
                            agent.held_keys.push((key, keystroke.clone()));
                        }
                        Some(PlatformInput::KeyDown(KeyDownEvent { keystroke,
                            is_held: false, prefer_character_input: false }))
                    }
                    wl_keyboard::KeyState::Released => {
                        agent.held_keys.retain(|(held, _)| *held != key);
                        Some(PlatformInput::KeyUp(KeyUpEvent { keystroke }))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let context = surface.map(|surface| AgentDispatch { surface, position: agent.position,
            modifiers: agent.modifiers, capslock: agent.capslock });
        drop(state);
        if let (Some(target), Some(context), Some(input)) = (target, context, input) { this.agent_deliver(target, context, input); }
    }
}
