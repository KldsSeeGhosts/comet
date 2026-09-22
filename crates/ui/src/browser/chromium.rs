//! CEF renders offscreen in an isolated process on macOS and Linux.
//! GPUI owns clipping, overlays, focus and native text input.
use super::model::{PageState, Presentation};
use gpui::{Bounds, Pixels, RenderImage};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU32, Ordering},
    },
};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug)]
pub enum NativeEvent {
    Changed,
    Frame,
    NewTab(String),
    Clipboard(String),
    Menu(Value),
}

#[derive(Clone, Default)]
pub struct BrowserData(Arc<Mutex<Weak<Worker>>>);

struct Worker {
    child: Arc<Mutex<Child>>,
    commands: super::command_writer::CommandWriter,
    routes: Arc<Mutex<HashMap<u32, Weak<Route>>>>,
    next_id: AtomicU32,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self
            .commands
            .send(b"{\"cmd\":\"shutdown\",\"id\":0}".to_vec());
        let child = self.child.clone();
        // Native close callbacks must finish before CEF shuts down. Never wait
        // for Chromium on GPUI's foreground thread.
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let mut child = child.lock().unwrap();
                if matches!(child.try_wait(), Ok(Some(_))) {
                    // A crashed browser may leave renderer/GPU children behind.
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // This process group was created by us, including its CEF children.
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                    break;
                }
                drop(child);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        });
    }
}
struct Route {
    tx: Sender<NativeEvent>,
    state: Mutex<PageState>,
    frame: Mutex<Option<(Arc<RenderImage>, f32)>>,
    input: Mutex<Value>,
    console: Mutex<std::collections::VecDeque<Value>>,
    network: Mutex<std::collections::VecDeque<Value>>,
    pending: Mutex<HashMap<u64, zeron_browser::ReplySender>>,
    sequence: std::sync::atomic::AtomicU64,
    #[cfg(feature = "browser-fixture")]
    evaluation: Mutex<Option<Value>>,
}

