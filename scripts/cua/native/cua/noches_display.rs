//! Output-local native pixels for capture/input, global logical pixels for overlays.
//! Names and geometry must come from the same live Wayland connection.
use super::*;
use serde::Serialize;
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Display {
    pub display_id: String,
    pub registry_id: u32,
    pub width: u32,
    pub height: u32,
    pub logical_x: i32,
    pub logical_y: i32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub transform: u32,
}
impl Display {
    pub fn point(&self, x: f64, y: f64) -> anyhow::Result<(u32, u32)> {
        anyhow::ensure!(self.transform == 0, "rotated output requires window-scoped input; desktop transform is not yet supported");
        anyhow::ensure!(x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 && x < self.width as f64 && y < self.height as f64,
            "point outside display {} ({}x{})", self.display_id, self.width, self.height);
        Ok((x.floor() as u32, y.floor() as u32))
    }
    pub fn logical_point(&self, x: f64, y: f64) -> anyhow::Result<(f64, f64)> {
        let (x, y) = self.point(x, y)?;
        anyhow::ensure!(self.width > 0 && self.height > 0 && self.logical_width > 0 && self.logical_height > 0, "incomplete output geometry");
        Ok((self.logical_x as f64 + x as f64 * self.logical_width as f64 / self.width as f64,
            self.logical_y as f64 + y as f64 * self.logical_height as f64 / self.height as f64))
    }
    pub fn layout_token(&self) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(serde_json::to_vec(self).expect("Display serializes")))
    }
}
struct Output {
    wl: WlOutput,
    xdg: Option<ZxdgOutputV1>,
    name: Option<String>,
    mode: Option<(u32, u32)>,
    origin: Option<(i32, i32)>,
    size: Option<(u32, u32)>,
    transform: u32,
}
#[derive(Default)]
pub(super) struct Outputs {
    entries: std::collections::BTreeMap<u32, Output>,
    manager: Option<ZxdgOutputManagerV1>,
}
impl Outputs {
    fn attach(&mut self, qh: &QueueHandle<State>) {
        if let Some(manager) = &self.manager {
            for (id, out) in &mut self.entries {
                if out.xdg.is_none() { out.xdg = Some(manager.get_xdg_output(&out.wl, qh, *id)); }
            }
        }
    }
    pub(super) fn global(&mut self, registry: &wl_registry::WlRegistry, id: u32, interface: &str, version: u32, qh: &QueueHandle<State>) {
        if interface == "zxdg_output_manager_v1" {
            self.manager = Some(registry.bind::<ZxdgOutputManagerV1, _, _>(id, version.min(3), qh, ()));
        } else if interface == "wl_output" {
            self.entries.insert(id, Output { wl: registry.bind::<WlOutput, _, _>(id, version.min(4), qh, id),
                xdg: None, name: None, mode: None, origin: None, size: None, transform: 0 });
        }
        self.attach(qh);
    }
    pub(super) fn remove(&mut self, id: u32) {
        if let Some(out) = self.entries.remove(&id) {
            if let Some(xdg) = out.xdg { xdg.destroy(); }
            if out.wl.version() >= 3 { out.wl.release(); }
        }
    }
    fn list(&self) -> Vec<Display> {
        self.entries.iter().filter_map(|(id, out)| {
            let (width, height) = out.mode?;
            let (logical_x, logical_y) = out.origin?;
            let (logical_width, logical_height) = out.size?;
            if width == 0 || height == 0 || logical_width == 0 || logical_height == 0 { return None; }
            Some(Display { display_id: out.name.clone()?, registry_id: *id, width, height, logical_x, logical_y,
                logical_width, logical_height, transform: out.transform })
        }).collect()
    }
    pub(super) fn resolve(&self, name: &str) -> anyhow::Result<Display> {
        let list = self.list();
        let wanted = if name == "primary" {
            list.iter().find(|d| d.logical_x <= 0 && d.logical_y <= 0 &&
                d.logical_x as i64 + d.logical_width as i64 > 0 && d.logical_y as i64 + d.logical_height as i64 > 0)
                .or_else(|| list.first())
        } else {
            let mut matches = list.iter().filter(|d| d.display_id == name);
            let first = matches.next();
            anyhow::ensure!(matches.next().is_none(), "ambiguous output name");
            first
        };
        wanted.cloned().ok_or_else(|| anyhow::anyhow!("display {name:?} unavailable or missing xdg-output logical geometry"))
    }
    pub(super) fn verify(&self, display: &Display) -> anyhow::Result<WlOutput> {
        anyhow::ensure!(self.resolve(&display.display_id)? == *display, "display layout changed; observe again before input");
        Ok(self.entries.get(&display.registry_id).ok_or_else(|| anyhow::anyhow!("display removed"))?.wl.clone())
    }
}
impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZxdgOutputManagerV1, _: <ZxdgOutputManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(state: &mut Self, _: &ZxdgOutputV1, event: zxdg_output_v1::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(out) = state.outputs.entries.get_mut(id) else { return };
        match event {
            zxdg_output_v1::Event::Name { name } => out.name = Some(name),
            zxdg_output_v1::Event::LogicalPosition { x, y } => out.origin = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => out.size = Some((width.max(0) as u32, height.max(0) as u32)),
            _ => {}
        }
    }
}
impl Dispatch<WlOutput, u32> for State {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(out) = state.outputs.entries.get_mut(id) else { return };
        match event {
            wl_output::Event::Name { name } => out.name = Some(name),
            wl_output::Event::Mode { flags: WEnum::Value(flags), width, height, .. } if flags.contains(wl_output::Mode::Current) => {
                out.mode = Some((width.max(0) as u32, height.max(0) as u32));
            }
            wl_output::Event::Geometry { transform, .. } => out.transform = match transform { WEnum::Value(t) => t as u32, WEnum::Unknown(t) => t },
            _ => {}
        }
    }
}
fn connection() -> anyhow::Result<(Connection, wayland_client::EventQueue<State>, State)> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    conn.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    for _ in 0..4 { queue.roundtrip(&mut state)?; }
    Ok((conn, queue, state))
}
pub fn displays() -> anyhow::Result<Vec<Display>> {
    let (_conn, _queue, state) = connection()?;
    Ok(state.outputs.list())
}
pub fn resolve(name: &str) -> anyhow::Result<Display> {
    let (_conn, _queue, state) = connection()?;
    state.outputs.resolve(name)
}
pub fn capture(display: &Display) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(display.transform == 0, "rotated desktop capture not supported; use window-scoped capture");
    super::capture_via_screencopy_selected(Some(display))
}

