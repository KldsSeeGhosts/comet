//! Project badges for sidebar rows and pane headers.
//!
//! The badge is the repository's favicon/app icon when one can be discovered:
//! local files for local projects, `WorkspaceFilesClient` reads for remote
//! devices. Decoding runs off the render thread and each project/connection
//! caches one small entity; without artwork the shell draws a colored monogram.
use super::*;
use crate::files::client::{FilesRequestContext, WorkspaceFilesClient};
use crate::image_media::{MediaImage, decode_project_icon, release_media};

pub(super) const ICON_PATHS: &[&str] = &[
    "public/apple-touch-icon.png",
    "apple-touch-icon.png",
    "public/favicon.svg",
    "favicon.svg",
    "public/favicon.png",
    "public/icon.png",
    "public/logo.png",
    "favicon.png",
    "app/icon.png",
    "src/app/icon.png",
    "public/favicon.ico",
    "favicon.ico",
    "app/favicon.ico",
    "static/favicon.ico",
    "src-tauri/icons/icon.png",
    "assets/icon.png",
    "src/assets/icon.png",
];

/// A cached badge is refreshed this long after it loaded.
const ICON_TTL: Duration = Duration::from_secs(300);
/// Full-cache sweep interval. Individual lookups refresh their own stale
/// entry, so the sweep only drops projects that left the sidebar and never
/// runs once per card.
const ICON_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Where a badge's bytes come from. `targetDeviceId` only routes a request
/// through the engine; it does not say whether this process shares the
/// engine's disk, so the local/RPC choice stays explicit instead of being
/// inferred from `target_device_id.is_none()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectIconSource {
    /// The engine runs in this process, so its project roots are on this disk.
    LocalFs,
    /// Everything else reads through `WorkspaceFilesClient`, including a
    /// space on the engine's own device when that engine is remote.
    WorkspaceRpc,
}

/// Direct `std::fs` is only correct when the engine shares this process's
/// filesystem and the space lives on that engine's device. A remote engine's
/// `local_device_id` names the engine host, not this machine.
fn icon_source(
    engine_mode: Option<&EngineMode>,
    local_device_id: Option<&str>,
    device_id: &str,
) -> ProjectIconSource {
    match engine_mode {
        Some(EngineMode::InProcess) if local_device_id == Some(device_id) => {
            ProjectIconSource::LocalFs
        }
        _ => ProjectIconSource::WorkspaceRpc,
    }
}

fn load_local_icon(root: &std::path::Path) -> Option<MediaImage> {
    // A project can point at a subdirectory; match the remote workspace RPC's
    // checkout-root resolution, including linked worktrees with a .git file.
    let canonical = root.canonicalize().ok()?;
    let root = canonical
        .ancestors()
        .find(|path| path.join(".git").exists())
        .unwrap_or(&canonical);
    for path in ICON_PATHS {
        let path = root.join(path);
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::File::open(&path).ok().and_then(|file| {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(zeron_proto::MAX_WORKSPACE_IMAGE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            Some(bytes)
        })?;
        let mime = match path.extension().and_then(|e| e.to_str()) {
            Some("svg") => "image/svg+xml",
            Some("ico") => "image/x-icon",
            _ => "image/png",
        };
        return decode_project_icon(mime, bytes).ok();
    }
    None
}

// Curated badge tones: (dark appearance, light appearance). Keep the ordering
// stable so projects retain their assigned color. These are explicit colors,
// independent of the selected theme accent; only their appearance variant
// changes.
const MONOGRAM_PALETTE: [(u32, u32); 8] = [
    (0x94a3b8, 0x475569), // slate
    (0x93c5fd, 0x2563eb), // blue
    (0xc4b5fd, 0x7c3aed), // violet
    (0xfda4af, 0xbe123c), // rose
    (0xfcd34d, 0xa16207), // amber
    (0x6ee7b7, 0x047857), // emerald
    (0x5eead4, 0x0f766e), // teal
    (0xfdba74, 0xc2410c), // orange
];

/// A stable palette slot for a project seed (FNV-1a).
pub(super) fn monogram_tone(seed: &str, theme: &Theme) -> gpui::Hsla {
    let hash = seed.bytes().fold(2166136261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16777619)
    });
    let (dark, light) = MONOGRAM_PALETTE[hash as usize % MONOGRAM_PALETTE.len()];
    gpui::rgb(if theme.appearance == crate::theme::Appearance::Dark {
        dark
    } else {
        light
    })
    .into()
}