fn helper_path() -> Result<std::path::PathBuf, String> {
    if let Some(path) = std::env::var_os("NOCHES_CHROMIUM_HELPER") {
        let path = std::path::PathBuf::from(path);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err("NOCHES_CHROMIUM_HELPER must name an absolute executable path".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let parent = exe
        .parent()
        .ok_or("Could not locate application directory")?;
    #[cfg(target_os = "macos")]
    let path = parent.join("../Frameworks/Noches Browser.app/Contents/MacOS/noches-chromium");
    #[cfg(target_os = "linux")]
    let path = parent.join("browser/noches-chromium");
    if path.is_file() {
        return Ok(path);
    }
    Err("The bundled Chromium runtime is missing. Reinstall Noches, or build it with scripts/build-chromium.sh and set NOCHES_CHROMIUM_HELPER.".into())
}
impl BrowserData {
    fn worker(&self) -> Result<Arc<Worker>, String> {
        let mut current = self.0.lock().unwrap();
        if let Some(worker) = current.upgrade() {
            if worker
                .child
                .lock()
                .unwrap()
                .try_wait()
                .map_err(|e| e.to_string())?
                .is_none()
            {
                return Ok(worker);
            }
        }
        use std::os::unix::process::CommandExt;
        let mut child = Command::new(helper_path()?)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!("Could not start Chromium: {e}. Check the bundled browser runtime.")
            })?;
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let routes: Arc<Mutex<HashMap<u32, Weak<Route>>>> = Arc::default();
        let reader_routes = routes.clone();
        std::thread::Builder::new().name("browser-frames".into()).spawn(move || {
            let result = (|| -> std::io::Result<()> {
                loop {
                    let mut header = [0u8; 9]; stdout.read_exact(&mut header)?;
                    let id = u32::from_le_bytes(header[1..5].try_into().unwrap());
                    let length = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
                    if length > 8192 * 8192 * 4 + 12 { return Err(std::io::Error::other("Browser packet is too large")); }
                    let mut data = vec![0; length]; stdout.read_exact(&mut data)?;
                    let route = reader_routes.lock().unwrap().get(&id).and_then(Weak::upgrade);
                    let Some(route) = route else { continue; };
                    let event = match header[0] {
                        b'F' => {
                            if data.len() < 12 { continue; }
                            let width = u32::from_le_bytes(data[..4].try_into().unwrap());
                            let height = u32::from_le_bytes(data[4..8].try_into().unwrap());
                            let scale=f32::from_bits(u32::from_le_bytes(data[8..12].try_into().unwrap()));
                            if !scale.is_finite() || !(0.5..=4.).contains(&scale) {continue;}
                            data.drain(..12);
                            let Some(pixels) = image::RgbaImage::from_raw(width, height, data) else { continue; };
                            *route.frame.lock().unwrap() = Some((Arc::new(RenderImage::new([image::Frame::new(pixels)])),scale));
                            NativeEvent::Frame
                        }
                        b'A' => {
                            if let Ok(message) = serde_json::from_slice::<Value>(&data) {
                                if let Some(id) = message["id"].as_u64() {
                                    if let Some(reply) = route.pending.lock().unwrap().remove(&id) {
                                        let result = if message.get("error").is_some() {
                                            Err(message["error"]["message"].as_str().unwrap_or("Chromium command failed").to_owned())
                                        } else { Ok(message["result"].clone()) };
                                        let _ = reply.send(result);
                                    }
                                }
                            }
                            if let Ok(message) = serde_json::from_slice::<Value>(&data) {
                                let method = message["method"].as_str().unwrap_or("");
                                let params = &message["params"];
                                let entry = match method {
                                    "Runtime.consoleAPICalled" => {
                                        let text = params["args"].as_array().into_iter().flatten().map(|v| v.get("value").unwrap_or(&v["description"]).to_string()).collect::<Vec<_>>().join(" ");
                                        Some((&route.console, json!({"type":params["type"],"text":text.chars().take(4000).collect::<String>(),"timestamp":params["timestamp"]})))
                                    }
                                    "Runtime.exceptionThrown" => Some((&route.console, json!({"type":"exception","text":params["exceptionDetails"]["text"]}))),
                                    "Network.requestWillBeSent" => Some((&route.network, json!({"event":"request","requestId":params["requestId"],"url":params["request"]["url"],"method":params["request"]["method"]}))),
                                    "Network.responseReceived" => Some((&route.network, json!({"event":"response","requestId":params["requestId"],"url":params["response"]["url"],"status":params["response"]["status"]}))),
                                    "Network.loadingFailed" => Some((&route.network, json!({"event":"failed","requestId":params["requestId"],"error":params["errorText"]}))),
                                    _ => None,
                                };
                                if let Some((log, entry)) = entry { let mut log=log.lock().unwrap();log.push_back(entry);while log.len()>100 {log.pop_front();} }
                            }
                            continue;
                        }
                        b'I' => {
                            if let Ok(Value::Object(update))=serde_json::from_slice::<Value>(&data) {
                                let mut input=route.input.lock().unwrap();
                                if !input.is_object() {*input=json!({});}
                                for (key,value) in update {input[&key]=value;}
                            }
                            NativeEvent::Frame
                        }
                        b'S' => {
                            let Ok(state) = serde_json::from_slice(&data) else { continue; };
                            *route.state.lock().unwrap() = state; NativeEvent::Changed
                        }
                        b'M' => {let Ok(menu)=serde_json::from_slice(&data) else {continue;};NativeEvent::Menu(menu)}
                        b'C' => NativeEvent::Clipboard(String::from_utf8_lossy(&data).into_owned()),
                        b'N' => NativeEvent::NewTab(String::from_utf8_lossy(&data).into_owned()),
                        #[cfg(feature = "browser-fixture")]
                        b'J' => { *route.evaluation.lock().unwrap() = serde_json::from_slice(&data).ok(); continue; }
                        _ => continue,
                    };
                    // At most one latest frame is retained per page. A busy UI
                    // never accumulates video frames or blocks the engine.
                    match event {
                        NativeEvent::NewTab(_) | NativeEvent::Clipboard(_) | NativeEvent::Menu(_) => { let _ = route.tx.blocking_send(event); }
                        _ => { let _ = route.tx.try_send(event); }
                    }
                }
            })();
            if result.is_err() {
                for route in reader_routes.lock().unwrap().values().filter_map(Weak::upgrade) {
                    let mut state = route.state.lock().unwrap();
                    state.loading = false;
                    state.error = Some("The Chromium browser process stopped. Close and reopen the tab to restart it.".into());
                    drop(state);
                    for (_, reply) in route.pending.lock().unwrap().drain() { let _ = reply.send(Err("The Chromium process stopped".into())); }
                    let _ = route.tx.try_send(NativeEvent::Changed);
                }
            }
        }).map_err(|e| e.to_string())?;
        let child = Arc::new(Mutex::new(child));
        let failed_child = child.clone();
        let failed_routes = routes.clone();
        let commands = super::command_writer::CommandWriter::new(stdin, move || {
            if let Ok(child) = failed_child.lock() {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
            }
            for route in failed_routes
                .lock()
                .unwrap()
                .values()
                .filter_map(Weak::upgrade)
            {
                let mut state = route.state.lock().unwrap();
                state.loading = false;
                state.error =
                    Some("The browser helper stopped accepting input. Reopen the tab.".into());
                drop(state);
                let _ = route.tx.try_send(NativeEvent::Changed);
            }
        })
        .map_err(|error| {
            if let Ok(mut child) = child.lock() {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
            }
            error.to_string()
        })?;
        let worker = Arc::new(Worker {
            child,
            commands,
            routes,
            next_id: AtomicU32::new(1),
        });
        *current = Arc::downgrade(&worker);
        Ok(worker)
    }
}
impl Worker {
    fn send(&self, id: u32, mut command: Value) -> Result<(), String> {
        command["id"] = id.into();
        let data = serde_json::to_vec(&command).map_err(|e| e.to_string())?;
        self.commands.send(data)
    }
}

