//! Per-space workspace layout persistence (WS5 of the split-pane workspace
//! feature): `{data_dir}/workspace-layout.json`, keyed by space id — the
//! Noches analog of Super's per-worktree `split_layouts` restore.
//!
//! Structure mirrors [`crate::settings::SettingsStore`] (own file, corrupt
//! fallback, atomic temp+rename writes, debounced saves) but is NOT a global:
//! the store is owned by the [`crate::shell::Shell`] as a plain field. The
//! layout values are serialized with the `zeron-workspace` engine's own serde
//! impls — [`WorkspaceLayout`] serializes directly and deserializes through
//! the engine's validated wire format, so a stored layout is never structurally
//! invalid (ids consistent, limits respected) and hand-edits cannot wedge the
//! tree.
//!
//! Terminal panes restore as their placeholder body and session-bound panes
//! as dormant identity cards: PTYs never reattach and `messages_snapshot`
//! embedding is deliberately deferred (chat content lives in CRDT docs).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use zeron_workspace::WorkspaceLayout;

/// Store file name inside the data dir.
pub const FILE_NAME: &str = "workspace-layout.json";
/// Schema version. A file carrying a different (older or newer) version is
/// discarded rather than guessed at; the next save rewrites version 1.
pub const VERSION: u32 = 1;
/// Debounce for layout writes after a mutation (mirrors ui-settings).
pub const SAVE_DEBOUNCE_MS: u64 = 400;
/// A persisted file larger than this is treated as corrupt (same spirit as
/// the engine's own layout-file cap).
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Reserved key for the projectless canvas (`selected_space == None`). Space
/// ids are engine-minted UUIDs (`spaces.rs`), so this cannot collide.
const NO_SPACE_KEY: &str = "__no_space__";

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    layouts: BTreeMap<String, WorkspaceLayout>,
}

/// In-memory owner of the persisted layouts. Mutations land in `layouts`
/// before any timer starts (the Shell arms the debounce); `revision`/
/// `saved_revision` follow the settings-store pattern so a failing write
/// leaves the data pending instead of lost.
pub struct WorkspaceLayoutStore {
    layouts: BTreeMap<String, WorkspaceLayout>,
    data_dir: PathBuf,
    revision: u64,
    saved_revision: u64,
}

/// The store key for a space selection (`None` = projectless canvas).
fn key_for(space: Option<&str>) -> String {
    space.unwrap_or(NO_SPACE_KEY).to_string()
}

/// The space selection a store key encodes.
fn key_space(key: &str) -> Option<&str> {
    (key != NO_SPACE_KEY).then_some(key)
}