pub(super) fn monogram(name: &str, seed: &str, selected: bool, theme: &Theme) -> AnyElement {
    let letter = name
        .trim()
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    let tone = monogram_tone(seed, theme);
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(tone.opacity(if selected { 0.26 } else { 0.16 }))
        .text_color(tone)
        .font_family(theme.font_mono.clone())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(
            div()
                .w_full()
                .text_center()
                .text_size(px(9.0))
                .line_height(px(13.0))
                .child(letter),
        )
        .into_any_element()
}

/// Stable key for device-local identity that belongs to one workspace profile.
/// Noches has no sidebar-pin store yet, but the icon cache must still not mix
/// profiles that can reach the same path on the same device.
fn profile_key(state: &AppState, development_org_id: Option<&str>) -> Option<String> {
    match state.workspace_scope? {
        WorkspaceScope::Local => Some("local".to_string()),
        WorkspaceScope::Synced => {
            let AuthState::SignedIn {
                user,
                org_id: Some(org_id),
            } = state.auth.as_ref()?
            else {
                return None;
            };
            Some(format!("synced:{org_id}:{}", user.id))
        }
        WorkspaceScope::Development => {
            let AuthState::SignedIn { user, .. } = state.auth.as_ref()? else {
                return None;
            };
            let (user_id, token_org_id) = user
                .id
                .split_once('@')
                .map_or((user.id.as_str(), None), |(user_id, org_id)| {
                    (user_id, (!org_id.is_empty()).then_some(org_id))
                });
            if user_id.is_empty() {
                return None;
            }
            let org_id = token_org_id
                .or(development_org_id.filter(|org_id| !org_id.is_empty()))
                .unwrap_or(zeron_engine::DEFAULT_ORG_ID);
            Some(format!("development:{org_id}:{user_id}"))
        }
    }
}

pub(super) struct ProjectIcon {
    name: String,
    seed: String,
    media: Option<MediaImage>,
    refreshed: std::time::Instant,
    _task: Task<()>,
}

impl ProjectIcon {
    fn new(
        name: String,
        seed: String,
        context: FilesRequestContext,
        source: ProjectIconSource,
        engine: Option<crate::state::EngineHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this, cx| release_media(this.media.take(), cx))
            .detach();
        let task = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let load = async {
                if source == ProjectIconSource::LocalFs {
                    let root = std::path::PathBuf::from(context.cwd);
                    return executor.spawn(async move { load_local_icon(&root) }).await;
                }
                let client = WorkspaceFilesClient::new(engine?, context.clone());
                for path in ICON_PATHS {
                    // This also resolves the current checkout identity on the owning host.
                    let file = match client
                        .read_file(zeron_proto::ReadWorkspaceFileRequest {
                            target: context.target.clone(),
                            path: (*path).into(),
                        })
                        .await
                    {
                        Ok(file) => file,
                        Err(error) if error.retryable() => return None,
                        Err(_) => continue,
                    };
                    // Skip candidates the remote engine cannot serve (such as an older
                    // host rejecting .ico or an oversized file), while a fetched but
                    // corrupt first match stays terminal and falls back to the monogram.
                    let (mime, bytes) =
                        match client.read_image((*path).into(), file.checkout_id).await {
                            Ok(image) => image,
                            Err(error) if error.retryable() => return None,
                            Err(_) => continue,
                        };
                    return executor
                        .spawn(async move { decode_project_icon(&mime, bytes).ok() })
                        .await;
                }
                None
            };
            let media = match futures::future::select(
                Box::pin(load),
                Box::pin(executor.timer(Duration::from_secs(30))),
            )
            .await
            {
                futures::future::Either::Left((media, _)) => {
                    media.map(|media| media.for_view((16.0, 16.0), 2.0, 4096))
                }
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                this.media = media;
                cx.notify();
            });
        });
        Self {
            name,
            seed,
            media: None,
            refreshed: std::time::Instant::now(),
            _task: task,
        }
    }
}

impl Render for ProjectIcon {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.media {
            Some(media) => gpui::img(media.image.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            None => monogram(&self.name, &self.seed, false, Theme::of(cx)),
        }
    }
}

