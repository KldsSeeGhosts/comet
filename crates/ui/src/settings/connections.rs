//! Saved native connections. Each computer opens in a separate window so local
//! files, pending edits and running sessions keep their original engine owner.
use super::widgets;
use crate::{
    popover,
    state::{AppState, EngineBootConfig},
    theme::Theme,
};
use gpui::{Context, Entity, SharedString, Subscription, Window, div, prelude::*, px};
use zeron_rpc::remote::{ConnectionProfile, ConnectionState, Connections};

pub struct ConnectionsPage {
    state: Entity<AppState>,
    boot: EngineBootConfig,
    hosts: Vec<ConnectionProfile>,
    error: Option<String>,
    notice: Option<String>,
    _observe: Subscription,
}
impl ConnectionsPage {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let result = Connections::load(&boot.data_dir);
        let (hosts, error) = match result {
            Ok(c) => (c.hosts, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            boot,
            hosts,
            error,
            notice: None,
            _observe: observe,
        }
    }
    fn paste(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let code = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .ok_or_else(|| anyhow::anyhow!("Copy a Noches connection code first."))?;
            let host = ConnectionProfile::from_code(&code)?;
            let name = host.name.clone();
            let mut connections = Connections::load(&self.boot.data_dir)?;
            connections.add(host);
            connections.save(&self.boot.data_dir)?;
            self.hosts = connections.hosts;
            self.notice = Some(format!(
                "{name} added. Open it to see its projects and sessions."
            ));
            Ok(())
        })();
        self.error = result.err().map(|e| e.to_string());
        cx.notify();
    }
    fn forget(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let mut connections = Connections::load(&self.boot.data_dir)?;
            connections.hosts.retain(|h| h.id != id);
            connections.save(&self.boot.data_dir)?;
            self.hosts = connections.hosts;
            self.notice =
                Some("Connection removed from this computer. Existing windows remain open.".into());
            Ok(())
        })();
        self.error = result.err().map(|e| e.to_string());
        cx.notify();
    }
}

pub fn status_label(status: Option<&ConnectionState>) -> &'static str {
    match status {
        Some(ConnectionState::Connected) => "Connected",
        Some(ConnectionState::Connecting) => "Connecting",
        Some(ConnectionState::Reconnecting) => "Reconnecting",
        Some(ConnectionState::Unauthorized) => "Access revoked",
        None => "Saved computer",
    }
}

impl Render for ConnectionsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let active = self.boot.remote.as_ref().map(|p| p.id.as_str());
        let state = self.state.read(cx);
        let active_status = state.remote_connection.clone();
        let rows: Vec<_> = self
            .hosts
            .clone()
            .into_iter()
            .map(|host| {
                let is_active = active == Some(host.id.as_str());
                let open_host = host.clone();
                let remove_id = host.id.clone();
                let caption = if is_active {
                    status_label(active_status.as_ref())
                } else {
                    "Saved computer"
                };
                div()
                    .p(px(14.0))
                    .rounded(px(10.0))
                    .bg(theme.surface)
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(widgets::row_tile(&theme, crate::icons::MONITOR))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(widgets::row_title(&theme, &host.name))
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(format!(
                                        "{caption} · {}",
                                        host.endpoint
                                    ))),
                            ),
                    )
                    .child(
                        popover::btn_ghost(&theme, "Remove", format!("forget-{}", host.id))
                            .id(SharedString::from(format!("forget-{}", host.id)))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.forget(&remove_id, cx)),
                            ),
                    )
                    .child(
                        popover::btn_primary(&theme, "Open computer")
                            .id(SharedString::from(format!("open-{}", host.id)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                crate::open_connection_window(
                                    this.boot.clone(),
                                    Some(open_host.clone()),
                                    cx,
                                );
                            })),
                    )
            })
            .collect();
        div().id("connections-page").size_full().overflow_y_scroll()
            .child(widgets::page_column()
                .child(widgets::page_header(&theme, "Connections", Some(self.hosts.len())))
                .child(widgets::page_subtitle(&theme, "Open another computer's projects and sessions. Work keeps running there when this connection drops."))
                .child(div().mt(px(20.0)).flex().items_center().justify_between()
                    .child(widgets::row_title(&theme, "This computer"))
                    .child(popover::btn_ghost(&theme, "Open local window", "open-local-window")
                        .id("open-local-window")
                        .on_click(cx.listener(|this, _, _, cx| crate::open_connection_window(this.boot.clone(), None, cx)))))
                .child(div().mt(px(20.0)).flex().flex_col().gap(px(10.0)).children(rows))
                .child(div().mt(px(20.0))
                    .child(popover::btn_primary(&theme, "Paste connection code")
                        .id("paste-connection-code")
                        .on_click(cx.listener(|this, _, _, cx| this.paste(cx)))))
                .child(widgets::page_subtitle(&theme, "Use a connection code from a computer with Noches remote access enabled. Both computers need access to the same tailnet."))
                .when_some(self.notice.clone(), |el, notice| el.child(div().mt(px(12.0)).text_color(theme.text_muted).child(notice)))
                .when_some(self.error.clone(), |el, error| el.child(div().mt(px(12.0)).text_color(theme.danger).child(error))))
    }
}
