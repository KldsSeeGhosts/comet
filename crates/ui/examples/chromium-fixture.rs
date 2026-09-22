// Real GPUI shell and Chromium agent bridge integration fixture.
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};
use zeron_ui::*;

async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

fn capture(directory: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let path = directory.join(format!("{name}.png"));
    #[cfg(target_os = "macos")]
    let status = {
        let app = objc2_app_kit::NSApplication::sharedApplication(
            objc2::MainThreadMarker::new().unwrap(),
        );
        let window = app
            .keyWindow()
            .or_else(|| app.mainWindow())
            .or_else(|| app.windows().iter().find(|window| window.isVisible()))
            .ok_or_else(|| anyhow::anyhow!("fixture window is not available"))?;
        std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-l", &window.windowNumber().to_string()])
            .arg(&path)
            .status()?
    };
    #[cfg(not(target_os = "macos"))]
    let status = {
        if std::env::var_os("NOCHES_FIXTURE_HYPRLAND").is_some() {
            let clients = std::process::Command::new("hyprctl")
                .args(["clients", "-j"])
                .output()?;
            let clients: serde_json::Value = serde_json::from_slice(&clients.stdout)?;
            let client = clients
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["pid"].as_u64() == Some(std::process::id() as u64))
                .ok_or_else(|| anyhow::anyhow!("fixture Wayland window not visible"))?;
            let geometry = format!(
                "{},{} {}x{}",
                client["at"][0], client["at"][1], client["size"][0], client["size"][1]
            );
            anyhow::ensure!(
                std::process::Command::new("grim")
                    .args(["-g", &geometry])
                    .arg(&path)
                    .status()?
                    .success(),
                "Wayland capture failed"
            );
            return Ok(());
        }
        let capture_window = std::env::var("ZERON_BROWSER_CAPTURE_WINDOW").ok();
        let windows = std::process::Command::new("xdotool")
            .args([
                "search",
                "--onlyvisible",
                "--pid",
                &std::process::id().to_string(),
            ])
            .output()?;
        let id = capture_window
            .or_else(|| {
                String::from_utf8(windows.stdout)
                    .ok()?
                    .lines()
                    .next()
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow::anyhow!("fixture window not visible"))?;
        std::process::Command::new("import")
            .args(["-window", &id])
            .arg(&path)
            .status()?
    };
    anyhow::ensure!(status.success(), "screenshot capture failed");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let output = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/tmp/zeron-browser-captures".into()),
    );
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let data = temp.path().to_path_buf();
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let _origin = format!("http://{}", listener.local_addr()?);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut request = [0; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            let (title, html) = if request.starts_with("GET /two ") {
                (
                    "Details",
                    "<a href='/'>Back to overview</a><h1>A closer look.</h1><p>Independent navigation, right beside your work.</p>",
                )
            } else {
                (
                    "Fieldnotes",
                    "<div class='eyebrow'>FIELDNOTES / WORKSPACE</div><h1>Make room<br>for good work.</h1><p>A quieter place to collect ideas, follow your progress, and build something that matters.</p><a class='button' id='details' href='/two'>Explore the workspace →</a><div class='cards'><article><small>01 / COLLECT</small><h2>Keep the good ideas.</h2><p>One place for the things you want to come back to.</p></article><article><small>02 / CREATE</small><h2>Find your next step.</h2><p>Small, thoughtful progress. Every single day.</p></article></div>",
                )
            };
            let body = format!(
                "<!doctype html><meta charset=utf-8><meta name='viewport' content='width=device-width'><title>{title}</title><style>body{{margin:0;padding:42px 32px;background:#f5f2eb;color:#263d35;font:15px/1.6 system-ui}}.eyebrow,small{{font-size:10px;letter-spacing:2px;color:#6d7c70}}h1{{font:500 45px/1.1 Georgia;margin:30px 0 20px}}p{{color:#6d776f;max-width:350px}}a{{color:inherit}}.button{{display:inline-block;margin:14px 0 30px;padding:10px 17px;background:#29483b;color:#fff;border-radius:7px;text-decoration:none;font-size:12px}}.cards{{display:grid;gap:14px}}article{{border:1px solid #d9ddd0;padding:20px;border-radius:10px}}h2{{font:500 21px Georgia;margin:12px 0}}article p{{font-size:12px;margin-bottom:0}}</style>{html}"
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    let failure = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result = failure.clone();
    gpui_platform::application().with_assets(icons::Assets).run(move |cx| {
        gpui_tokio::init(cx); gpui_base::init(cx);
        let settings = settings::UiSettings::default();
        settings::init(settings.clone(), data.clone(), cx);
        let fonts = typography::register_fonts(cx);
        typography::init(settings.ui_font_family.clone(), settings.ui_font_size, settings.terminal_font_family.clone(), settings.terminal_font_size, settings.code_font_family.clone(), settings.code_font_size, fonts, cx);
        theme_library::init(data.clone(), cx);
        appearance::init(appearance::AppearanceMode::Dark, settings.theme_selection, settings.accent, settings.surface, cx);
        history::init(settings.git_history_columns, settings.git_history_column_widths,
            settings.git_history_column_order, settings.git_history_author_display, cx);
        composer::init(cx, settings.composer_send_behavior); terminal::panel::init(cx); app_menus::init(cx);
        let state = cx.new(|_| {
            let mut s = state::AppState::new();
            s.connection = zeron_proto::view::ConnectionStatus::Ready;
            s.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
            s.local_device_id = Some("local".into());
            s.devices = vec![serde_json::from_value(serde_json::json!({"id":"local","name":"This device","platform":std::env::consts::OS,"lastSeenAt":null})).unwrap()];
            s.selected_chat = Some("browser-fixture".into()); s.selected_space = Some("project".into());
            s.auto_selected = true; s.chats_synced = true; s.spaces_synced = true;
            s.spaces = vec![serde_json::from_value(serde_json::json!({"id":"project","deviceId":"local","path":"/tmp/fieldnotes","createdAt":"2026-09-08T00:00:00Z"})).unwrap()];
            s.chats = vec![serde_json::from_value(serde_json::json!({"id":"browser-fixture","deviceId":"local","spaceId":"project","title":"Build the Fieldnotes workspace","archived":false,"createdAt":"2026-09-08T00:00:00Z","config":{"harness":"claude-code","model":"claude-sonnet-4-6","reasoning":null,"sandbox":"workspace-write"}})).unwrap()];
            let mut other = s.chats[0].clone(); other.id = "other-session".into(); s.chats.push(other);
            s
        });
        let socket = zeron_browser::socket_path(&data);
        let boot = EngineBootConfig { remote: None, data_dir: data, ipc_port: 0, edge_url: String::new(), edge_token: None, org_id: None, workos_client_id: None, default_harness: HarnessId::ClaudeCode };
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(gpui::point(px(12.),px(30.)), size(px(1000.),px(680.))))),
            titlebar: Some(gpui::TitlebarOptions { title: None, appears_transparent: true, traffic_light_position: Some(gpui::point(px(14.),px(14.))) }),
            app_owns_titlebar_drag: true,
            ..Default::default()
        }, |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx))).unwrap();
        state.update(cx, |_, cx| cx.notify());
        cx.activate(true);

        cx.spawn(async move |cx| {
            let run: anyhow::Result<()> = async {
                pause(cx, 1000).await;
                let opened=browser_call(&socket, "browser-fixture", zeron_browser::Action::Open {url:_origin.clone()},cx).await?;
                let id=opened["tab"].as_u64().unwrap();
                let deadline=std::time::Instant::now()+Duration::from_secs(20);
                loop {
                    let state=browser_call(&socket,"browser-fixture",zeron_browser::Action::State{tab:id},cx).await?;
                    anyhow::ensure!(state["error"].is_null(),"browser error: {state}");
                    if state["title"]=="Fieldnotes" && state["loading"]==false {break;}
                    anyhow::ensure!(std::time::Instant::now()<deadline,"page load timed out: {state}");pause(cx,100).await;
                }
                pause(cx,800).await;
                let (_,browser)=window.update(cx,|s,_,cx|s.fixture_active_browser(cx).unwrap())?;
                anyhow::ensure!(browser.read_with(cx,|b,_|b.fixture_native_visible()),"GPUI did not receive the Chromium frame");
                capture(&output,"chromium-shared-page")?;
                let snapshot=browser_call(&socket,"browser-fixture",zeron_browser::Action::Snapshot{tab:id},cx).await?;
                anyhow::ensure!(snapshot["text"].as_str().unwrap().contains("Make room"),"DOM snapshot did not match displayed page");
                let reference=snapshot["elements"].as_array().unwrap().iter().find(|e|e["tag"]=="a").unwrap()["reference"].as_str().unwrap().to_string();
                browser_call(&socket,"browser-fixture",zeron_browser::Action::Click{tab:id,reference},cx).await?;
                pause(cx,500).await;
                let snapshot=browser_call(&socket,"browser-fixture",zeron_browser::Action::Snapshot{tab:id},cx).await?;
                anyhow::ensure!(snapshot["title"]=="Details","agent navigation did not reach the visible tab");
                let value=browser_call(&socket,"browser-fixture",zeron_browser::Action::Evaluate{tab:id,expression:"console.log('agent fixture'); document.body.insertAdjacentHTML('beforeend','<input id=keys>'); document.querySelector('#keys').focus(); document.querySelector('#keys').addEventListener('keydown',e=>window.lastKey=e.key); 6*7".into()},cx).await?;
                anyhow::ensure!(value==42,"evaluate returned {value}");
                browser_call(&socket,"browser-fixture",zeron_browser::Action::Press{tab:id,key:"Enter".into()},cx).await?;
                let key=browser_call(&socket,"browser-fixture",zeron_browser::Action::Evaluate{tab:id,expression:"window.lastKey".into()},cx).await?;
                anyhow::ensure!(key=="Enter","key event missing: {key}");
                pause(cx,100).await;
                let console=browser_call(&socket,"browser-fixture",zeron_browser::Action::Console{tab:id},cx).await?;
                anyhow::ensure!(console.to_string().contains("agent fixture"),"console event missing: {console}");
                let network=browser_call(&socket,"browser-fixture",zeron_browser::Action::Network{tab:id},cx).await?;
                anyhow::ensure!(network.to_string().contains("/two"),"network event missing: {network}");
                let background=browser_call(&socket,"other-session",zeron_browser::Action::Open{url:_origin.clone()},cx).await?;
                anyhow::ensure!(background["tab"]!=id,"background tab reused visible id");
                pause(cx,300).await;
                let foreground=browser_call(&socket,"browser-fixture",zeron_browser::Action::State{tab:id},cx).await?;
                anyhow::ensure!(foreground["title"]=="Details","background tab changed foreground");
                let background_id=background["tab"].as_u64().unwrap();
                let deadline=std::time::Instant::now()+Duration::from_secs(15);
                loop {
                    let state=browser_call(&socket,"other-session",zeron_browser::Action::State{tab:background_id},cx).await?;
                    if state["title"]=="Fieldnotes" && state["loading"]==false {break;}
                    anyhow::ensure!(std::time::Instant::now()<deadline,"background load timed out");pause(cx,100).await;
                }
                browser_call(&socket,"other-session",zeron_browser::Action::Evaluate{tab:background_id,expression:format!("window.open({}); true",serde_json::to_string(&format!("{_origin}/two")).unwrap())},cx).await?;
                let deadline=std::time::Instant::now()+Duration::from_secs(5);
                loop {
                    let tabs=browser_call(&socket,"other-session",zeron_browser::Action::Tabs,cx).await?;
                    if tabs["tabs"].as_array().unwrap().len()==2 {break;}
                    anyhow::ensure!(std::time::Instant::now()<deadline,"background popup lost its owner");pause(cx,100).await;
                }
                let wrong=browser_call(&socket,"other-session",zeron_browser::Action::State{tab:id},cx).await;
                anyhow::ensure!(wrong.is_err(),"another session could access the tab");
                window.update(cx,|s,_,cx|s.fixture_browser_menu(true,cx))?;pause(cx,250).await;
                capture(&output,"chromium-menu-overlay")?;
                window.update(cx,|s,_,cx|{s.fixture_browser_menu(false,cx);s.fixture_resize_browser(420.,cx);})?;pause(cx,300).await;
                capture(&output,"chromium-resized")?;
                browser.read_with(cx, |b, _| b.fixture_crash_browser());
                let deadline=std::time::Instant::now()+Duration::from_secs(5);
                loop {
                    let state=browser_call(&socket,"browser-fixture",zeron_browser::Action::State{tab:id},cx).await?;
                    if state["error"].is_string() {break;}
                    anyhow::ensure!(std::time::Instant::now()<deadline,"browser crash was not reported");pause(cx,100).await;
                }
                browser_call(&socket,"browser-fixture",zeron_browser::Action::Close{tab:id},cx).await?;
                let closed=browser_call(&socket,"browser-fixture",zeron_browser::Action::State{tab:id},cx).await;
                anyhow::ensure!(closed.is_err(),"closed tab was still addressable");
                let recovered=browser_call(&socket,"browser-fixture",zeron_browser::Action::Open{url:_origin.clone()},cx).await?;
                let recovered_id=recovered["tab"].as_u64().unwrap();
                let deadline=std::time::Instant::now()+Duration::from_secs(20);
                loop {
                    let state=browser_call(&socket,"browser-fixture",zeron_browser::Action::State{tab:recovered_id},cx).await?;
                    anyhow::ensure!(state["error"].is_null(),"restart failed: {state}");
                    if state["title"]=="Fieldnotes" && state["loading"]==false {break;}
                    anyhow::ensure!(std::time::Instant::now()<deadline,"restart timed out");pause(cx,100).await;
                }
                browser_call(&socket,"browser-fixture",zeron_browser::Action::Close{tab:recovered_id},cx).await?;
                std::fs::write(output.join("result.txt"),"PASS: GPUI Chromium frame, shared agent navigation, session isolation, background tabs and popups, evaluate, keyboard, console, network, menu overlay, resize, close and crash recovery\n")?;
                Ok(())
            }.await;
            if let Err(error)=run { eprintln!("Chromium fixture: {error:#}");*result.lock().unwrap()=Some(error.to_string()); }
            let _ = window.update(cx, |shell, window, cx| shell.fixture_blur_browser(window, cx));
            pause(cx, 200).await;
            drop(state);
            let _=window.update(cx,|_,window,_|window.remove_window());pause(cx,200).await;cx.update(|cx|cx.quit());
        }).detach();
    });
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
async fn browser_call(
    socket: &std::path::Path,
    session: &str,
    action: zeron_browser::Action,
    cx: &mut AsyncApp,
) -> anyhow::Result<serde_json::Value> {
    let socket = socket.to_path_buf();
    let request = zeron_browser::Request {
        session: session.into(),
        action,
    };
    cx.background_executor()
        .spawn(async move { zeron_browser::transport::connect(&socket, &request) })
        .await
        .map_err(anyhow::Error::msg)
}
