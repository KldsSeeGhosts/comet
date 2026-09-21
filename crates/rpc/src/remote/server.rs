use super::{Credentials, Frame, MAX_FRAME, PROTOCOL, encode};
use futures::{SinkExt, StreamExt};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    time::timeout,
};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
};

#[derive(Clone)]
pub struct ServerOptions {
    pub upstream: String,
    pub credentials: PathBuf,
    pub retention: Duration,
    pub replay_bytes: usize,
}
impl ServerOptions {
    pub fn new(upstream: String, credentials: PathBuf) -> Self {
        Self {
            upstream,
            credentials,
            retention: Duration::from_secs(3600),
            replay_bytes: 32 * 1024 * 1024,
        }
    }
}
struct State {
    received: u64,
    sent: u64,
    acknowledged: u64,
    replay: VecDeque<(u64, String, bool)>,
    bytes: usize,
    touched: Instant,
    generation: u64,
    valid: bool,
}
struct Delivery {
    generation: u64,
    seq: u64,
    payload: String,
    end: bool,
    ack: oneshot::Sender<bool>,
}
struct Session {
    state: Mutex<State>,
    input: mpsc::Sender<Delivery>,
    changed: watch::Sender<u64>,
    token: String,
}
impl Session {
    fn invalidate(&self) {
        self.state.lock().unwrap().valid = false;
        self.changed.send_modify(|n| *n += 1);
    }
}
type Sessions = Arc<tokio::sync::Mutex<HashMap<(String, String), Arc<Session>>>>;

async fn session(
    options: &ServerOptions,
    token: String,
    device_id: &str,
) -> anyhow::Result<Arc<Session>> {
    let (mut upstream, _) = timeout(
        Duration::from_secs(8),
        tokio_tungstenite::connect_async(&options.upstream),
    )
    .await??;
    // Refuse a port that now belongs to another installation or profile identity.
    upstream
        .send(Message::Text(
            serde_json::json!({"id":0,"method":"EngineInfo","params":{}}).to_string(),
        ))
        .await?;
    let identity = timeout(Duration::from_secs(8), upstream.next())
        .await?
        .ok_or_else(|| anyhow::anyhow!("Engine closed"))??;
    let identity: crate::ServerFrame = serde_json::from_str(identity.to_text()?)?;
    anyhow::ensure!(
        identity
            .ok
            .as_ref()
            .and_then(|v| v.get("deviceId"))
            .and_then(|v| v.as_str())
            == Some(device_id),
        "Engine identity changed; pair this computer again"
    );
    let (input, mut requests) = mpsc::channel::<Delivery>(256);
    let (changed, _) = watch::channel(0);
    let session = Arc::new(Session {
        state: Mutex::new(State {
            received: 0,
            sent: 0,
            acknowledged: 0,
            replay: VecDeque::new(),
            bytes: 0,
            touched: Instant::now(),
            generation: 0,
            valid: true,
        }),
        input,
        changed,
        token,
    });
    let weak = Arc::downgrade(&session);
    let options = options.clone();
    let mut changed = session.changed.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut request_buffer = String::new();
        loop {
            let Some(s) = weak.upgrade() else { break };
            if !s.state.lock().unwrap().valid {
                break;
            }
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    let received = {
                        let st = s.state.lock().unwrap();
                        if !st.valid || st.generation != request.generation {
                            let _ = request.ack.send(false);
                            continue;
                        }
                        st.received
                    };
                    if request.seq <= received { let _ = request.ack.send(true); continue; }
                    if request.seq != received + 1 { let _ = request.ack.send(false); break; }
                    request_buffer.push_str(&request.payload);
                    if request_buffer.len() > MAX_FRAME { break; }
                    if request.end && !matches!(timeout(Duration::from_secs(10), upstream.send(Message::Text(std::mem::take(&mut request_buffer)))).await, Ok(Ok(()))) { break; }
                    s.state.lock().unwrap().received = request.seq;
                    let _ = request.ack.send(true);
                }
                message = upstream.next() => match message {
                    Some(Ok(Message::Text(payload))) => {
                        let overflow = {
                            let mut state = s.state.lock().unwrap();
                            for (payload, end) in super::chunks(&payload) {
                                state.sent += 1;
                                let seq = state.sent;
                                state.bytes += payload.len();
                                state.replay.push_back((seq, payload, end));
                            }
                            state.bytes > options.replay_bytes
                        };
                        s.changed.send_modify(|n| *n += 1);
                        if overflow { break; }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                },
                _ = changed.changed() => {},
                _ = tick.tick() => {
                    let expired = s.state.lock().unwrap().touched.elapsed() > options.retention;
                    let authorized = Credentials::load(&options.credentials).is_ok_and(|c| c.authorizes(&s.token));
                    if expired || !authorized { break; }
                    if !matches!(timeout(Duration::from_secs(5), upstream.send(Message::Ping(Vec::new()))).await, Ok(Ok(()))) { break; }
                }
            }
        }
        if let Some(s) = weak.upgrade() {
            s.invalidate();
        }
        let _ = timeout(Duration::from_secs(1), upstream.close(None)).await;
    });
    Ok(session)
}