pub struct NativePage {
    worker: Arc<Worker>,
    route: Arc<Route>,
    id: u32,
    pub bounds: Bounds<Pixels>,
    pub scale: f32,
    geometry: Option<(u32, u32, u32)>,
    presentation: Presentation,
    pub image: Option<Arc<RenderImage>>,
    pub image_scale: f32,
    pub menu: Option<Value>,
    pub menu_active: usize,
    preedit: String,
    pub pressed: std::cell::Cell<Option<gpui::MouseButton>>,
}
impl NativePage {
    pub fn new(
        _: &gpui::Window,
        data: &BrowserData,
        tx: Sender<NativeEvent>,
    ) -> Result<Self, String> {
        let worker = data.worker()?;
        let id = worker.next_id.fetch_add(1, Ordering::Relaxed);
        let route = Arc::new(Route {
            tx,
            state: Mutex::default(),
            frame: Mutex::default(),
            input: Mutex::new(json!({"focused":true})),
            console: Mutex::default(),
            network: Mutex::default(),
            pending: Mutex::default(),
            sequence: std::sync::atomic::AtomicU64::new(1),
            #[cfg(feature = "browser-fixture")]
            evaluation: Mutex::default(),
        });
        worker
            .routes
            .lock()
            .unwrap()
            .insert(id, Arc::downgrade(&route));
        worker.send(id, json!({"cmd":"create"}))?;
        let page = Self {
            worker,
            route,
            id,
            bounds: Bounds::default(),
            scale: 1.,
            geometry: None,
            presentation: Presentation::Live,
            image: None,
            image_scale: 1.,
            menu: None,
            menu_active: 0,
            preedit: String::new(),
            pressed: std::cell::Cell::new(None),
        };
        for (method, params) in [
            ("Runtime.enable", json!({})),
            (
                "Network.enable",
                json!({"maxTotalBufferSize":1048576,"maxResourceBufferSize":65536}),
            ),
        ] {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            page.cdp(method, params, tx);
        }
        Ok(page)
    }
    pub fn load(&self, url: &str) -> Result<(), String> {
        self.worker.send(self.id, json!({"cmd":"load","url":url}))
    }
    pub fn reload(&self) {
        self.command(json!({"cmd":"reload"}));
    }
    pub fn history(&self, forward: bool) {
        self.command(json!({"cmd":if forward {"forward"} else {"back"}}));
    }
    pub fn state(&self) -> PageState {
        self.route.state.lock().unwrap().clone()
    }
    pub fn present(&mut self, presentation: Presentation) {
        if (self.presentation == Presentation::Hidden) != (presentation == Presentation::Hidden) {
            self.command(
                json!({"cmd":"visible","value":u8::from(presentation != Presentation::Hidden)}),
            );
        }
        if presentation == Presentation::Hidden && self.menu.take().is_some() {
            self.command(json!({"cmd":"dismiss-menu"}));
        }
        self.presentation = presentation;
    }
    pub fn update_frame(&mut self, window: &mut gpui::Window) {
        if let Some((frame, scale)) = self.route.frame.lock().unwrap().take() {
            self.image_scale = scale;
            if let Some(old) = self.image.replace(frame) {
                let _ = window.drop_image(old);
            }
        }
    }
    pub fn sync(&mut self, bounds: Bounds<Pixels>, scale: f32) {
        self.bounds = bounds;
        self.scale = scale;
        let geometry = (
            (f32::from(bounds.size.width) * scale)
                .round()
                .clamp(1., 8192.) as u32,
            (f32::from(bounds.size.height) * scale)
                .round()
                .clamp(1., 8192.) as u32,
            scale.to_bits(),
        );
        if self.geometry != Some(geometry) {
            self.command(
                json!({"cmd":"resize","width":geometry.0,"height":geometry.1,"scale":scale}),
            );
            self.geometry = Some(geometry);
        }
    }
    pub fn command(&self, value: Value) {
        let _ = self.worker.send(self.id, value);
    }
    pub fn logs(&self, network: bool) -> Value {
        let log = if network {
            &self.route.network
        } else {
            &self.route.console
        };
        json!({"entries":log.lock().unwrap().iter().cloned().collect::<Vec<_>>()})
    }
    pub fn cdp(&self, method: &str, params: Value, reply: zeron_browser::ReplySender) {
        let id = self.route.sequence.fetch_add(1, Ordering::Relaxed);
        self.route
            .pending
            .lock()
            .unwrap()
            .retain(|_, sender| !sender.is_closed());
        if self.route.pending.lock().unwrap().len() >= 32 {
            let _ = reply.send(Err("Too many pending browser operations".into()));
            return;
        }
        self.route.pending.lock().unwrap().insert(id, reply);
        if let Err(error) = self.worker.send(
            self.id,
            json!({"cmd":"cdp","message":{"id":id,"method":method,"params":params}}),
        ) {
            if let Some(reply) = self.route.pending.lock().unwrap().remove(&id) {
                let _ = reply.send(Err(error));
            }
        }
    }
    #[cfg(feature = "browser-fixture")]
    pub fn evaluate(&self, script: &str) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        self.cdp(
            "Runtime.evaluate",
            json!({"expression":script,"returnByValue":true}),
            tx,
        );
    }
    #[cfg(feature = "browser-fixture")]
    pub fn evaluation(&self) -> Option<Value> {
        None
    }
}
impl Drop for NativePage {
    fn drop(&mut self) {
        self.command(json!({"cmd":"close"}));
        self.worker.routes.lock().unwrap().remove(&self.id);
    }
}

