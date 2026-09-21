//! Bounded, deadline-limited writes to the Linux browser helper. The UI only
//! queues commands; a helper that stops reading cannot block its event loop.
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const QUEUE_BYTES: usize = 1024 * 1024;
const QUEUE_COMMANDS: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

struct Command {
    bytes: Vec<u8>,
    queued: Arc<AtomicUsize>,
}
impl Drop for Command {
    fn drop(&mut self) {
        self.queued.fetch_sub(self.bytes.len(), Ordering::AcqRel);
    }
}

pub(super) struct CommandWriter {
    tx: mpsc::SyncSender<Command>,
    queued: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    on_failure: Arc<dyn Fn() + Send + Sync>,
}
impl CommandWriter {
    pub(super) fn new(
        mut pipe: impl Write + AsRawFd + Send + 'static,
        on_failure: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let fd = pipe.as_raw_fd();
        // SAFETY: fd belongs to pipe, which remains alive on the writer thread.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let (tx, rx) = mpsc::sync_channel::<Command>(QUEUE_COMMANDS);
        let queued = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let on_failure: Arc<dyn Fn() + Send + Sync> = Arc::new(on_failure);
        let writer_failed = failed.clone();
        let failure = on_failure.clone();
        std::thread::Builder::new()
            .name("browser-commands".into())
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    if writer_failed.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(error) = write_deadline(&mut pipe, &command.bytes, WRITE_TIMEOUT) {
                        tracing::warn!(%error, "browser helper stopped accepting commands");
                        if !writer_failed.swap(true, Ordering::AcqRel) {
                            failure();
                        }
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            queued,
            failed,
            on_failure,
        })
    }

    pub(super) fn send(&self, data: Vec<u8>) -> Result<(), String> {
        if self.failed.load(Ordering::Acquire) {
            return Err("Browser helper is unavailable".into());
        }
        let size = data
            .len()
            .checked_add(4)
            .filter(|size| *size <= QUEUE_BYTES);
        let admitted = size.is_some_and(|size| {
            self.queued
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    queued
                        .checked_add(size)
                        .filter(|total| *total <= QUEUE_BYTES)
                })
                .is_ok()
        });
        if !admitted {
            return self.fail();
        }
        let mut bytes = Vec::with_capacity(size.unwrap());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        if self
            .tx
            .try_send(Command {
                bytes,
                queued: self.queued.clone(),
            })
            .is_err()
        {
            return self.fail();
        }
        Ok(())
    }

    fn fail(&self) -> Result<(), String> {
        if !self.failed.swap(true, Ordering::AcqRel) {
            (self.on_failure)();
        }
        Err("Browser helper stopped accepting input; reopen the tab".into())
    }
}

fn write_deadline(
    pipe: &mut (impl Write + AsRawFd),
    mut bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match pipe.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut fd = libc::pollfd {
                    fd: pipe.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: poll borrows one valid pollfd; pipe owns its descriptor.
                let result = unsafe {
                    libc::poll(
                        &mut fd,
                        1,
                        remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                    )
                };
                if result == 0 {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    #[test]
    fn queued_commands_preserve_framing_and_order() {
        let (pipe, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let writer = CommandWriter::new(pipe, || panic!("healthy writer failed")).unwrap();
        for value in [b"first".as_slice(), b"second".as_slice()] {
            writer.send(value.to_vec()).unwrap();
        }
        for expected in [b"first".as_slice(), b"second".as_slice()] {
            let mut length = [0; 4];
            reader.read_exact(&mut length).unwrap();
            let mut data = vec![0; u32::from_le_bytes(length) as usize];
            reader.read_exact(&mut data).unwrap();
            assert_eq!(data, expected);
        }
    }

    #[test]
    fn stalled_reader_has_a_deadline() {
        let (mut pipe, _reader) = UnixStream::pair().unwrap();
        pipe.set_nonblocking(true).unwrap();
        let start = Instant::now();
        let error = write_deadline(
            &mut pipe,
            &vec![0; 8 * 1024 * 1024],
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn paused_helper_saturates_a_bounded_queue_without_blocking_send() {
        let (pipe, _reader) = UnixStream::pair().unwrap();
        let writer = CommandWriter::new(pipe, || {}).unwrap();
        let start = Instant::now();
        let mut rejected = false;
        for _ in 0..128 {
            if writer.send(vec![0; 64 * 1024]).is_err() {
                rejected = true;
                break;
            }
            assert!(writer.queued.load(Ordering::Acquire) <= QUEUE_BYTES);
        }
        assert!(
            rejected,
            "a paused helper must eventually exhaust the budget"
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn queue_overflow_fails_once_without_waiting_for_the_helper() {
        let (pipe, _reader) = UnixStream::pair().unwrap();
        let failures = Arc::new(AtomicUsize::new(0));
        let count = failures.clone();
        let writer = CommandWriter::new(pipe, move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let start = Instant::now();
        assert!(writer.send(vec![0; QUEUE_BYTES]).is_err());
        assert!(writer.send(vec![0]).is_err());
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
