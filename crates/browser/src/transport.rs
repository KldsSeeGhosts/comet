//! Length-bounded, one-request-per-connection Unix IPC. Filesystem permissions
//! protect it from other users; it intentionally trusts processes of this uid.
use crate::{Reply, ReplySender, Request};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_REPLY: usize = 24 * 1024 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(20);
pub struct Pending {
    pub request: Request,
    pub reply: ReplySender,
}
pub struct Server {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn read_line(reader: &mut impl BufRead, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.len() > limit || !bytes.ends_with(b"\n") {
        return Err(std::io::Error::other(
            "Browser message is too large or incomplete",
        ));
    }
    Ok(bytes)
}
pub fn connect(path: &Path, request: &Request) -> Reply {
    let mut stream = UnixStream::connect(path).map_err(|e| {
        format!("Noches browser is unavailable. Open the desktop app on this device: {e}")
    })?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut bytes = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    if bytes.len() >= MAX_REQUEST {
        return Err("Browser request is too large".into());
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).map_err(|e| e.to_string())?;
    let bytes = read_line(&mut BufReader::new(stream), MAX_REPLY).map_err(|e| e.to_string())?;
    serde_json::from_slice::<Reply>(&bytes).map_err(|e| e.to_string())?
}
pub fn bind(path: &Path) -> std::io::Result<(Server, tokio::sync::mpsc::Receiver<Pending>)> {
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Missing socket directory"))?;
    match std::fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // Check ownership against a file we create in the same private directory.
    // symlink_metadata also prevents following a substituted directory symlink.
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.mode() & 0o777 != 0o700 {
        return Err(std::io::Error::other(
            "Browser socket directory must be private (0700)",
        ));
    }
    // Bind first. Never unlink a live owner's socket. Stale files may only be
    // removed inside the private directory after a refused connection.
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path) {
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                }
                _ => return Err(e),
            }
            UnixListener::bind(path)?
        }
        Err(e) => return Err(e),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    if std::fs::metadata(path)?.uid() != metadata.uid() {
        return Err(std::io::Error::other("Browser socket ownership mismatch"));
    }
    listener.set_nonblocking(true)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Pending>(16);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    std::thread::Builder::new()
        .name("browser-control".into())
        .spawn(move || {
            // Different conversations must not queue behind a slow page.
            let active = Arc::new(AtomicUsize::new(0));
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                if active.load(Ordering::Acquire) >= 16 {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    let _ = stream.write_all(
                        b"{\"Err\":\"Browser is busy; retry after inspecting the page\"}\n",
                    );
                    continue;
                }
                active.fetch_add(1, Ordering::AcqRel);
                let active = active.clone();
                let tx = tx.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    serve_connection(stream, tx, stop);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            }
        })?;
    Ok((
        Server {
            path: path.into(),
            stopped,
        },
        rx,
    ))
}

fn serve_connection(
    mut stream: UnixStream,
    tx: tokio::sync::mpsc::Sender<Pending>,
    stop: Arc<AtomicBool>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let result = (|| -> Reply {
        let bytes =
            read_line(&mut BufReader::new(&mut stream), MAX_REQUEST).map_err(|e| e.to_string())?;
        let request: Request = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let (reply, mut answer) = tokio::sync::oneshot::channel();
        tx.try_send(Pending { request, reply })
            .map_err(|_| "Browser is busy or closing".to_string())?;
        let start = std::time::Instant::now();
        loop {
            match answer.try_recv() {
                Ok(reply) => return reply,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    return Err("Browser tab closed before the action completed".into());
                }
                Err(_) => {}
            }
            if stop.load(Ordering::Acquire) || start.elapsed() > Duration::from_secs(15) {
                return Err("Browser action timed out; inspect the page before retrying".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    let result = serde_json::to_vec(&result).unwrap_or_default();
    if result.len() < MAX_REPLY {
        let _ = stream.write_all(&result);
    } else {
        let _ = stream.write_all(b"{\"Err\":\"Browser reply is too large\"}");
    }
    let _ = stream.write_all(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;
    #[test]
    fn round_trip_real_socket_preserves_session_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::socket_path(dir.path());
        let (server, mut requests) = bind(&path).unwrap();
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            connect(
                &client_path,
                &Request {
                    session: "session-a".into(),
                    action: Action::Snapshot { tab: 4 },
                },
            )
        });
        let pending = requests.blocking_recv().expect("request arrives");
        assert_eq!(pending.request.session, "session-a");
        assert_eq!(pending.request.action.tab(), Some(4));
        pending.reply.send(Err("Tab closed".into())).unwrap();
        assert_eq!(client.join().unwrap(), Err("Tab closed".into()));
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn slow_page_does_not_block_another_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::socket_path(dir.path());
        let (_server, mut requests) = bind(&path).unwrap();
        let slow_path = path.clone();
        let slow = std::thread::spawn(move || {
            connect(
                &slow_path,
                &Request {
                    session: "slow".into(),
                    action: Action::Snapshot { tab: 1 },
                },
            )
        });
        let waiting = requests.blocking_recv().unwrap();
        let fast = std::thread::spawn(move || {
            connect(
                &path,
                &Request {
                    session: "fast".into(),
                    action: Action::Tabs,
                },
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let next = loop {
            if let Ok(request) = requests.try_recv() {
                break request;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "another conversation was blocked by the slow page"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(next.request.session, "fast");
        next.reply.send(Ok(serde_json::json!({"tabs":[]}))).unwrap();
        assert!(fast.join().unwrap().is_ok());
        waiting.reply.send(Err("timeout".into())).unwrap();
        assert!(slow.join().unwrap().is_err());
    }
    #[test]
    fn live_socket_cannot_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::socket_path(dir.path());
        let (_server, _rx) = bind(&path).unwrap();
        assert!(bind(&path).is_err());
        assert!(path.exists());
    }
    #[test]
    fn reject_oversized_and_partial_messages() {
        assert!(read_line(&mut std::io::Cursor::new(b"12345\n"), 4).is_err());
        assert!(read_line(&mut std::io::Cursor::new(b"123"), 4).is_err());
        assert_eq!(
            read_line(&mut std::io::Cursor::new(b"123\n"), 4).unwrap(),
            b"123\n"
        );
    }
    #[test]
    fn reject_public_socket_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bind(&dir.path().join("control.sock")).is_err());
    }
}
