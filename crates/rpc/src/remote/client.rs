use super::{encode, ConnectionProfile, Frame, PROTOCOL};
use crate::{ClientFrame, RpcClient, RpcError, ServerFrame};
use futures::{SinkExt, StreamExt};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    time::timeout,
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Unauthorized,
}

pub struct RemoteClient {
    pub client: Arc<RpcClient>,
    pub status: watch::Receiver<ConnectionState>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}

impl RemoteClient {
    /// Release the retained gateway session without stopping the host engine.
    pub async fn shutdown(&self) {
        self.shutdown.send_replace(true);
        let mut stopped = self.stopped.clone();
        let _ = timeout(Duration::from_secs(2), async move {
            while !*stopped.borrow_and_update() {
                if stopped.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

/// The RPC client survives physical socket loss. Unacknowledged requests replay
/// only into the same server session; a lost server session fails pending calls.
/// Streaming consumers can then resubscribe for a fresh engine snapshot.
///
/// The wrapper bounds the transport's life: dropping it — or calling
/// [`RemoteClient::shutdown`] — ends the reconnect loop, so clones of
/// [`RemoteClient::client`] fail every later call promptly instead of dialing a
/// session nobody can observe.
pub fn connect(profile: ConnectionProfile) -> Result<RemoteClient, RpcError> {
    profile
        .validate()
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    let (out, outgoing) = mpsc::channel(256);
    let (incoming, rx) = mpsc::channel(256);
    let (status, status_rx) = watch::channel(ConnectionState::Connecting);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (stopped_tx, stopped) = watch::channel(false);
    tokio::spawn(async move {
        run(profile, outgoing, incoming, status, shutdown_rx).await;
        stopped_tx.send_replace(true);
    });
    Ok(RemoteClient {
        client: Arc::new(RpcClient::new(out, rx)),
        status: status_rx,
        shutdown,
        stopped,
    })
}

/// Terminal ends for an attempt, reported as `Ok` so `run` stops instead of
/// redialing. Every retryable end arrives as an `Err` and takes the backoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// The wrapped `RpcClient` — or its last clone — is gone: nothing left to
    /// serve, so a new session would only leak.
    ClientGone,
    /// `RemoteClient::shutdown` was called, or the wrapper was dropped (the
    /// `shutdown` sender is the transport's only owner). The retained gateway
    /// session is released while the socket is still up.
    Stopped,
}

struct Replay {
    session: String,
    established: bool,
    cursor: u64,
    next: u64,
    outbox: VecDeque<(u64, String, bool)>,
    partial: String,
    pending: HashMap<u64, ()>,
    bytes: usize,
}
impl Replay {
    fn new() -> Self {
        Self {
            session: uuid::Uuid::new_v4().to_string(),
            established: false,
            cursor: 0,
            next: 0,
            partial: String::new(),
            outbox: VecDeque::new(),
            pending: HashMap::new(),
            bytes: 0,
        }
    }
    fn acknowledge(&mut self, seq: u64) {
        while self.outbox.front().is_some_and(|(n, _, _)| *n <= seq) {
            let (_, data, _) = self.outbox.pop_front().unwrap();
            self.bytes -= data.len();
        }
    }
    fn queue(&mut self, payload: String) -> Result<u64, RpcError> {
        let frame: ClientFrame =
            serde_json::from_str(&payload).map_err(|e| RpcError::Transport(e.to_string()))?;
        if frame.cancel {
            self.pending.remove(&frame.id);
        } else {
            self.pending.insert(frame.id, ());
        }
        for (payload, end) in super::chunks(&payload) {
            self.next += 1;
            self.bytes += payload.len();
            self.outbox.push_back((self.next, payload, end));
        }
        Ok(self.next)
    }
    async fn reset(&mut self, incoming: &mpsc::Sender<String>) -> Result<(), RpcError> {
        for id in self.pending.keys() {
            incoming.send(serde_json::to_string(&ServerFrame { id: *id,
                err: Some("Remote connection was reset. The last action may have completed; check its result before retrying.".into()),
                ..Default::default() }).unwrap()).await.map_err(|_| RpcError::Closed)?;
        }
        *self = Self::new();
        Ok(())
    }
}

async fn run(
    profile: ConnectionProfile,
    mut outgoing: mpsc::Receiver<String>,
    incoming: mpsc::Sender<String>,
    status: watch::Sender<ConnectionState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut replay = Replay::new();
    let mut retry = Duration::from_millis(250);
    loop {
        if incoming.is_closed() || wrapper_gone(&shutdown) {
            break;
        }
        let started = Instant::now();
        let result = connection(
            &profile,
            &mut replay,
            &mut outgoing,
            &incoming,
            &status,
            &mut shutdown,
        )
        .await;
        // Any `Ok` is terminal ([`Ended`]); the two checks before it catch a
        // wrapper that was dropped while the socket was already down.
        if result.is_ok() || incoming.is_closed() || wrapper_gone(&shutdown) {
            break;
        }
        let unauthorized = result
            .as_ref()
            .err()
            .is_some_and(|e| matches!(e, RpcError::Transport(s) if s == "unauthorized"));
        status.send_replace(if unauthorized {
            ConnectionState::Unauthorized
        } else {
            ConnectionState::Reconnecting
        });
        // A session reset wakes subscription loops; it never retries a mutation
        // under a new session identity. Unauthorized clients do not retain work.
        if matches!(result, Err(RpcError::Transport(ref s)) if s == "reset") || unauthorized {
            if replay.reset(&incoming).await.is_err() {
                break;
            }
        }
        if unauthorized {
            break;
        }
        if started.elapsed() > Duration::from_secs(20) {
            retry = Duration::from_millis(250);
        }
        tokio::select! {
            _ = incoming.closed() => break,
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(retry) => {}
        }
        retry = (retry * 2).min(Duration::from_secs(5));
    }
}

/// The wrapper can no longer be reached: `shutdown` was requested, or every
/// sender is gone because the `RemoteClient` was dropped. `borrow()` keeps
/// reporting the last value after the sender dies, so the closed channel has to
/// be observed separately — otherwise a held `Arc<RpcClient>` clone keeps this
/// loop dialing sessions nobody can shut down or observe.
fn wrapper_gone(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

async fn connection(
    profile: &ConnectionProfile,
    replay: &mut Replay,
    outgoing: &mut mpsc::Receiver<String>,
    incoming: &mpsc::Sender<String>,
    status: &watch::Sender<ConnectionState>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Ended, RpcError> {
    let mut request = profile
        .endpoint
        .clone()
        .into_client_request()
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", profile.token)
            .parse()
            .map_err(|_| RpcError::Transport("Invalid connection key".into()))?,
    );
    // A dial parked on a handshake that never answers (a stranger holding the
    // port, a peer that vanished mid-upgrade) must not hold shutdown for the
    // full timeout, nor outlive the wrapper that asked to stop.
    let dial = tokio::select! {
        dial = timeout(
            Duration::from_secs(8),
            tokio_tungstenite::connect_async(request),
        ) => dial,
        _ = incoming.closed() => return Ok(Ended::ClientGone),
        _ = shutdown.changed() => return Ok(Ended::Stopped),
    };
    let (mut socket, _) = match dial {
        Ok(Ok(ws)) => ws,
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
            if response.status().as_u16() == 401 =>
        {
            return Err(RpcError::Transport("unauthorized".into()));
        }
        _ => return Err(RpcError::Transport("Computer unreachable".into())),
    };
    macro_rules! send {
        ($frame:expr) => {
            timeout(
                Duration::from_secs(10),
                socket.send(Message::Text(encode(&$frame)?)),
            )
            .await
            .map_err(|_| RpcError::Transport("Send timed out".into()))?
            .map_err(|_| RpcError::Closed)?
        };
    }
    send!(Frame::Hello {
        version: PROTOCOL,
        session: replay.session.clone(),
        cursor: replay.cursor,
        resume: replay.established
    });
    // Waiting on the Welcome is the other window where a stop request would sit
    // behind a silent peer for ten seconds.
    let message = tokio::select! {
        message = timeout(Duration::from_secs(10), socket.next()) => message
            .map_err(|_| RpcError::Closed)?
            .ok_or(RpcError::Closed)?
            .map_err(|_| RpcError::Closed)?,
        _ = incoming.closed() => return Ok(Ended::ClientGone),
        _ = shutdown.changed() => return Ok(Ended::Stopped),
    };
    let frame: Frame = serde_json::from_str(message.to_text().map_err(|_| RpcError::Closed)?)
        .map_err(|_| RpcError::Closed)?;
    let received = match frame {
        Frame::Welcome {
            version: PROTOCOL,
            received,
            ..
        } if received <= replay.next => received,
        Frame::Reset => return Err(RpcError::Transport("reset".into())),
        _ => {
            return Err(RpcError::Transport(
                "Incompatible connection protocol".into(),
            ));
        }
    };
    replay.established = true;
    replay.acknowledge(received);
    let mut transmitted = received;
    status.send_replace(ConnectionState::Connected);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut last_received = Instant::now();
    loop {
        if let Some((seq, payload, end)) = replay
            .outbox
            .iter()
            .find(|(seq, _, _)| *seq > transmitted)
            .cloned()
        {
            send!(Frame::Data { seq, payload, end });
            transmitted = seq;
        }
        let more = replay.next > transmitted;
        tokio::select! {
            _ = tokio::task::yield_now(), if more => {},
            _ = incoming.closed() => {
                send!(Frame::Goodbye);
                let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                return Ok(Ended::ClientGone);
            }
            _ = shutdown.changed() => {
                // Explicit stop, or the wrapper was dropped: release the
                // retained session while the socket is still up, then never
                // dial again.
                send!(Frame::Goodbye);
                let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                return Ok(Ended::Stopped);
            }
            payload = outgoing.recv(), if replay.bytes < 8 * 1024 * 1024 => {
                let Some(payload) = payload else { return Ok(Ended::ClientGone) };
                if payload.len() > super::MAX_FRAME {
                    let frame: ClientFrame = serde_json::from_str(&payload).map_err(|_| RpcError::Closed)?;
                    incoming.send(serde_json::to_string(&ServerFrame { id: frame.id,
                        err: Some("Remote request exceeds the 32 MiB limit".into()), ..Default::default()
                    }).unwrap()).await.map_err(|_| RpcError::Closed)?;
                    continue;
                }
                replay.queue(payload)?;
            }
            message = socket.next() => {
                last_received = Instant::now();
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_))) => {
                        timeout(Duration::from_secs(10), socket.flush()).await
                            .map_err(|_| RpcError::Closed)?.map_err(|_| RpcError::Closed)?;
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => return Err(RpcError::Closed),
                };
                match serde_json::from_str::<Frame>(&text).map_err(|_| RpcError::Closed)? {
                    Frame::Data { seq, payload, end } => {
                        if seq > replay.cursor {
                            if seq != replay.cursor + 1 { return Err(RpcError::Transport("reset".into())); }
                            replay.partial.push_str(&payload);
                            if replay.partial.len() > super::MAX_FRAME { return Err(RpcError::Transport("reset".into())); }
                            if end {
                            let payload = std::mem::take(&mut replay.partial);
                            for line in payload.lines() {
                                if let Ok(frame) = serde_json::from_str::<ServerFrame>(line)
                                    && (frame.err.is_some() || frame.done || (frame.ok.is_some() && frame.ok.as_ref().and_then(|v| v.get("stream")) != Some(&serde_json::Value::Bool(true)))) {
                                        replay.pending.remove(&frame.id);
                                    }
                            }
                            incoming.send(payload).await.map_err(|_| RpcError::Closed)?;
                            }
                            replay.cursor = seq;
                        }
                        send!(Frame::Ack { seq: replay.cursor });
                    }
                    Frame::Ack { seq } if seq <= replay.next => replay.acknowledge(seq),
                    Frame::Pong => {},
                    Frame::Reset => return Err(RpcError::Transport("reset".into())),
                    _ => return Err(RpcError::Closed),
                }
            }
            _ = tick.tick() => {
                if last_received.elapsed() > Duration::from_secs(15) { return Err(RpcError::Closed); }
                send!(Frame::Ping);
            }
        }
    }
}