impl super::BrowserSurface {
    #[cfg(feature = "browser-fixture")]
    pub fn fixture_crash_browser(&self) {
        if let Some(native) = &self.native {
            let child = native.worker.child.lock().unwrap();
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
        }
    }
    pub(super) fn on_native_event(
        &mut self,
        event: NativeEvent,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gpui::Focusable;
        let Some(native) = &mut self.native else {
            return;
        };
        native.update_frame(window);
        match event {
            NativeEvent::NewTab(url) => {
                cx.emit(super::BrowserEvent::NewTab(Some(url)));
            }
            NativeEvent::Menu(menu) => {
                if self.presentation == Presentation::Live {
                    native.menu_active = menu["items"]
                        .as_array()
                        .and_then(|items| {
                            items
                                .iter()
                                .position(|item| item["selected"].as_bool().unwrap_or(false))
                        })
                        .unwrap_or(0);
                    native.menu = Some(menu);
                } else {
                    native.command(json!({"cmd":"dismiss-menu"}));
                }
            }
            NativeEvent::Clipboard(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
            NativeEvent::Frame | NativeEvent::Changed => {
                let mut page = native.state();
                if page.url.is_none() {
                    page.url = self.page.url.clone();
                }
                if page.error.is_some() {
                    page.loading = false;
                }
                if !self.address.focus_handle(cx).is_focused(window) {
                    if let Some(url) = &page.url {
                        if self.address.read(cx).text() != url {
                            self.address
                                .update(cx, |input, cx| input.set_text(url.clone(), cx));
                        }
                    }
                    self.address_edited = false;
                }
                if page != self.page {
                    self.page = page;
                    cx.emit(super::BrowserEvent::Changed);
                }
            }
        }
        cx.notify();
    }
    pub(super) fn linux_pointer(
        &self,
        kind: &str,
        position: gpui::Point<Pixels>,
        button: Option<gpui::MouseButton>,
        modifiers: gpui::Modifiers,
    ) {
        let Some(native) = &self.native else {
            return;
        };
        if self.presentation != Presentation::Live {
            return;
        }
        if kind == "down" {
            native.pressed.set(button);
        }
        if kind == "up" && native.pressed.replace(None) != button {
            return;
        }
        let position = position - native.bounds.origin;
        native.command(json!({"cmd":kind,"x":f32::from(position.x),"y":f32::from(position.y),"button":button.map(mouse_button).unwrap_or(0),"mods":modifiers_mask(modifiers) | if kind == "move" { button.map(|b| 1 << (mouse_button(b)+7)).unwrap_or(0) } else { 0 }}));
    }
    pub(super) fn linux_key(&self, stroke: &gpui::Keystroke, down: bool) {
        let Some(native) = &self.native else {
            return;
        };
        let printable = stroke.key_char.as_deref().filter(|s| {
            s.chars().count() == 1
                && !s.chars().any(char::is_control)
                && !stroke.modifiers.control
                && !stroke.modifiers.platform
        });
        let key = match printable.unwrap_or(stroke.key.as_str()) {
            "enter" => "Return",
            "backspace" => "BackSpace",
            "delete" => "Delete",
            "escape" => "Escape",
            "tab" => "Tab",
            "left" => "Left",
            "right" => "Right",
            "up" => "Up",
            "down" => "Down",
            "home" => "Home",
            "end" => "End",
            "pageup" => "Page_Up",
            "pagedown" => "Page_Down",
            "space" => "space",
            key => key,
        };
        native.command(json!({"cmd":if down {"key_down"} else {"key_up"},"key":key,"text":stroke.key_char.as_deref().unwrap_or(""),"mods":modifiers_mask(stroke.modifiers)}));
    }
}
pub(super) fn modifiers_mask(m: gpui::Modifiers) -> u32 {
    u32::from(m.shift)
        | (u32::from(m.control) << 2)
        | (u32::from(m.alt) << 3)
        | (u32::from(m.platform) << 26)
}
fn mouse_button(button: gpui::MouseButton) -> u32 {
    match button {
        gpui::MouseButton::Left => 1,
        gpui::MouseButton::Middle => 2,
        gpui::MouseButton::Right => 3,
        gpui::MouseButton::Navigate(gpui::NavigationDirection::Back) => 8,
        gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward) => 9,
    }
}

