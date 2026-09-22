//! Updates belong to this desktop process, even when it controls a remote host.
use super::*;
use std::sync::{Arc, atomic::Ordering};

impl Shell {
    pub(super) fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update_checking
            || matches!(
                self.update_flow,
                UpdateFlow::Downloading | UpdateFlow::Ready(_) | UpdateFlow::Installing
            )
        {
            return;
        }
        if !zeron_update::identity::distributed() {
            self.update_status =
                "Local source build. Install Noches or Noches Dev to receive application updates."
                    .into();
            cx.notify();
            return;
        }
        self.update_checking = true;
        self.update_status = "Checking for updates…".into();
        if manual {
            self.update_dismissed = None;
        }
        let check = Tokio::spawn(cx, async { zeron_update::fetch_latest("").await });
        self.update_check_task = Some(cx.spawn(async move |this, cx| {
            let result = check.await;
            let _ = this.update(cx, |shell, cx| {
                shell.update_checking = false;
                shell.update_checked_at = Some(Utc::now().format("%Y-%m-%d %H:%M UTC").to_string());
                match result {
                    Ok(Ok(manifest)) => {
                        let newer = zeron_update::version_newer(
                            &manifest.version,
                            zeron_update::current_version(),
                        );
                        shell.update_status = if newer {
                            format!("Version {} is available.", manifest.version).into()
                        } else {
                            "You're up to date.".into()
                        };
                        shell.update_manifest = newer.then_some(manifest);
                    }
                    error => {
                        shell.update_manifest = None;
                        shell.update_status = match error {
                            Ok(Err(error)) => {
                                format!("Could not check for updates: {error:#}").into()
                            }
                            Err(error) => format!("Could not check for updates: {error}").into(),
                            _ => unreachable!(),
                        };
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn update_label(&self) -> SharedString {
        match &self.update_flow {
            UpdateFlow::Idle => self.update_status.clone(),
            UpdateFlow::Downloading => {
                let received = self.update_progress.received.load(Ordering::Relaxed);
                let total = self.update_progress.total.load(Ordering::Relaxed);
                if total > 0 && received >= total {
                    "Verifying and preparing update…".into()
                } else if total > 0 {
                    format!(
                        "Downloading… {}% ({:.1} / {:.1} MB)",
                        received * 100 / total,
                        received as f64 / 1e6,
                        total as f64 / 1e6
                    )
                    .into()
                } else {
                    "Starting download…".into()
                }
            }
            UpdateFlow::Ready(_) => {
                "Update ready. Install and restart when your work is finished.".into()
            }
            UpdateFlow::Installing => "Installing update…".into(),
            UpdateFlow::Failed(message) => format!("Update failed: {message}").into(),
        }
    }

    pub(super) fn render_update_strip(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let manifest = self.update_manifest.as_ref()?;
        if self.update_dismissed.as_deref() == Some(&manifest.version) {
            return None;
        }
        Some(
            div()
                .id("update-strip")
                .mx(px(Theme::SPACE_SM))
                .p(px(8.0))
                .rounded(px(6.0))
                .bg(theme.accent_wash)
                .text_color(theme.accent)
                .text_size(crate::typography::ui_rems(11.0))
                .cursor_pointer()
                .on_click(
                    cx.listener(|this, _, _, cx| this.open_settings(SettingsSection::Updates, cx)),
                )
                .child(self.update_label())
                .into_any_element(),
        )
    }

    pub(super) fn render_updates_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let busy = self.update_checking
            || matches!(
                self.update_flow,
                UpdateFlow::Downloading | UpdateFlow::Installing
            );
        let ready = matches!(self.update_flow, UpdateFlow::Ready(_));
        let can_install = self.install.supports_desktop_update();
        let has_update = self.update_manifest.is_some();
        let mut page = div()
            .id("updates-page")
            .overflow_y_scroll()
            .h_full()
            .p(px(32.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .text_color(theme.text)
            .text_size(crate::typography::ui_rems(13.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(24.0))
                    .child("Updates"),
            )
            .child(format!(
                "{} {} · {} channel",
                zeron_update::identity::app_name(),
                zeron_update::current_version(),
                zeron_update::identity::channel()
            ))
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("Build {}", zeron_update::identity::commit())),
            )
            .child(self.update_label())
            .child(self.update_status.clone());
        if let Some(checked) = &self.update_checked_at {
            page = page.child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("Last checked {checked}")),
            );
        }
        if !busy && !ready {
            page = page.child(
                div()
                    .id("check-for-updates")
                    .role(gpui::Role::Button)
                    .aria_label("Check for updates")
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .bg(theme.accent_wash)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(true, cx)))
                    .child("Check for updates"),
            );
        }
        if has_update && can_install && !busy {
            page = page.child(
                div()
                    .id("install-update")
                    .role(gpui::Role::Button)
                    .aria_label(if ready {
                        "Install and restart"
                    } else {
                        "Download update"
                    })
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .bg(theme.accent_wash)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.on_update_strip_click(cx)))
                    .child(if ready {
                        "Install and restart"
                    } else {
                        "Download update"
                    }),
            );
        }
        if !can_install {
            page = page.child("This installation is not managed. Install a Noches desktop package to enable in-app installation.");
        }
        if let Some(manifest) = &self.update_manifest {
            let url = manifest.notes_url.clone();
            if url.starts_with("https://github.com/") {
                page = page.child(
                    div()
                        .id("update-release-notes")
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .child("Release notes"),
                );
            }
        }
        page.child(div().text_color(theme.text_muted).child("Downloads can run while you work. Finish active runs, close terminals, and save files before restarting. This updates only this application.")).into_any_element()
    }