impl WorkspaceLayoutStore {
    /// Load from `{data_dir}/workspace-layout.json`; empty on any failure
    /// (missing, corrupt, wrong version). The file itself is left untouched —
    /// the next successful save heals it.
    pub fn load(data_dir: &Path) -> Self {
        let layouts = match read_capped(&Self::path(data_dir)) {
            Ok(bytes) => match Self::parse(&bytes) {
                Ok(layouts) => layouts,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        path = %Self::path(data_dir).display(),
                        "workspace-layout corrupt; starting empty"
                    );
                    BTreeMap::new()
                }
            },
            // A missing file is every first boot; only read failures above
            // warn, so keep this quiet.
            Err(_) => BTreeMap::new(),
        };
        Self {
            layouts,
            data_dir: data_dir.into(),
            revision: 0,
            saved_revision: 0,
        }
    }

    /// Parse and validate a store file. The layout VALUES go through the
    /// engine's validated `Deserialize`, so a structurally impossible tree
    /// (dangling id, bad ratio, duplicate key) is rejected here, not at
    /// restore time.
    fn parse(bytes: &[u8]) -> Result<BTreeMap<String, WorkspaceLayout>, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(VERSION)) {
            return Err(serde_json::Error::custom(
                "unsupported workspace-layout version",
            ));
        }
        let file: StoreFile = serde_json::from_value(value)?;
        Ok(file.layouts)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// The stored layout for a space selection, if any.
    pub fn layout_for(&self, space: Option<&str>) -> Option<WorkspaceLayout> {
        self.layouts.get(&key_for(space)).cloned()
    }

    /// Find a saved tree that owns a session, preferring its native project
    /// when more than one project has docked the same session.
    pub fn space_for_session(
        &self,
        session_id: &str,
        preferred: Option<&str>,
    ) -> Option<Option<String>> {
        let contains = |layout: &WorkspaceLayout| {
            layout.views.values().any(|view| {
                view.tabs.values().any(|tab| {
                    tab.panes.values().any(|pane| pane.session_id.as_deref() == Some(session_id))
                })
            })
        };
        if let Some(layout) = self.layouts.get(&key_for(preferred))
            && contains(layout)
        {
            return Some(preferred.map(str::to_owned));
        }
        self.layouts.iter().find_map(|(key, layout)| {
            contains(layout).then(|| key_space(key).map(str::to_owned))
        })
    }

    /// Replace the stored layout for a space selection. A no-op when the
    /// layout is byte-equal to what is stored (keeps idle flushes from
    /// rewriting the file).
    pub fn set_layout(&mut self, space: Option<&str>, layout: WorkspaceLayout) {
        let key = key_for(space);
        if self.layouts.get(&key) == Some(&layout) {
            return;
        }
        self.layouts.insert(key, layout);
        self.revision += 1;
    }

    /// Drop entries for spaces failing `keep` — space deletion (here or on
    /// another device) must not leave its layout behind. Returns how many
    /// entries were removed.
    pub fn retain_spaces(&mut self, keep: impl Fn(Option<&str>) -> bool) -> usize {
        let before = self.layouts.len();
        self.layouts.retain(|key, _| keep(key_space(key)));
        let removed = before - self.layouts.len();
        if removed > 0 {
            self.revision += 1;
        }
        removed
    }

    /// Whether unflushed changes exist.
    pub fn needs_save(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// Write atomically (temp file + rename) so a crash mid-write never
    /// corrupts. No-op when everything is already saved. A failure leaves
    /// `needs_save()` true, so the next flush retries.
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.needs_save() {
            return Ok(());
        }
        let file = StoreFile {
            version: VERSION,
            layouts: self.layouts.clone(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::create_dir_all(&self.data_dir)?;
        let path = Self::path(&self.data_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        self.saved_revision = self.revision;
        Ok(())
    }
}

/// Read at most [`MAX_FILE_BYTES`] bytes; anything larger is a corrupt file.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace-layout file exceeds the size cap",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_workspace::{Direction, PaneId, ViewId};

    /// A non-trivial layout built through the engine's public ops.
    fn sample_layout() -> WorkspaceLayout {
        let mut layout = WorkspaceLayout::new();
        layout.split_view(ViewId(1), Direction::Right, Default::default()).unwrap();
        layout.split_pane(PaneId(3), Direction::Down, Default::default()).unwrap();
        layout.set_view_ratio(&[], 0.72).unwrap();
        layout.validate().unwrap();
        layout
    }

    #[test]
    fn round_trips_layouts_per_space_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkspaceLayoutStore::load(dir.path());
        let a = sample_layout();
        let mut b = WorkspaceLayout::new();
        b.split_pane(PaneId(3), Direction::Right, Default::default()).unwrap();
        let none = WorkspaceLayout::new();
        store.set_layout(Some("space-a"), a.clone());
        store.set_layout(Some("space-b"), b.clone());
        store.set_layout(None, none.clone());
        store.set_layout(Some("gone"), WorkspaceLayout::new());
        store.flush().unwrap();
        assert!(!store.needs_save());

        let reloaded = WorkspaceLayoutStore::load(dir.path());
        assert_eq!(reloaded.layout_for(Some("space-a")).as_ref(), Some(&a));
        assert_eq!(reloaded.layout_for(Some("space-b")).as_ref(), Some(&b));
        assert_eq!(reloaded.layout_for(None).as_ref(), Some(&none));
        assert_eq!(
            reloaded.layout_for(Some("missing")),
            None,
            "an unknown space reads as no layout, not the default"
        );
    }

    #[test]
    fn flush_is_a_no_op_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkspaceLayoutStore::load(dir.path());
        assert!(!store.needs_save());
        store.flush().unwrap();
        assert!(!WorkspaceLayoutStore::path(dir.path()).exists());
        // An identical set does not dirty the store either.
        let layout = WorkspaceLayout::new();
        store.set_layout(Some("s"), layout.clone());
        store.set_layout(Some("s"), layout);
        assert_eq!(store.revision, 1);
        store.flush().unwrap();
        let written = std::fs::read_to_string(WorkspaceLayoutStore::path(dir.path())).unwrap();
        assert!(written.contains("\"version\": 1"));
    }

    #[test]
    fn corrupt_file_falls_back_to_empty_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            WorkspaceLayoutStore::path(dir.path()),
            r#"{"version":1 "layouts": {"a": TRUNCATED"#,
        )
        .unwrap();
        let store = WorkspaceLayoutStore::load(dir.path());
        assert_eq!(store.layout_for(Some("a")), None);
        assert!(!store.needs_save(), "a corrupt load must not look like a change");
        // The damaged file survives until a real change rewrites it.
        assert!(WorkspaceLayoutStore::path(dir.path()).exists());
    }

    #[test]
    fn structurally_invalid_layouts_are_rejected_by_the_engine_serde() {
        let dir = tempfile::tempdir().unwrap();
        // next_id below the live ids: the engine's validated Deserialize
        // refuses it, so the whole file degrades to empty instead of
        // restoring a broken tree. (`sample_layout` minted ids 4..=7, so its
        // serialized next_id is 8.)
        let json = serde_json::to_string(&sample_layout())
            .unwrap()
            .replace("\"next_id\":8", "\"next_id\":1");
        assert!(json.contains("\"next_id\":1"), "repair target missing");
        std::fs::write(
            WorkspaceLayoutStore::path(dir.path()),
            format!(r#"{{"version":1,"layouts":{{"s":{json}}}}}"#),
        )
        .unwrap();
        let store = WorkspaceLayoutStore::load(dir.path());
        assert_eq!(store.layout_for(Some("s")), None);
    }

    #[test]
    fn wrong_or_missing_version_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("newer", r#"{"version":2,"layouts":{}}"#),
            ("missing", r#"{"layouts":{}}"#),
            ("array", r#"[1,2,3]"#),
        ] {
            std::fs::write(WorkspaceLayoutStore::path(dir.path()), text).unwrap();
            let store = WorkspaceLayoutStore::load(dir.path());
            assert_eq!(store.layout_for(None), None, "{name}");
        }
    }

    #[test]
    fn retain_spaces_drops_deleted_spaces_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkspaceLayoutStore::load(dir.path());
        store.set_layout(Some("kept"), WorkspaceLayout::new());
        store.set_layout(Some("deleted"), WorkspaceLayout::new());
        store.set_layout(None, WorkspaceLayout::new());
        let removed = store.retain_spaces(|space| match space {
            None => true,
            Some(id) => id != "deleted",
        });
        assert_eq!(removed, 1);
        assert!(store.layout_for(Some("kept")).is_some());
        assert!(store.layout_for(None).is_some());
        assert_eq!(store.layout_for(Some("deleted")), None);
        assert!(store.needs_save());
        store.flush().unwrap();
        let reloaded = WorkspaceLayoutStore::load(dir.path());
        assert_eq!(reloaded.layout_for(Some("deleted")), None);
    }

    #[test]
    fn oversize_file_is_treated_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "x".repeat((MAX_FILE_BYTES + 2) as usize);
        std::fs::write(WorkspaceLayoutStore::path(dir.path()), blob).unwrap();
        let store = WorkspaceLayoutStore::load(dir.path());
        assert_eq!(store.layout_for(None), None);
    }
}