/// Serve only on a specific tailnet or loopback interface. No public listener.
pub async fn serve(listener: TcpListener, options: ServerOptions) -> anyhow::Result<()> {
    anyhow::ensure!(
        super::private_ip(listener.local_addr()?.ip()),
        "Bind to a Tailscale IP or localhost"
    );
    let uri: tokio_tungstenite::tungstenite::http::Uri = options.upstream.parse()?;
    let ip: std::net::IpAddr = uri.host().unwrap_or("").trim_matches(['[', ']']).parse()?;
    anyhow::ensure!(
        ip.is_loopback() && uri.scheme_str() == Some("ws"),
        "Upstream must be the local engine's loopback WebSocket"
    );
    let sessions: Sessions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let mut cleanup = tokio::time::interval(Duration::from_secs(5));
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = cleanup.tick() => {
                sessions.lock().await.retain(|_, session| {
                    let keep = {
                        let st = session.state.lock().unwrap();
                        st.valid && st.touched.elapsed() <= options.retention
                    };
                    if !keep { session.invalidate(); }
                    keep
                });
                continue;
            }
        };
        if !super::private_ip(peer.ip()) {
            continue;
        }
        let sessions = sessions.clone();
        let options = options.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(stream, sessions, options).await {
                tracing::debug!(%error, "remote connection ended");
            }
        });
    }
}