    pub(super) fn on_update_strip_click(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.update_flow, UpdateFlow::Idle) {
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
            UpdateFlow::Downloading => self.update_flow = UpdateFlow::Downloading,
            UpdateFlow::Installing => self.update_flow = UpdateFlow::Installing,
            _ => self.begin_update_download(cx),
        }
    }

    pub(super) fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        let Some(manifest) = self.update_manifest.clone() else {
            return;
        };
        let install = self.install.clone();
        let data_dir = self.data_dir.clone();
        self.update_progress = Arc::default();
        let progress = self.update_progress.clone();
        self.update_flow = UpdateFlow::Downloading;
        let download = Tokio::spawn(cx, async move {
            install
                .stage_with_progress("", &manifest, &data_dir, progress)
                .await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let result = download.await;
            let _ = this.update(cx, |shell, cx| {
                shell.update_flow = match result {
                    Ok(Ok(path)) => UpdateFlow::Ready(path),
                    Ok(Err(error)) => UpdateFlow::Failed(format!("{error:#}").into()),
                    Err(error) => UpdateFlow::Failed(error.to_string().into()),
                };
                cx.notify();
            });
        }));
        self.update_progress_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let active = this
                    .update(cx, |shell, cx| {
                        if matches!(shell.update_flow, UpdateFlow::Downloading) {
                            cx.notify();
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        }));
        cx.notify();
    }

    pub(super) fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        if cx.windows().len() > 1 {
            self.update_flow = UpdateFlow::Ready(staged);
            self.update_status = "Close other Noches windows before installing so their editors and local work remain safe.".into();
            cx.notify();
            return;
        }
        if !self.prepare_exit(PendingExit::InstallUpdate(staged.clone()), cx) {
            return;
        }
        let engine = if self.boot.remote.is_none() {
            self.state.read(cx).engine().cloned()
        } else {
            None
        };
        if self.boot.remote.is_none() && engine.is_none() {
            self.update_flow = UpdateFlow::Ready(staged);
            self.update_status = "Wait for the local engine to connect before restarting.".into();
            cx.notify();
            return;
        }
        self.update_flow = UpdateFlow::Installing;
        let guard = Tokio::spawn(cx, async move {
            if let Some(engine) = engine {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    engine
                        .client()
                        .call(methods::CHECK_UPDATE_READY, serde_json::json!({})),
                )
                .await
                .map_err(|_| "The engine did not answer. Retry once it is connected.".to_owned())?
                .map_err(|error| error.to_string())?;
            }
            Ok::<_, String>(())
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let result = guard.await;
            let _ = this.update(cx, |shell, cx| {
                match result {
                    Ok(Ok(())) => {
                        if !shell.prepare_exit(PendingExit::InstallUpdate(staged.clone()), cx) {
                            shell.update_flow = UpdateFlow::Ready(staged);
                            return;
                        }
                        match shell
                            .install
                            .apply_desktop(&staged, shell.boot.remote.is_none())
                        {
                            Ok(()) => crate::app_menus::quit_after_save(cx),
                            Err(error) => {
                                shell.update_status =
                                    format!("Could not install: {error:#}").into();
                                shell.update_flow = UpdateFlow::Ready(staged);
                                shell.sidebar_notice = Some(shell.update_status.clone());
                            }
                        }
                    }
                    error => {
                        shell.update_status = match error {
                            Ok(Err(message)) => message.into(),
                            Err(error) => error.to_string().into(),
                            _ => unreachable!(),
                        };
                        shell.sidebar_notice = Some(shell.update_status.clone());
                        shell.update_flow = UpdateFlow::Ready(staged);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}
