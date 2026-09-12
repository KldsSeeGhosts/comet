use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, Visitor},
};

use crate::{LayoutError, MAX_PANES, Result, SplitNode, ViewId, ViewLayout, WorkspaceLayout};

pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdPolicy {
    Reject,
    /// Raise a stale next_id to max(live IDs) + 1. Historical deleted IDs cannot
    /// be recovered from a damaged file, so strict loading is the default.
    Repair,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLayout {
    root: SplitNode<ViewId>,
    active_view_id: ViewId,
    #[serde(deserialize_with = "unique_map")]
    views: BTreeMap<ViewId, ViewLayout>,
    next_id: u64,
    revision: u64,
}

impl WireLayout {
    fn into_layout(self, policy: IdPolicy) -> Result<WorkspaceLayout> {
        let mut layout = WorkspaceLayout {
            root: self.root,
            active_view_id: self.active_view_id,
            views: self.views,
            next_id: self.next_id,
            revision: self.revision,
        };
        if policy == IdPolicy::Repair {
            let minimum = layout
                .ids()
                .last()
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(LayoutError::Exhausted)?;
            layout.next_id = layout.next_id.max(minimum);
        }
        layout.validate()?;
        Ok(layout)
    }
}

impl<'de> Deserialize<'de> for WorkspaceLayout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        WireLayout::deserialize(deserializer)?
            .into_layout(IdPolicy::Reject)
            .map_err(de::Error::custom)
    }
}

pub(crate) fn unique_map<'de, D, K, V>(
    deserializer: D,
) -> std::result::Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct UniqueMap<K, V>(PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> Visitor<'de> for UniqueMap<K, V> {
        type Value = BTreeMap<K, V>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object with unique ID keys")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut access: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut map = BTreeMap::new();
            while let Some(key) = access.next_key()? {
                if map.contains_key(&key) {
                    return Err(de::Error::custom("duplicate ID key"));
                }
                if map.len() >= MAX_PANES {
                    return Err(de::Error::custom("map entry limit exceeded"));
                }
                map.insert(key, access.next_value()?);
            }
            Ok(map)
        }
    }
    deserializer.deserialize_map(UniqueMap(PhantomData))
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl WorkspaceLayout {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with_policy(path, IdPolicy::Reject)
    }

    pub fn load_with_policy(path: impl AsRef<Path>, policy: IdPolicy) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(LayoutError::Limit("file size"));
        }
        serde_json::from_slice::<WireLayout>(&bytes)?.into_layout(policy)
    }

    /// Write a sibling temporary file, sync it, then rename over the destination.
    /// The parent directory must exist. Concurrent writers use distinct temp files;
    /// the revision guard is in-memory, not an interprocess lock.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(LayoutError::Limit("file size"));
        }
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or(LayoutError::Invalid("save path has no filename"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        for _ in 0..32 {
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(
                ".{}.{}.tmp",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let temporary_path = parent.join(temporary_name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = match options.open(&temporary_path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let temporary = Temporary(temporary_path);
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary.0, path)?;
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a temporary layout file",
        )
        .into())
    }
}