async fn connection(
    stream: tokio::net::TcpStream,
    sessions: Sessions,
    options: ServerOptions,
) -> anyhow::Result<()> {
    let mut profile = None;
    #[allow(clippy::result_large_err)]
    let auth = |req: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let token = req
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        if !req.headers().contains_key("origin")
            && req.uri().path() == "/"
            && req.uri().query().is_none()
            && let (Some(token), Ok(creds)) = (token, Credentials::load(&options.credentials))
            && creds.authorizes(token)
        {
            profile = creds.clients.into_iter().find(|c| c.token == token);
            return Ok(response);
        }
        let mut error = ErrorResponse::new(Some("Connection not authorized".into()));
        *error.status_mut() = StatusCode::UNAUTHORIZED;
        Err(error)
    };
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(MAX_FRAME),
        max_frame_size: Some(MAX_FRAME),
        ..Default::default()
    };
    let mut socket = timeout(
        Duration::from_secs(8),
        tokio_tungstenite::accept_hdr_async_with_config(stream, auth, Some(config)),
    )
    .await??;
    macro_rules! send {
        ($frame:expr) => {
            timeout(
                Duration::from_secs(10),
                socket.send(Message::Text(encode(&$frame)?)),
            )
            .await??
        };
    }
    let profile = profile.ok_or_else(|| anyhow::anyhow!("Unauthorized"))?;
    let hello = timeout(Duration::from_secs(8), socket.next())
        .await?
        .ok_or_else(|| anyhow::anyhow!("Missing hello"))??;
    let Frame::Hello {
        version: PROTOCOL,
        session: session_id,
        cursor,
        resume,
    } = serde_json::from_str(hello.to_text()?)?
    else {
        anyhow::bail!("Unsupported protocol")
    };
    uuid::Uuid::parse_str(&session_id)?;
    // A fresh start carries no cursor. Accepting a nonzero one would hand the
    // client a session whose earlier replies it already skipped, so make it
    // resubscribe instead of silently losing that output.
    if !resume && cursor != 0 {
        tracing::warn!(cursor, "fresh session hello carried a nonzero cursor");
        send!(Frame::Reset);
        return Ok(());
    }
    let key = (profile.id, session_id);
    let mut all = sessions.lock().await;
    all.retain(|_, s| {
        let st = s.state.lock().unwrap();
        st.valid && st.touched.elapsed() <= options.retention
    });
    // `resume: false` is an explicit fresh start for this exact slot: the new
    // reader has no cursor, so a retained session must never be inherited. Its
    // replay floor sits above zero and its `received` watermark would make the
    // replacement reader's seq 1 look like a duplicate, so retire the previous
    // runtime (and its upstream socket) before dialing the replacement.
    if !resume {
        if let Some(previous) = all.remove(&key) {
            previous.invalidate();
        }
    }
    let existing = all.get(&key).cloned();
    // A resumed session must still retain every unacknowledged server frame.
    let resumed = existing.as_ref().is_some_and(|s| {
        let st = s.state.lock().unwrap();
        st.valid && cursor >= st.acknowledged && cursor <= st.sent
    });
    if resume && !resumed {
        drop(all);
        send!(Frame::Reset);
        return Ok(());
    }
    let session = if let Some(s) = existing.filter(|_| resumed) {
        drop(all);
        s
    } else {
        // Only reachable with `resume: false`, which retires this key above:
        // the cap counts the other slots, since a replacement never grows the
        // map.
        anyhow::ensure!(all.len() < 32, "Too many active connections");
        drop(all);
        let candidate = session(&options, profile.token, &profile.device_id).await?;
        let mut all = sessions.lock().await;
        if let Some(existing) = all.get(&key).cloned() {
            // A concurrent connection installed a session for this key while we
            // dialed upstream. This hello asked for a fresh session, so the
            // predecessor's replay history and sequence counters must not be
            // inherited: retire it and take the slot.
            existing.invalidate();
        } else if all.len() >= 32 {
            candidate.invalidate();
            anyhow::bail!("Too many active connections");
        }
        all.insert(key.clone(), candidate.clone());
        candidate
    };
    let (generation, received) = {
        let mut st = session.state.lock().unwrap();
        st.generation += 1;
        st.touched = Instant::now();
        (st.generation, st.received)
    };
    let mut changed = session.changed.subscribe();
    let mut sent = cursor;
    send!(Frame::Welcome {
        version: PROTOCOL,
        resumed,
        received
    });
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut last_input = Instant::now();
    loop {
        let (valid, frames) = {
            let st = session.state.lock().unwrap();
            (
                st.valid && st.generation == generation,
                st.replay
                    .iter()
                    .filter(|(seq, _, _)| *seq > sent)
                    .take(1)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        if !valid {
            send!(Frame::Reset);
            break;
        }
        for (seq, payload, end) in frames {
            timeout(
                Duration::from_secs(10),
                socket.send(Message::Text(encode(&Frame::Data { seq, payload, end })?)),
            )
            .await??;
            sent = seq;
        }
        let more = session.state.lock().unwrap().sent > sent;
        tokio::select! {
            _ = tokio::task::yield_now(), if more => {},
            message = socket.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_))) => {
                        timeout(Duration::from_secs(10), socket.flush()).await??;
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => break,
                };
                last_input = Instant::now();
                {
                    let mut st = session.state.lock().unwrap();
                    if !st.valid || st.generation != generation { break; }
                    st.touched = last_input;
                }
                match serde_json::from_str::<Frame>(&text)? {
                    Frame::Data { seq, payload, end } => {
                        let (tx, rx) = oneshot::channel();
                        timeout(Duration::from_secs(10), session.input.send(Delivery { generation, seq, payload, end, ack: tx })).await??;
                        anyhow::ensure!(timeout(Duration::from_secs(12), rx).await??, "Request sequence gap");
                        send!(Frame::Ack { seq });
                    }
                    Frame::Ack { seq } => {
                        let mut st = session.state.lock().unwrap();
                        if !st.valid || st.generation != generation { break; }
                        anyhow::ensure!(seq <= st.sent, "Invalid acknowledgment");
                        st.acknowledged = st.acknowledged.max(seq);
                        while st.replay.front().is_some_and(|(n, _, _)| *n <= seq) {
                            let (_, payload, _) = st.replay.pop_front().unwrap(); st.bytes -= payload.len();
                        }
                    }
                    Frame::Ping => { send!(Frame::Pong); }
                    Frame::Goodbye => {
                        {
                            let mut st = session.state.lock().unwrap();
                            if st.generation != generation { break; }
                            st.valid = false;
                        }
                        session.changed.send_modify(|n| *n += 1);
                        let mut all = sessions.lock().await;
                        if all.get(&key).is_some_and(|s| Arc::ptr_eq(s, &session)) { all.remove(&key); }
                        break;
                    }
                    _ => anyhow::bail!("Unexpected frame"),
                }
            }
            _ = changed.changed() => {},
            _ = tick.tick() => {
                if last_input.elapsed() > Duration::from_secs(25) { break; }
            }
        }
    }
    Ok(())
}
