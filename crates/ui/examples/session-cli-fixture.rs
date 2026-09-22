//! Production shell + native PTY with isolated, synthetic provider history.
//! This fixture never calls a model or touches an existing provider session.
use gpui::{AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use sha2::{Digest, Sha256};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use zeron_proto::{AgentEvent, HarnessId, SessionSurface, SessionSurfaceState};
use zeron_ui::*;

const CHAT: &str = "native-cli-fixture";
const ID: &str = "11111111-1111-4111-8111-111111111111";
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let cwd = temp.path().to_string_lossy().into_owned();
    let executable = temp.path().join("fixture-codex");
    std::fs::write(
        &executable,
        r##"#!/bin/sh
case "$1" in resume) ;; *) exit 1 ;; esac
stty -echo
printf '\033[2J\033[HNoches native CLI fixture\r\n\r\nSame session and working directory. This is an isolated fake provider.\r\n\r\n> '
IFS= read -r prompt
cat >> history.jsonl <<'ROWS'
{"type":"event_msg","timestamp":"2026-09-22T12:00:00Z","payload":{"type":"user_message","message":"Check the native view"}}
{"type":"response_item","timestamp":"2026-09-22T12:00:01Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The native CLI reply is now part of the same conversation."}]}}
{"type":"event_msg","payload":{"type":"task_complete"}}
ROWS
printf 'Check the native view\r\n\r\nThe native CLI reply is now part of the same conversation.\r\n\r\n> '
exec sleep 60
"##,
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
    let runtime = tokio::runtime::Runtime::new()?;
    let _runtime_guard = runtime.enter();
    let registry = Arc::new(zeron_engine::HarnessRegistry::new());
    registry.register(Arc::new(
        zeron_harness::CodexHarness::new().with_executable(executable),
    ));
    let profile =
        zeron_engine::EngineProfile::development(&temp.path().join("engine"), "fixture", "fixture");
    let journals = profile.store_root().join("journals");
    let core = Arc::new(runtime.block_on(async {
        zeron_engine::EngineCore::assemble_with_profile(profile, registry, HarnessId::Codex, None)
    })?);
    let config = zeron_proto::ChatConfig {
        harness: HarnessId::Codex,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
    };
    core.workspace.create_chat(
        CHAT,
        None,
        Some(&core.device_id),
        Some(config),
        Some(cwd.clone()),
    )?;
    core.workspace
        .rename_chat(CHAT, "Same session, two views")?;
    core.workspace.set_chat_harness_session(CHAT, ID, &cwd);
    zeron_engine::RunJournal::open(&journals)?.append(
        CHAT,
        &AgentEvent::SessionStarted {
            harness: HarnessId::Codex,
            model: "fixture".into(),
            tools: vec![],
            cwd: cwd.clone(),
            session_id: ID.into(),
            assistant_message_id: "initial".into(),
        },
    )?;
    let history = format!(
        "{}\n",
        serde_json::json!({"type":"session_meta","payload":{"id":ID,"cwd":cwd}})
    );
    std::fs::write(temp.path().join("history.jsonl"), &history)?;
    let request = zeron_proto::RunRequest {
        prompt: String::new(),
        harness: Some(HarnessId::Codex),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.clone(),
        sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume: Some(ID.into()),
        attachments: vec![],
        worktree: None,
    };
    let binding = serde_json::json!({"surface":"chat","harness":"codex","session_id":ID,"request":request,
        "checkpoint":{"path":temp.path().join("history.jsonl"),"offset":history.len(),"digest":format!("{:x}",Sha256::digest(history.as_bytes()))},
        "terminal":null,"activity_file":temp.path().join("activity"),"input_offset":null});
    std::fs::write(
        journals.join(format!("{:x}.native.json", Sha256::digest(CHAT.as_bytes()))),
        serde_json::to_vec(&binding)?,
    )?;
    let entry = serde_json::from_value(
        serde_json::json!({"id":"initial","role":"assistant","parts":[{"id":"initial-text","kind":"text","text":"Use the Chat / CLI toggle to continue this conversation in the provider's native terminal."}],"createdAt":1788900000000_i64,"deviceId":core.device_id,"status":"complete"}),
    )?;
    core.doc_host.open(CHAT)?.doc().push_message(&entry)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let _ipc = runtime.block_on(zeron_engine::serve_ipc(port, core.rpc_service()))?;
    let data = temp.path().join("ui");
    std::fs::create_dir(&data)?;
    let boot = EngineBootConfig {
        remote: None,
        data_dir: data.clone(),
        ipc_port: port,
        edge_url: String::new(),
        edge_token: None,
        org_id: None,
        workos_client_id: None,
        default_harness: HarnessId::Codex,
    };
    let handle = runtime.block_on(state::EngineHandle::bootstrap(boot.clone()))?;
    let chats = core.workspace.read_chats()?;
    let device = core.device_id.clone();
    let visual_core = core.clone();
    let failure = Arc::new(std::sync::Mutex::new(None));
    let result = failure.clone();
    gpui_platform::application()
        .with_assets(icons::Assets)
        .run(move |cx| {
            gpui_tokio::init(cx);
            gpui_base::init(cx);
            let settings = settings::UiSettings::default();
            settings::init(settings.clone(), data.clone(), cx);
            let fonts = typography::register_fonts(cx);
            typography::init(
                settings.ui_font_family.clone(),
                settings.ui_font_size,
                settings.terminal_font_family.clone(),
                settings.terminal_font_size,
                settings.code_font_family.clone(),
                settings.code_font_size,
                fonts,
                cx,
            );
            theme_library::init(data.clone(), cx);
            appearance::init(
                appearance::AppearanceMode::Dark,
                settings.theme_selection,
                settings.accent,
                settings.surface,
                cx,
            );
            history::init(
                settings.git_history_columns,
                settings.git_history_column_widths,
                settings.git_history_column_order,
                settings.git_history_author_display,
                cx,
            );
            composer::init(cx, settings.composer_send_behavior);
            terminal::panel::init(cx);
            app_menus::init(cx);
            let rpc = handle.clone();
            let state = cx.new(|_| {
                let mut s = state::AppState::new();
                s.fixture_attachment_engine(handle);
                s.connection = zeron_proto::view::ConnectionStatus::Ready;
                s.workspace_scope = Some(zeron_proto::WorkspaceScope::Development);
                s.local_device_id = Some(device.clone());
                s.chats = chats;
                s.selected_chat = Some(CHAT.into());
                s.auto_selected = true;
                s.chats_synced = true;
                s.spaces_synced = true;
                s.no_project = true;
                s
            });
            let window = cx
                .open_window(
                    WindowOptions {
                        window_background: theme::Theme::of(cx).window_background_appearance(),
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            gpui::point(px(20.), px(40.)),
                            size(px(1100.), px(800.)),
                        ))),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx)),
                )
                .unwrap();
            cx.activate(true);
            cx.spawn(async move |cx| {
                let run: anyhow::Result<()> = async {
                    for (surface, file) in [
                        (SessionSurface::Chat, "chat"),
                        (SessionSurface::Cli, "cli"),
                        (SessionSurface::Chat, "returned-chat"),
                    ] {
                        let response = rpc
                            .client()
                            .call_as::<SessionSurfaceState>(
                                zeron_rpc::methods::SWITCH_SESSION_SURFACE,
                                serde_json::json!({"chatId":CHAT,"target":surface}),
                            )
                            .await?;
                        if let Some(terminal) = response.terminal {
                            cx.background_executor()
                                .timer(Duration::from_millis(300))
                                .await;
                            rpc.client()
                                .call(
                                    zeron_rpc::methods::WRITE_TERMINAL,
                                    serde_json::json!({"terminalId":terminal.id,"data":"DQ=="}),
                                )
                                .await?;
                        }
                        let entries = visual_core.doc_host.open(CHAT)?.doc().read_entries()?;
                        state.update(cx, |s, cx| {
                            s.receive_transcript_frame(
                                zeron_doc::TranscriptFrame::Reset {
                                    reset: serde_json::from_value(
                                        serde_json::to_value(entries).unwrap(),
                                    )
                                    .unwrap(),
                                },
                                cx,
                            )
                            .unwrap();
                            cx.notify();
                        });
                        cx.background_executor()
                            .timer(Duration::from_millis(1400))
                            .await;
                        gpui::AnyWindowHandle::from(window).update(cx, |_, w, cx| {
                            w.draw(cx).clear();
                            w.render_to_image()?
                                .save(output.join(format!("{file}.png")))?;
                            Ok::<_, anyhow::Error>(())
                        })??;
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = run {
                    *result.lock().unwrap() = Some(format!("{error:#}"));
                }
                let _ = window.update(cx, |_, w, _| w.remove_window());
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    runtime.block_on(core.shutdown());
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
