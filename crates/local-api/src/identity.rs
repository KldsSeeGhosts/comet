use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::PROTOCOL_VERSION;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SocketIdentity {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub protocol_version: u32,
    pub instance_id: Uuid,
    pub instance_name: String,
    pub pid: u32,
    pub socket: SocketIdentity,
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn uid() -> u32 {
    // geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub(crate) fn private_dir(path: &Path, create: bool) -> Result<PathBuf> {
    if create {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "not a real directory: {}",
        path.display()
    );
    ensure!(
        meta.uid() == uid() && meta.mode() & 0o777 == 0o700,
        "API directory must be owned by this user with mode 0700: {}",
        path.display()
    );
    Ok(fs::canonicalize(path)?)
}

pub(crate) fn socket_identity(path: &Path) -> Result<SocketIdentity> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.file_type().is_socket(),
        "not a Unix socket: {}",
        path.display()
    );
    ensure!(
        meta.uid() == uid() && meta.mode() & 0o777 == 0o600,
        "socket must be owned by this user with mode 0600: {}",
        path.display()
    );
    Ok(SocketIdentity {
        path: path.to_owned(),
        device: meta.dev(),
        inode: meta.ino(),
        uid: meta.uid(),
    })
}

pub(crate) fn read_manifest(path: &Path) -> Result<Manifest> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == uid() && meta.mode() & 0o777 == 0o600,
        "manifest must be a private regular file: {}",
        path.display()
    );
    ensure!(meta.len() <= 65536, "manifest is too large");
    let manifest: Manifest = serde_json::from_reader(file)?;
    ensure!(
        manifest.protocol_version == PROTOCOL_VERSION
            && valid_name(&manifest.instance_name)
            && !manifest.instance_id.is_nil()
            && manifest.pid > 0
            && manifest.pid <= i32::MAX as u32,
        "invalid instance manifest: {}",
        path.display()
    );
    let parent = path.parent().context("manifest has no parent")?;
    ensure!(
        path == parent.join(format!("{}.json", manifest.instance_name))
            && manifest.socket.path == parent.join(format!("{}.sock", manifest.instance_name))
            && manifest.socket.uid == uid(),
        "manifest path identity mismatch"
    );
    Ok(manifest)
}

fn pid_definitely_dead(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // Signal zero checks existence only. EPERM and PID reuse both mean refusal.
    (unsafe { libc::kill(pid as libc::pid_t, 0) == -1 })
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// The lock file is permanent. Unlinking it would allow two different flock inodes.
pub(crate) struct Registration {
    pub manifest: Manifest,
    manifest_path: PathBuf,
    _lock: File,
}

impl Registration {
    pub async fn bind(data_dir: &Path, name: String) -> Result<(Self, UnixListener)> {
        ensure!(
            valid_name(&name),
            "instance name must be 1-48 ASCII letters, digits, underscores or hyphens"
        );
        let dir = private_dir(&data_dir.join("local-api"), true)?;
        let socket = dir.join(format!("{name}.sock"));
        let manifest_path = dir.join(format!("{name}.json"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(format!("{name}.lock")))?;
        let meta = lock.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == uid() && meta.mode() & 0o777 == 0o600,
            "unsafe instance lock"
        );
        // The open file stays alive for the registration's whole lifetime.
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "instance {name} is locked by another process"
        );

        let socket_exists = match fs::symlink_metadata(&socket) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        let old = match fs::symlink_metadata(&manifest_path) {
            Ok(_) => Some(read_manifest(&manifest_path)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if socket_exists {
            let old = old
                .as_ref()
                .context("refusing to remove socket without an identity manifest")?;
            ensure!(
                socket_identity(&socket)? == old.socket,
                "refusing to remove socket with mismatched manifest identity"
            );
            // An HTTP response, malformed response, timeout, or live listener is never stale proof.
            let health = crate::client::raw_health(&socket).await;
            ensure!(
                health.is_err(),
                "instance already responds to health checks"
            );
            match tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(&socket)).await {
                Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {}
                _ => bail!("refusing to remove socket: listener death is not proven"),
            }
            ensure!(
                pid_definitely_dead(old.pid),
                "refusing to remove socket: manifest PID is alive or uncertain"
            );
            ensure!(
                read_manifest(&manifest_path)? == *old && socket_identity(&socket)? == old.socket,
                "instance identity changed during stale check"
            );
            fs::remove_file(&socket)?;
        } else if let Some(old) = &old {
            ensure!(
                pid_definitely_dead(old.pid),
                "refusing to replace manifest: PID is alive or uncertain"
            );
        }
        if old.is_some() {
            fs::remove_file(&manifest_path)?;
        }

        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&socket);
            return Err(error.into());
        }
        let registration = Self {
            manifest: Manifest {
                protocol_version: PROTOCOL_VERSION,
                instance_id: Uuid::new_v4(),
                instance_name: name,
                pid: std::process::id(),
                socket: socket_identity(&socket)?,
            },
            manifest_path,
            _lock: lock,
        };
        let temporary = dir.join(format!(".{}.tmp", registration.manifest.instance_id));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, &registration.manifest)?;
            file.sync_all()?;
            // Publish atomically without overwriting an unexpected new manifest.
            fs::hard_link(&temporary, &registration.manifest_path)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result?;
        Ok((registration, listener))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Never unlink a replacement socket or another instance's manifest.
        if socket_identity(&self.manifest.socket.path).ok().as_ref() == Some(&self.manifest.socket)
        {
            let _ = fs::remove_file(&self.manifest.socket.path);
        }
        if read_manifest(&self.manifest_path).ok().as_ref() == Some(&self.manifest) {
            let _ = fs::remove_file(&self.manifest_path);
        }
    }
}