/// Same card as the pull-request badge tooltip: project name, then the
/// owning device below in the muted line.
struct ProjectIconTooltip {
    name: SharedString,
    device: SharedString,
}

impl Render for ProjectIconTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let card = div()
            .max_w(px(320.0))
            .px(px(9.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(crate::popover::surface_bg(theme))
            .when(!theme.is_frost(), |el| el.shadow_md())
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(self.name.clone()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(self.device.clone()),
            );
        crate::frost::frosted(6.0, crate::frost::MENU_BLUR, card)
    }
}

fn project_icon_frame(
    chat_id: &str,
    name: &str,
    device: &str,
    size: f32,
    child: impl IntoElement,
) -> AnyElement {
    let name: SharedString = name.to_owned().into();
    let device: SharedString = device.to_owned().into();
    div()
        .id(SharedString::from(format!("project-icon-{chat_id}")))
        .size(px(size))
        .flex_none()
        .tooltip(move |_, cx| {
            cx.new(|_| ProjectIconTooltip {
                name: name.clone(),
                device: device.clone(),
            })
            .into()
        })
        .tooltip_show_delay(Duration::from_millis(350))
        .child(child)
        .into_any_element()
}

/// A session's badge inputs, resolved where the space is already known.
/// Carrying owned data keeps the per-card path from scanning `state.chats`
/// and keeps borrowed state out of the call that needs `&mut Context`.
pub(super) struct ProjectIconRequest {
    pub(super) chat_id: String,
    name: String,
    seed: String,
    device: String,
    context: Option<FilesRequestContext>,
    source: ProjectIconSource,
}

impl ProjectIconRequest {
    /// A monogram-only request for tests that build sidebar rows by hand.
    #[cfg(test)]
    pub(super) fn monogram_only(chat_id: &str, name: &str) -> Self {
        Self {
            chat_id: chat_id.into(),
            name: name.into(),
            seed: name.into(),
            device: "Unknown device".into(),
            context: None,
            source: ProjectIconSource::WorkspaceRpc,
        }
    }

    pub(super) fn resolve(
        state: &AppState,
        chat: &zeron_proto::Chat,
        space: Option<&zeron_proto::Space>,
    ) -> Self {
        let name = space
            .map(|space| space.display_name().to_string())
            .unwrap_or_else(|| "Home".into());
        // Same fallback as the row's "@ device" fragment.
        let device = state
            .device_name(&chat.device_id)
            .unwrap_or("Unknown device")
            .to_string();
        let seed = space
            .map(|space| space.path.clone())
            .unwrap_or_else(|| "home".into());
        let source = space.map_or(ProjectIconSource::WorkspaceRpc, |space| {
            icon_source(
                state.engine().map(|engine| engine.mode()).as_ref(),
                state.local_device_id.as_deref(),
                &space.device_id,
            )
        });
        let context = space.map(|space| FilesRequestContext {
            target: zeron_proto::WorkspaceTarget {
                chat_id: None,
                space_id: Some(space.id.clone()),
                checkout_path: None,
            },
            target_device_id: (state.local_device_id.as_deref() != Some(&space.device_id))
                .then(|| space.device_id.clone()),
            cwd: space.path.clone(),
            checkout_id: space.checkout_id.clone(),
        });
        Self {
            chat_id: chat.id.clone(),
            name,
            seed,
            device,
            context,
            source,
        }
    }
}

