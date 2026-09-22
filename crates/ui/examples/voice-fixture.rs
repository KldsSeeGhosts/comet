//! Render voice controls with isolated fixture state. No microphone or API calls.
use gpui::{AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use zeron_ui::*;
fn main() -> anyhow::Result<()> {
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let data = temp.path().to_path_buf();
    let failure = Arc::new(Mutex::new(None));
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
            let state = cx.new(|_| {
                let mut s = state::AppState::new();
                s.connection = zeron_proto::view::ConnectionStatus::Ready;
                s.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
                s.local_device_id = Some("fixture".into());
                s.chats_synced = true;
                s.spaces_synced = true;
                s.auto_selected = true;
                s
            });
            let boot = state::EngineBootConfig {
                remote: None,
                data_dir: data,
                ipc_port: 0,
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zeron_proto::HarnessId::Mock,
            };
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
                    for (name, mode, live, width) in [
                        (
                            "voice-setup-dark",
                            appearance::AppearanceMode::Dark,
                            false,
                            1100.,
                        ),
                        (
                            "voice-live-dark",
                            appearance::AppearanceMode::Dark,
                            true,
                            1100.,
                        ),
                        (
                            "voice-live-light",
                            appearance::AppearanceMode::Light,
                            true,
                            1100.,
                        ),
                        (
                            "voice-setup-small",
                            appearance::AppearanceMode::Dark,
                            false,
                            700.,
                        ),
                    ] {
                        cx.update(|cx| appearance::set_mode(mode, cx));
                        window.update(cx, |shell, w, cx| {
                            w.resize(size(px(width), px(800.)));
                            shell.fixture_voice_panel(live, cx);
                        })?;
                        cx.background_executor()
                            .timer(Duration::from_millis(800))
                            .await;
                        let raw_window: gpui::AnyWindowHandle = window.into();
                        raw_window.update(cx, |_, w, cx| {
                            w.draw(cx).clear();
                            w.render_to_image()?
                                .save(output.join(format!("{name}.png")))?;
                            Ok::<_, anyhow::Error>(())
                        })??;
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = run {
                    *result.lock().unwrap() = Some(error.to_string());
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