#[derive(Clone, Debug)]
pub enum Action {
    Move,
    Click { button: u32, count: u32 },
    Scroll { direction: String, amount: u32 },
    Drag { end_x: f64, end_y: f64, duration_ms: u32 },
}
struct PointerSession {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    held: Option<u32>,
}
impl Drop for PointerSession {
    fn drop(&mut self) {
        if let Some(button) = self.held.take() {
            self.pointer.button(event_time_ms(), button, ButtonState::Released);
            self.pointer.frame();
        }
        self.pointer.destroy();
        let _ = self.conn.flush();
    }
}
impl PointerSession {
    fn verify(&mut self, display: &Display) -> anyhow::Result<()> {
        self.queue.roundtrip(&mut self.state)?;
        self.state.outputs.verify(display)?;
        Ok(())
    }
    fn motion(&mut self, display: &Display, x: f64, y: f64) -> anyhow::Result<()> {
        let (x, y) = display.point(x, y)?;
        self.verify(display)?;
        self.pointer.motion_absolute(event_time_ms(), x, y, display.width, display.height);
        self.pointer.frame();
        self.verify(display)?;
        let (gx, gy) = display.logical_point(x as f64, y as f64)?;
        record_synth_cursor(gx.round() as i32, gy.round() as i32);
        Ok(())
    }
    fn button(&mut self, display: &Display, button: u32, down: bool) -> anyhow::Result<()> {
        self.verify(display)?;
        if down { self.held = Some(button); }
        self.pointer.button(event_time_ms(), button, if down { ButtonState::Pressed } else { ButtonState::Released });
        self.pointer.frame();
        if !down { self.held = None; }
        self.verify(display)
    }
}
pub fn perform(display: &Display, x: f64, y: f64, action: Action) -> anyhow::Result<()> {
    display.point(x, y)?;
    if let Action::Drag { end_x, end_y, duration_ms } = &action {
        display.point(*end_x, *end_y)?;
        anyhow::ensure!((1..=5000).contains(duration_ms), "drag duration must be 1..5000ms");
    }
    let (conn, queue, state) = connection()?;
    let output = state.outputs.verify(display)?;
    let manager = state.vptr_manager.as_ref().ok_or_else(|| anyhow::anyhow!("no native output-bound pointer support"))?;
    anyhow::ensure!(manager.version() >= 2, "virtual-pointer v2 is required for output-bound input");
    let seat = state.seats.selected().ok_or_else(|| anyhow::anyhow!("no ordinary input seat"))?;
    let pointer = manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &queue.handle(), ());
    let mut session = PointerSession { conn, queue, state, pointer, held: None };
    session.motion(display, x, y)?;
    match action {
        Action::Move => {}
        Action::Click { button, count } => {
            anyhow::ensure!((1..=3).contains(&count) && [272, 273, 274].contains(&button), "invalid click");
            for i in 0..count {
                if i > 0 { std::thread::sleep(std::time::Duration::from_millis(80)); }
                session.button(display, button, true)?;
                session.button(display, button, false)?;
            }
        }
        Action::Scroll { direction, amount } => {
            anyhow::ensure!((1..=50).contains(&amount), "scroll amount must be 1..50");
            let (axis, sign) = match direction.as_str() {
                "up" => (Axis::VerticalScroll, -1), "down" => (Axis::VerticalScroll, 1),
                "left" => (Axis::HorizontalScroll, -1), "right" => (Axis::HorizontalScroll, 1),
                _ => anyhow::bail!("unknown scroll direction"),
            };
            for _ in 0..amount {
                session.verify(display)?;
                session.pointer.axis_source(AxisSource::Wheel);
                session.pointer.axis_discrete(event_time_ms(), axis, sign as f64 * 10.0, sign);
                session.pointer.frame();
                session.verify(display)?;
            }
        }
        Action::Drag { end_x, end_y, duration_ms } => {
            session.button(display, BTN_LEFT, true)?;
            let steps = (duration_ms / 16).max(1);
            for step in 1..=steps {
                std::thread::sleep(std::time::Duration::from_millis((duration_ms / steps) as u64));
                let t = step as f64 / steps as f64;
                session.motion(display, x + (end_x - x) * t, y + (end_y - y) * t)?;
            }
            session.button(display, BTN_LEFT, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod noches_display_tests {
    use super::*;
    fn right() -> Display { Display { display_id: "DP-1".into(), registry_id: 42, width: 3840, height: 2160,
        logical_x: 2304, logical_y: 0, logical_width: 2560, logical_height: 1440, transform: 0 } }
    #[test] fn right_monitor_offset_and_scale() {
        assert_eq!(right().logical_point(1920.0, 1080.0).unwrap(), (3584.0, 720.0));
    }
    #[test] fn fractional_size_comes_from_compositor_not_integer_scale() {
        let mut d = right(); d.display_id = "DP-2".into(); d.logical_x = 0; d.logical_width = 2304; d.logical_height = 1296;
        assert_eq!(d.logical_point(1920.0, 1080.0).unwrap(), (1152.0, 648.0));
    }
    #[test] fn corner_zero_is_not_center() { assert_eq!(right().point(0.0, 0.0).unwrap(), (0, 0)); }
    #[test] fn outside_and_nonfinite_points_are_rejected() {
        for p in [(-1.0,0.0),(3840.0,0.0),(0.0,2160.0),(f64::NAN,0.0),(0.0,f64::INFINITY)] { assert!(right().point(p.0,p.1).is_err()); }
    }
    #[test] fn negative_origins_work() { let mut d=right(); d.logical_x=-2560; assert_eq!(d.logical_point(1920.0,1080.0).unwrap(),(-1280.0,720.0)); }
    #[test] fn changed_layout_invalidates_token() { let d=right(); let mut next=d.clone(); next.logical_x=0; assert_ne!(d.layout_token(),next.layout_token()); }
    #[test] fn rotation_is_refused_instead_of_misrouted() { let mut d=right(); d.transform=1; assert!(d.point(0.0,0.0).is_err()); }
}