impl Shell {
    /// The project badge for a session: the repository favicon when one is
    /// found, else a colored monogram. `size` is the square edge in px.
    pub(super) fn render_project_icon(
        &self,
        request: ProjectIconRequest,
        size: f32,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ProjectIconRequest {
            chat_id,
            name,
            seed,
            device,
            context,
            source,
        } = request;
        let Some(context) = context else {
            return project_icon_frame(
                &chat_id,
                &name,
                &device,
                size,
                monogram(&name, &seed, selected, Theme::of(cx)),
            );
        };
        let (engine, key) = {
            let state = self.state.read(cx);
            // The engine's device is part of the cache identity: two remote
            // profiles under the same account can otherwise reuse each
            // other's badge when both spaces route to the engine's own
            // device (`None`).
            let key = format!(
                "{:?}:{:?}:{:?}:{:?}:{}:{:?}:{}",
                profile_key(state, self.boot.org_id.as_deref()),
                source,
                state.local_device_id,
                context.target_device_id,
                context.cwd,
                context.checkout_id,
                name
            );
            (state.engine().cloned(), key)
        };
        // Don't cache a remote miss before a connection exists.
        if source == ProjectIconSource::WorkspaceRpc && engine.is_none() {
            return project_icon_frame(
                &chat_id,
                &name,
                &device,
                size,
                monogram(&name, &seed, selected, Theme::of(cx)),
            );
        }
        let mut cache = self.project_icons.borrow_mut();
        // A stale entry is refreshed on its own lookup; the sweep only drops
        // projects that no longer render, so it can run at most once per
        // interval instead of once per card.
        if self.project_icons_pruned.get().elapsed() >= ICON_PRUNE_INTERVAL {
            self.project_icons_pruned.set(std::time::Instant::now());
            cache.retain(|_, entity| entity.read(cx).refreshed.elapsed() < ICON_TTL);
        }
        if cache
            .get(&key)
            .is_some_and(|entity| entity.read(cx).refreshed.elapsed() >= ICON_TTL)
        {
            cache.remove(&key);
        }
        let entity = cache
            .entry(key)
            .or_insert_with(|| {
                let entity = cx.new(|cx| {
                    ProjectIcon::new(name.clone(), seed.clone(), context, source, engine, cx)
                });
                // The monogram below is drawn by the shell (it needs the row's
                // selected state, which the shared entity can't hold), so the
                // shell must redraw when artwork lands.
                cx.observe(&entity, |_, _, cx| cx.notify()).detach();
                entity
            })
            .clone();
        drop(cache);
        if entity.read(cx).media.is_none() {
            return project_icon_frame(
                &chat_id,
                &name,
                &device,
                size,
                monogram(&name, &seed, selected, Theme::of(cx)),
            );
        }
        project_icon_frame(&chat_id, &name, &device, size, entity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_in_process_engine_on_its_own_device_reads_local_disk() {
        let remote = EngineMode::Remote {
            url: "ws://127.0.0.1:1".into(),
        };
        assert_eq!(
            icon_source(Some(&EngineMode::InProcess), Some("studio"), "studio"),
            ProjectIconSource::LocalFs
        );
        assert_eq!(
            icon_source(Some(&remote), Some("studio"), "studio"),
            ProjectIconSource::WorkspaceRpc
        );
        assert_eq!(
            icon_source(Some(&EngineMode::InProcess), Some("studio"), "server"),
            ProjectIconSource::WorkspaceRpc
        );
        assert_eq!(
            icon_source(None, None, "server"),
            ProjectIconSource::WorkspaceRpc
        );
    }
    fn png(path: &std::path::Path, width: u32) {
        image::RgbaImage::new(width, 2).save(path).unwrap();
    }
    #[test]
    fn sidebar_project_icon_priority_and_missing_fallback() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load_local_icon(temp.path()).is_none());
        png(&temp.path().join("favicon.png"), 3);
        assert_eq!(load_local_icon(temp.path()).unwrap().width, 3.0);
        std::fs::create_dir(temp.path().join("public")).unwrap();
        png(&temp.path().join("public/apple-touch-icon.png"), 7);
        assert_eq!(load_local_icon(temp.path()).unwrap().width, 7.0);
        std::fs::write(temp.path().join(".git"), "gitdir: /unused-test-checkout").unwrap();
        std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        assert_eq!(
            load_local_icon(&temp.path().join("src/nested"))
                .unwrap()
                .width,
            7.0
        );
        // A corrupt first match uses the default rather than showing unrelated lower-priority art.
        std::fs::write(temp.path().join("public/apple-touch-icon.png"), b"invalid").unwrap();
        assert!(load_local_icon(temp.path()).is_none());
    }
    #[test]
    fn sidebar_project_icons_support_svg_and_ico() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("favicon.svg"), br##"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="8"><rect width="12" height="8" fill="#f00"/></svg>"##).unwrap();
        assert_eq!(load_local_icon(temp.path()).unwrap().width, 12.0);
        std::fs::remove_file(temp.path().join("favicon.svg")).unwrap();
        image::RgbaImage::new(16, 16)
            .save(temp.path().join("favicon.ico"))
            .unwrap();
        assert_eq!(load_local_icon(temp.path()).unwrap().width, 16.0);
    }
}
