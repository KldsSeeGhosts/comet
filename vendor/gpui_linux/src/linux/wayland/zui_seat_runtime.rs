// Included by GPUI's Wayland client, after its normal definitions.
#[path = "seat_selection.rs"]
mod noches_seat_selection;

#[derive(Default)]
struct NochesSeatProbe {
    names: std::collections::BTreeMap<u32, String>,
}
impl Dispatch<wl_seat::WlSeat, u32> for NochesSeatProbe {
    fn event(state: &mut Self, _: &wl_seat::WlSeat, event: wl_seat::Event,
             id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_seat::Event::Name { name } = event { state.names.insert(*id, name); }
    }
}
fn noches_seat_names(conn: &Connection, globals: &GlobalList) -> std::collections::BTreeMap<u32, String> {
    // Use a separate queue before WaylandClientState exists. No physical input
    // object is acquired until all initial seat names have arrived.
    let mut queue = conn.new_event_queue::<NochesSeatProbe>();
    let qh = queue.handle();
    let mut probe = NochesSeatProbe::default();
    let mut handles = Vec::new();
    globals.contents().with_list(|list| {
        for global in list.iter().filter(|global| global.interface == "wl_seat") {
            handles.push(globals.registry().bind::<wl_seat::WlSeat, _, _>(
                global.name, wl_seat_version(global.version), &qh, global.name));
        }
    });
    queue.roundtrip(&mut probe).expect("Wayland seat-name discovery failed");
    for seat in handles { seat.release(); }
    probe.names
}

impl WaylandClientState {
    fn noches_drop_pointer(&mut self) {
        if let Some(gesture) = self.pinch_gesture.take() { gesture.destroy(); }
        if let Some(cursor) = self.cursor_shape_device.take() { cursor.destroy(); }
        if let Some(pointer) = self.wl_pointer.take() { pointer.release(); }
        self.mouse_focused_window = None;
        self.mouse_location = None;
        self.cursor_hidden_window = None;
        self.button_pressed = None;
        self.continuous_scroll_delta = None;
        self.discrete_scroll_delta = None;
        self.scroll_event_received = false;
        self.pinch_scale = 1.0;
    }
    fn noches_drop_keyboard(&mut self) {
        if let Some(input) = self.text_input.take() { input.destroy(); }
        if let Some(keyboard) = self.wl_keyboard.take() { keyboard.release(); }
        self.repeat.current_id = self.repeat.current_id.wrapping_add(1);
        self.repeat.current_keycode = None;
        self.keyboard_focused_window = None;
        self.keymap_state = None;
        self.compose_state = None;
        self.pre_edit_text = None;
        self.ime_pre_edit = None;
        self.composing = false;
        self.last_ime_cursor_rectangle = None;
        self.enter_token = None;
        self.modifiers = Modifiers::default();
        self.capslock = Capslock { on: false };
    }
    fn noches_drop_seat_devices(&mut self) {
        self.noches_drop_pointer();
        self.noches_drop_keyboard();
        if let Some(device) = self.data_device.take() { device.release(); }
        if let Some(device) = self.primary_selection.take() { device.destroy(); }
        self.data_offers.clear();
        self.primary_data_offer = None;
        self.drag.data_offer = None;
        self.drag.window = None;
        self.serial_tracker = SerialTracker::new();
    }
    fn noches_refresh_seat(&mut self, qh: &QueueHandle<WaylandClientStatePtr>) {
        let Some((seat, bits)) = self.noches_seats.current() else {
            self.noches_drop_seat_devices();
            // Retain the old wl_seat resource, with no input children, until a
            // replacement arrives. Global removal does not destroy resources.
            return;
        };
        if seat != self.wl_seat {
            self.noches_drop_seat_devices();
            if !self.noches_seats.contains(&self.wl_seat) { self.wl_seat.release(); }
            self.wl_seat = seat.clone();
            self.globals.seat = seat.clone();
        }
        if self.data_device.is_none() {
            self.data_device = self.globals.data_device_manager.as_ref()
                .map(|manager| manager.get_data_device(&seat, qh, ()));
        }
        if self.primary_selection.is_none() {
            self.primary_selection = self.globals.primary_selection_manager.as_ref()
                .map(|manager| manager.get_device(&seat, qh, ()));
        }
        let capabilities = wl_seat::Capability::from_bits_truncate(bits);
        if capabilities.contains(wl_seat::Capability::Keyboard) {
            if self.wl_keyboard.is_none() {
                self.wl_keyboard = Some(seat.get_keyboard(qh, ()));
                self.text_input = self.globals.text_input_manager.as_ref()
                    .map(|manager| manager.get_text_input(&seat, qh, ()));
            }
        } else if self.wl_keyboard.is_some() {
            self.noches_drop_keyboard();
        }
        if capabilities.contains(wl_seat::Capability::Pointer) {
            if self.wl_pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                self.cursor_shape_device = self.globals.cursor_shape_manager.as_ref()
                    .map(|manager| manager.get_pointer(&pointer, qh, ()));
                self.pinch_gesture = self.globals.gesture_manager.as_ref()
                    .map(|manager| manager.get_pinch_gesture(&pointer, qh, ()));
                self.wl_pointer = Some(pointer);
            }
        } else if self.wl_pointer.is_some() {
            self.noches_drop_pointer();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandClientStatePtr {
    fn event(this: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event,
             _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        match event {
            wl_seat::Event::Name { name } => state.noches_seats.name(seat, name),
            wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } => {
                state.noches_seats.capabilities(seat, capabilities.bits());
            }
            _ => return,
        }
        state.noches_refresh_seat(qh);
    }
}
