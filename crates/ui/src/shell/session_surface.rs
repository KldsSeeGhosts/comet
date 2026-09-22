use super::*;
use crate::session_surface::SessionSurfaceControl;
use zeron_workspace::PaneId;

pub(super) struct NativeSurface {
    pub control: Entity<SessionSurfaceControl>,
    _observe: Subscription,
}

impl Shell {
    pub(super) fn ensure_session_surfaces(&mut self, cx: &mut Context<Self>) {
        let mut panes = self.workspace.chat_pane_sessions();
        if !self.workspace_mode()
            && let Some(pane) = self.workspace.layout.active_pane_id()
        {
            panes = vec![(pane, self.state.read(cx).selected_chat.clone())];
        }
        let live: std::collections::HashSet<_> = panes
            .iter()
            .filter(|(_, chat)| chat.is_some())
            .map(|(pane, _)| *pane)
            .collect();
        self.native_surfaces.retain(|pane, _| live.contains(pane));
        for (pane, chat) in panes {
            let Some(chat) = chat else {
                continue;
            };
            if self
                .native_surfaces
                .get(&pane)
                .is_some_and(|s| s.control.read(cx).chat == chat)
            {
                continue;
            }
            let control = cx.new(|cx| SessionSurfaceControl::new(chat, self.state.clone(), cx));
            let mut was_cli = false;
            let observe = cx.observe(&control, move |shell, control, cx| {
                let cli = control.read(cx).is_cli();
                if was_cli && !cli && shell.workspace.layout.active_pane_id() == Some(pane) {
                    shell.focus_composer(cx);
                }
                was_cli = cli;
                cx.notify();
            });
            self.native_surfaces.insert(
                pane,
                NativeSurface {
                    control,
                    _observe: observe,
                },
            );
        }
    }

    pub(super) fn surface_control(&self, pane: PaneId) -> Option<AnyElement> {
        self.native_surfaces
            .get(&pane)
            .map(|s| s.control.clone().into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use zeron_proto::{SessionSurface, SessionSurfaceState};

    #[gpui::test]
    fn native_view_keeps_pane_composer_and_draft(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    remote: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                shell.open_chat("native-session".into(), cx);
                shell.ensure_session_surfaces(cx);
                let pane = shell.workspace.layout.active_pane_id().unwrap();
                let composer = shell.composer.clone();
                composer.update(cx, |composer, cx| {
                    composer
                        .input
                        .update(cx, |input, cx| input.set_text("unsent draft", cx))
                });
                let transcript = shell.transcript.clone();
                let layout = serde_json::to_value(&shell.workspace.layout).unwrap();
                let control = shell.native_surfaces[&pane].control.clone();
                for surface in [SessionSurface::Cli, SessionSurface::Chat] {
                    control.update(cx, |control, cx| {
                        control.accept(
                            SessionSurfaceState {
                                surface,
                                terminal: None,
                                can_switch: true,
                                reason: None,
                            },
                            cx,
                        )
                    });
                    shell.ensure_session_surfaces(cx);
                    assert_eq!(shell.composer.entity_id(), composer.entity_id());
                    assert_eq!(shell.transcript.entity_id(), transcript.entity_id());
                    assert_eq!(
                        serde_json::to_value(&shell.workspace.layout).unwrap(),
                        layout
                    );
                    assert_eq!(
                        shell.native_surfaces[&pane].control.entity_id(),
                        control.entity_id()
                    );
                    assert_eq!(composer.read(cx).input.read(cx).text(), "unsent draft");
                    assert_eq!(control.read(cx).is_cli(), surface == SessionSurface::Cli);
                }
            })
            .unwrap();
    }
}