#[cfg(feature = "browser-fixture")]
impl super::BrowserSurface {
    pub fn fixture_linux_menu_open(&self) -> bool {
        self.native.as_ref().is_some_and(|n| n.menu.is_some())
    }
    pub fn fixture_linux_bounds(&self) -> Bounds<Pixels> {
        self.native.as_ref().unwrap().bounds
    }
    pub fn fixture_linux_evaluation(&self) -> Option<Value> {
        self.native.as_ref().unwrap().evaluation()
    }
}

impl gpui::EntityInputHandler for super::BrowserSurface {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        actual: &mut Option<std::ops::Range<usize>>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<String> {
        let native = self.native.as_ref()?;
        let input = native.route.input.lock().unwrap();
        let text = input["text"].as_str().unwrap_or("");
        let units: Vec<u16> = text.encode_utf16().collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *actual = Some(range.clone());
        Some(String::from_utf16_lossy(units.get(range)?))
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let native = self.native.as_ref()?;
        let input = native.route.input.lock().unwrap();
        let text = input["text"].as_str().unwrap_or("");
        let offset = |key: &str| {
            let n = input[key].as_u64().unwrap_or(0) as usize;
            text.get(..n).unwrap_or(text).encode_utf16().count()
        };
        let cursor = offset("cursor");
        let selection = offset("selection");
        Some(gpui::UTF16Selection {
            range: cursor.min(selection)..cursor.max(selection),
            reversed: cursor < selection,
        })
    }
    fn marked_text_range(
        &self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        let native = self.native.as_ref()?;
        (!native.preedit.is_empty()).then(|| 0..native.preedit.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) {
        if let Some(native) = &mut self.native {
            native.command(json!({"cmd":"unmark"}));
            native.preedit.clear();
        }
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) {
        if let Some(native) = &mut self.native {
            native.command(json!({"cmd":"commit","text":text}));
            native.preedit.clear();
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) {
        if let Some(native) = &mut self.native {
            native.command(json!({"cmd":"preedit","text":text}));
            native.preedit = text.to_owned();
        }
    }
    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: Bounds<Pixels>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let native = self.native.as_ref()?;
        let input = native.route.input.lock().unwrap();
        let c = &input["caret"];
        Some(Bounds::new(
            native.bounds.origin
                + gpui::point(
                    gpui::px(c[0].as_f64().unwrap_or(0.) as f32 / native.scale),
                    gpui::px(c[1].as_f64().unwrap_or(0.) as f32 / native.scale),
                ),
            gpui::size(
                gpui::px(c[2].as_f64().unwrap_or(1.) as f32 / native.scale),
                gpui::px(c[3].as_f64().unwrap_or(18.) as f32 / native.scale),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<usize> {
        None
    }
    fn accepts_text_input(&self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> bool {
        self.native.as_ref().is_some_and(|n| {
            n.route.input.lock().unwrap()["focused"]
                .as_bool()
                .unwrap_or(false)
        })
    }
}

impl super::BrowserSurface {
    pub(super) fn linux_dismiss_menu(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(native) = &mut self.native {
            native.menu = None;
            native.command(json!({"cmd":"dismiss-menu"}));
        }
        cx.notify();
    }
    pub(super) fn linux_choose_menu(&mut self, index: usize, cx: &mut gpui::Context<Self>) {
        if let Some(native) = &mut self.native {
            if let Some(item) = native
                .menu
                .as_ref()
                .and_then(|m| m["items"].get(index))
                .filter(|i| i["enabled"].as_bool().unwrap_or(false))
            {
                let action = item["action"].as_str().unwrap_or("");
                if action == "text" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        native.command(json!({"cmd":"text","text":text}));
                    }
                } else {
                    native.command(json!({"cmd":action}));
                }
            }
        }
        self.linux_dismiss_menu(cx);
    }
    pub(super) fn linux_menu_key(&mut self, key: &str, cx: &mut gpui::Context<Self>) -> bool {
        let Some(native) = self.native.as_mut().filter(|n| n.menu.is_some()) else {
            return false;
        };
        match key {
            "escape" => self.linux_dismiss_menu(cx),
            "enter" => {
                let index = native.menu_active;
                self.linux_choose_menu(index, cx);
            }
            "up" | "down" => {
                if let Some(items) = native.menu.as_ref().unwrap()["items"]
                    .as_array()
                    .filter(|items| !items.is_empty())
                {
                    for _ in 0..items.len() {
                        native.menu_active = if key == "down" {
                            (native.menu_active + 1) % items.len()
                        } else {
                            (native.menu_active + items.len() - 1) % items.len()
                        };
                        if items[native.menu_active]["enabled"]
                            .as_bool()
                            .unwrap_or(false)
                        {
                            break;
                        }
                    }
                    cx.notify();
                }
            }
            _ => {}
        }
        true
    }
    pub(super) fn linux_menu(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = &theme.for_popup();
        use gpui::{IntoElement, div, prelude::*, px};
        let native = self.native.as_ref()?;
        let menu = native.menu.as_ref()?;
        let items = menu["items"].as_array()?;
        let pos = native.bounds.origin
            + gpui::point(
                px(menu["x"].as_f64().unwrap_or(0.) as f32),
                px(menu["y"].as_f64().unwrap_or(0.) as f32),
            );
        let mut content = crate::popover::popover_card(theme)
            .w(px(240.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.linux_dismiss_menu(cx)));
        let mut rows = div()
            .id("browser-menu-options")
            .flex()
            .flex_col()
            .max_h(px(320.))
            .overflow_y_scroll();
        for (index, item) in items.iter().enumerate() {
            let enabled = item["enabled"].as_bool().unwrap_or(false);
            let row = crate::popover::menu_row(
                theme,
                index == native.menu_active,
                format!("browser-option-{index}"),
            )
            .id(("browser-option", index))
            .child(gpui::SharedString::from(
                item["label"].as_str().unwrap_or("").to_owned(),
            ))
            .when(!enabled, |el| el.opacity(0.4))
            .when(enabled, |el| {
                el.on_click(cx.listener(move |this, _, _, cx| this.linux_choose_menu(index, cx)))
            });
            rows = rows.child(row);
        }
        content = content.child(rows);
        Some(crate::popover::menu_at(
            "browser-page-menu",
            pos,
            content.into_any_element(),
            None,
        ))
    }
}
