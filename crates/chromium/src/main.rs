//! CEF owns Chromium processes; the GPUI process receives only frames and CDP
//! replies over inherited pipes. No TCP debugging port or Node runtime.
mod handlers;
mod input;
#[cfg(target_os = "macos")]
mod macos;
use cef::*;
use handlers::*;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashMap,
    io::Write,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Output {
    frames: HashMap<u32, Vec<u8>>,
    telemetry: std::collections::VecDeque<(u8, u32, Vec<u8>)>,
    events: std::collections::VecDeque<(u8, u32, Vec<u8>)>,
}
#[derive(Clone)]
pub struct Pipe(Arc<Mutex<Output>>);
impl Pipe {
    fn send(&self, kind: u8, id: u32, bytes: Vec<u8>) {
        let mut out = self.0.lock().unwrap();
        if kind == b'F' {
            out.frames.insert(id, bytes);
        } else {
            if kind == b'S' {
                out.events
                    .retain(|(queued_kind, queued_id, _)| *queued_kind != kind || *queued_id != id);
            }
            if out.events.len() >= 256 && kind != b'A' {
                return;
            }
            if out.events.len() >= 256 {
                std::process::exit(1);
            }
            out.events.push_back((kind, id, bytes));
        }
    }
    fn telemetry(&self, id: u32, bytes: Vec<u8>) {
        let mut out = self.0.lock().unwrap();
        if bytes.len() > 256 * 1024 {
            return;
        }
        if out.telemetry.len() >= 128 {
            out.telemetry.pop_front();
        }
        out.telemetry.push_back((b'A', id, bytes));
    }
    fn json(&self, kind: u8, id: u32, value: Value) {
        self.send(kind, id, serde_json::to_vec(&value).unwrap());
    }
}
pub struct PageData {
    id: u32,
    pipe: Pipe,
    width: i32,
    height: i32,
    scale: f32,
    state: Value,
    closed: bool,
    view: Vec<u8>,
    view_width: i32,
    view_height: i32,
    popup: Vec<u8>,
    popup_rect: Rect,
    popup_width: i32,
    popup_height: i32,
}
type Data = Rc<RefCell<PageData>>;
impl PageData {
    fn state(&self) {
        self.pipe.json(b'S', self.id, self.state.clone());
    }
    fn frame(&self) {
        if self.view.is_empty() {
            return;
        }
        let mut pixels = self.view.clone();
        let x = (self.popup_rect.x as f32 * self.scale) as i32;
        let y = (self.popup_rect.y as f32 * self.scale) as i32;
        for row in 0..self.popup_height {
            for col in 0..self.popup_width {
                if x + col < 0
                    || y + row < 0
                    || x + col >= self.view_width
                    || y + row >= self.view_height
                {
                    continue;
                }
                let src = ((row * self.popup_width + col) * 4) as usize;
                let dst = (((y + row) * self.view_width + x + col) * 4) as usize;
                if src + 4 <= self.popup.len() {
                    pixels[dst..dst + 4].copy_from_slice(&self.popup[src..src + 4]);
                }
            }
        }
        // GPUI's RenderImage stores BGRA, like CEF's software paint buffer.
        let mut bytes = Vec::with_capacity(12 + pixels.len());
        bytes.extend_from_slice(&(self.view_width as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.view_height as u32).to_le_bytes());
        bytes.extend_from_slice(&self.scale.to_bits().to_le_bytes());
        bytes.extend(pixels);
        self.pipe.send(b'F', self.id, bytes);
    }
}
struct Page {
    browser: Browser,
    data: Data,
    _devtools: Registration,
}
fn allowed(url: &str) -> bool {
    url == "about:blank"
        || url::Url::parse(url).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Noches Chromium: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let trace = |stage: &str| {
        if std::env::var_os("NOCHES_CHROMIUM_TRACE").is_some() {
            eprintln!("Chromium: {stage}");
        }
    };
    trace("starting");
    let args = cef::args::Args::new();
    #[cfg(target_os = "macos")]
    let subprocess = std::env::args().any(|a| a.starts_with("--type="));
    #[cfg(target_os = "macos")]
    let _sandbox = if subprocess {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        Some(sandbox)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let _loader = {
        let loader = cef::library_loader::LibraryLoader::new(
            &std::env::current_exe().map_err(|e| e.to_string())?,
            subprocess,
        );
        if !loader.load() {
            return Err("Could not load bundled Chromium framework".into());
        }
        loader
    };
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    trace("execute process");
    let mut app = RuntimeApp::new();
    let result = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if result >= 0 {
        std::process::exit(result);
    }
    #[cfg(target_os = "macos")]
    macos::initialize();
    let profile = tempfile::Builder::new()
        .prefix("noches-chromium-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    #[allow(unused_mut)]
    let mut settings = Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        root_cache_path: profile.path().to_string_lossy().as_ref().into(),
        log_severity: LogSeverity::WARNING,
        ..Default::default()
    };
    #[cfg(target_os = "macos")]
    {
        let helper = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .unwrap()
            .join("../Frameworks/Noches Browser Helper.app/Contents/MacOS/Noches Browser Helper")
            .canonicalize()
            .map_err(|e| format!("Browser subprocess is missing: {e}"))?;
        settings.browser_subprocess_path = helper.to_string_lossy().as_ref().into();
    }
    trace("initialize");
    if cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        return Err("CEF initialization failed".into());
    }
    trace("initialized");
    let pipe = Pipe(Arc::new(Mutex::new(Output::default())));
    let writer = pipe.clone();
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        loop {
            let packets = {
                let mut out = writer.0.lock().unwrap();
                let mut packets: Vec<_> = out.events.drain(..).collect();
                packets.extend(out.telemetry.drain(..));
                packets.extend(out.frames.drain().map(|(id, frame)| (b'F', id, frame)));
                packets
            };
            for (kind, id, bytes) in packets {
                let result = (|| -> std::io::Result<()> {
                    stdout.write_all(&[kind])?;
                    stdout.write_all(&id.to_le_bytes())?;
                    stdout.write_all(&(bytes.len() as u32).to_le_bytes())?;
                    stdout.write_all(&bytes)?;
                    stdout.flush()
                })();
                if result.is_err() {
                    std::process::exit(0);
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    });
    let (tx, rx) = mpsc::sync_channel::<Value>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        use std::io::Read;
        loop {
            let mut header = [0u8; 4];
            if stdin.read_exact(&mut header).is_err() {
                break;
            }
            let length = u32::from_le_bytes(header) as usize;
            if length > 1024 * 1024 {
                break;
            }
            let mut bytes = vec![0; length];
            if stdin.read_exact(&mut bytes).is_err() {
                break;
            }
            if let Ok(value) = serde_json::from_slice(&bytes) {
                if tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let mut pages: HashMap<u32, Page> = HashMap::new();
    let mut closing: Vec<Page> = Vec::new();
    let mut context = request_context_create_context(
        Some(&RequestContextSettings::default()),
        None::<&mut RequestContextHandler>,
    );
    let mut quitting = None;
    trace("message loop");
    loop {
        cef::do_message_loop_work();
        for _ in 0..32 {
            match rx.try_recv() {
                Ok(command) => {
                    if command["cmd"] == "shutdown" {
                        quitting.get_or_insert_with(Instant::now);
                    } else {
                        command_page(command, &mut pages, &mut closing, &pipe, context.as_mut());
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    quitting.get_or_insert_with(Instant::now);
                    break;
                }
            }
        }
        if quitting.is_some() {
            for (_, page) in pages.drain() {
                if let Some(host) = page.browser.host() {
                    host.close_browser(1);
                }
                closing.push(page);
            }
        }
        closing.retain(|page| !page.data.borrow().closed);
        if let Some(start) = quitting {
            if closing.is_empty() {
                break;
            }
            if start.elapsed() > Duration::from_secs(4) {
                std::process::exit(0);
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    trace("closing context");
    drop(context);
    trace("shutdown");
    cef::shutdown();
    trace("shutdown complete");
    Ok(())
}
fn command_page(
    command: Value,
    pages: &mut HashMap<u32, Page>,
    closing: &mut Vec<Page>,
    pipe: &Pipe,
    context: Option<&mut RequestContext>,
) {
    let id = command["id"].as_u64().unwrap_or(0) as u32;
    let cmd = command["cmd"].as_str().unwrap_or("");
    if cmd == "create" {
        if pages.contains_key(&id) || pages.len() >= 64 {
            return;
        }
        let data = Rc::new(RefCell::new(PageData {
            id,
            pipe: pipe.clone(),
            width: 1000,
            height: 700,
            scale: 1.,
            state: json!({"url":null,"title":"","loading":false,"can_back":false,"can_forward":false,"error":null}),
            closed: false,
            view: Vec::new(),
            view_width: 0,
            view_height: 0,
            popup: Vec::new(),
            popup_rect: Rect::default(),
            popup_width: 0,
            popup_height: 0,
        }));
        let browser = browser_host_create_browser_sync(
            Some(&WindowInfo {
                windowless_rendering_enabled: 1,
                ..Default::default()
            }),
            Some(&mut PageClient::new(data.clone())),
            Some(&"about:blank".into()),
            Some(&BrowserSettings {
                windowless_frame_rate: 30,
                ..Default::default()
            }),
            None,
            context,
        );
        if let Some(browser) = browser {
            if let Some(host) = browser.host() {
                if let Some(registration) =
                    host.add_dev_tools_message_observer(Some(&mut DevTools::new(data.clone())))
                {
                    pages.insert(
                        id,
                        Page {
                            browser,
                            data,
                            _devtools: registration,
                        },
                    );
                }
            }
        } else {
            let mut d = data.borrow_mut();
            d.state["error"] = "Could not create Chromium page".into();
            d.state();
        }
        return;
    }
    if cmd == "close" {
        if let Some(page) = pages.remove(&id) {
            if let Some(host) = page.browser.host() {
                host.close_browser(1);
            }
            closing.push(page);
        }
        return;
    }
    let Some(page) = pages.get_mut(&id) else {
        return;
    };
    let Some(host) = page.browser.host() else {
        return;
    };
    match cmd {
        "load" => {
            let url = command["url"].as_str().unwrap_or("");
            if allowed(url) {
                let mut d = page.data.borrow_mut();
                d.state["error"] = Value::Null;
                d.state["loading"] = true.into();
                d.state["url"] = url.into();
                d.state();
                drop(d);
                if let Some(frame) = page.browser.main_frame() {
                    frame.load_url(Some(&url.into()));
                }
            }
        }
        "reload" => page.browser.reload(),
        "back" => page.browser.go_back(),
        "forward" => page.browser.go_forward(),
        "visible" => host.was_hidden(i32::from(command["value"].as_u64().unwrap_or(0) == 0)),
        "resize" => {
            let mut d = page.data.borrow_mut();
            d.scale = (command["scale"].as_f64().unwrap_or(1.) as f32).clamp(0.5, 4.);
            d.width = (command["width"].as_f64().unwrap_or(1000.) as f32 / d.scale)
                .round()
                .clamp(1., 4096.) as i32;
            d.height = (command["height"].as_f64().unwrap_or(700.) as f32 / d.scale)
                .round()
                .clamp(1., 4096.) as i32;
            drop(d);
            host.notify_screen_info_changed();
            host.was_resized();
        }
        "cdp" => {
            let value = &command["message"];
            let bytes = serde_json::to_vec(value).unwrap();
            if host.send_dev_tools_message(Some(&bytes)) == 0 {
                pipe.json(
                    b'A',
                    id,
                    json!({"id":value["id"],"error":{"message":"Chromium rejected the command"}}),
                );
            }
        }
        _ => input::dispatch(cmd, &command, &page.browser, &host),
    }
}
