//! Main-thread routing from local agent tools into the shell's existing tabs.
use super::*;
use serde_json::{Value, json};
use zeron_browser::{Action, Request};

impl Shell {
    pub(super) fn ensure_browser_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Unit-test shells must not bind a real socket: its accept thread
        // wakes the deterministic test scheduler from a foreign thread when
        // the shell drops, failing whichever test is running in parallel.
        if cfg!(test) || self.browser_control_started || self.state.read(cx).remote_host.is_some() {
            return;
        }
        self.browser_control_started = true;
        let path = zeron_browser::socket_path(&self.data_dir);
        match zeron_browser::transport::bind(&path) {
            Ok((server, mut requests)) => {
                let task = cx.spawn_in(window, async move |this, cx| {
                    while let Some(pending) = requests.recv().await {
                        if pending.reply.is_closed() {
                            continue;
                        }
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.handle_browser_request(pending.request, pending.reply, window, cx);
                        });
                    }
                });
                self.browser_control = Some((server, task));
            }
            Err(error) => tracing::warn!(%error, "Could not start integrated browser control"),
        }
    }

    fn handle_browser_request(
        &mut self,
        request: Request,
        reply: zeron_browser::ReplySender,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.read(cx);
        let local = state.remote_host.is_none()
            && state.chats.iter().any(|chat| {
                chat.id == request.session
                    && Some(chat.device_id.as_str()) == state.local_device_id.as_deref()
            });
        if !local {
            let _ = reply.send(Err("Conversation is not available on this desktop. Remote browser control is not connected.".into()));
            return;
        }
        let tabs: Vec<u64> = self
            .right_tabs
            .get(&request.session)
            .into_iter()
            .flatten()
            .filter_map(|s| {
                if let RightSurface::Browser(id) = s {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        if let Some(tab) = request.action.tab() {
            if !tabs.contains(&tab) || !self.browsers.contains_key(&tab) {
                let _ = reply.send(Err(
                    "Tab does not belong to this conversation or has been closed".into(),
                ));
                return;
            }
        }
        match request.action {
            Action::Tabs => {
                let tabs: Vec<_> = tabs
                    .into_iter()
                    .filter_map(|tab| {
                        self.browsers
                            .get(&tab)
                            .map(|browser| browser_state(tab, &browser.read(cx).page))
                    })
                    .collect();
                let _ = reply.send(Ok(json!({"engine":"chromium","tabs":tabs})));
            }
            Action::Open { url } => {
                let url = match crate::browser::model::normalize_address(&url) {
                    Ok(url) => url,
                    Err(error) => {
                        let _ = reply.send(Err(error.into()));
                        return;
                    }
                };
                if tabs.len() >= 16 {
                    let _=reply.send(Err("Close an existing browser tab before opening more than 16 in this conversation".into()));
                    return;
                }
                self.add_browser_for_session(request.session.clone(), Some(url), window, cx);
                if self.active_chat == request.session && !self.right_pane_open(cx) {
                    self.toggle_right_pane(cx);
                }
                let id = self.browser_seq;
                let result = browser_state(id, &self.browsers[&id].read(cx).page);
                let _ = reply.send(if let Some(error) = result["error"].as_str() {
                    Err(error.into())
                } else {
                    Ok(result)
                });
            }
            Action::Close { tab } => {
                if self.active_chat == request.session {
                    self.close_right_surface(RightSurface::Browser(tab), window, cx);
                } else {
                    if let Some(browser) = self.browsers.remove(&tab) {
                        browser.update(cx, |browser, cx| browser.close(cx));
                    }
                    self.browser_subs.remove(&tab);
                    if let Some(tabs) = self.right_tabs.get_mut(&request.session) {
                        tabs.retain(|surface| *surface != RightSurface::Browser(tab));
                    }
                    self.panels.update(&request.session, |panel| {
                        if panel.right_active == RightSurface::Browser(tab) {
                            panel.right_active = RightSurface::Picker;
                        }
                    });
                }
                let _ = reply.send(Ok(json!({"closed":tab})));
                cx.notify();
            }
            action => {
                let tab = action.tab().unwrap();
                let browser = self.browsers[&tab].clone();
                browser.update(cx, |browser, cx| {
                    browser.agent_action(tab, action, reply, window, cx)
                });
            }
        }
    }
}
fn browser_state(tab: u64, page: &crate::browser::model::PageState) -> Value {
    json!({"tab":tab,"url":page.url,"title":page.title,"loading":page.loading,"error":page.error,"canBack":page.can_back,"canForward":page.can_forward})
}
