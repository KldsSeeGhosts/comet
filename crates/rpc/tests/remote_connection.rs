use async_trait::async_trait;
use futures::{StreamExt, stream};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::{Notify, watch},
    time::timeout,
};
use zeron_rpc::{
    RpcError, RpcReply, RpcService,
    remote::{self, ConnectionProfile, Credentials, ServerOptions},
};

struct Engine {
    mutations: AtomicUsize,
    /// Upstream sessions the gateway opened. `session()` probes `EngineInfo`
    /// exactly once per session, so this is a session counter.
    sessions: AtomicUsize,
    release: Notify,
}
#[async_trait]
impl RpcService for Engine {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            "EngineInfo" => {
                self.sessions.fetch_add(1, Ordering::SeqCst);
                RpcReply::value(
                    &serde_json::json!({"deviceId":"linux-test","workspaceScope":"local"}),
                )
            }
            "Echo" => {
                self.mutations.fetch_add(1, Ordering::SeqCst);
                RpcReply::value(&params)
            }
            "Mutate" => {
                self.mutations.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                RpcReply::value(&"done")
            }
            "Watch" => Ok(RpcReply::Stream(
                stream::unfold(0u64, |n| async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Some((serde_json::json!(n), n + 1))
                })
                .boxed(),
            )),
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    credentials: std::path::PathBuf,
    profile: ConnectionProfile,
    engine: Arc<Engine>,
    options: ServerOptions,
    gateway_addr: std::net::SocketAddr,
    gateway: tokio::task::JoinHandle<anyhow::Result<()>>,
    proxy: tokio::task::JoinHandle<()>,
    upstream: tokio::task::JoinHandle<()>,
    network: watch::Sender<(u64, bool)>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.gateway.abort();
        self.proxy.abort();
        self.upstream.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::with_delay(Duration::ZERO).await
    }
    async fn with_delay(delay: Duration) -> Self {
        Self::with_limits(delay, 32 * 1024 * 1024).await
    }
    async fn with_limits(delay: Duration, replay_bytes: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("keys.json");
        let engine = Arc::new(Engine {
            mutations: AtomicUsize::new(0),
            sessions: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("ws://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(zeron_rpc::serve_ws_listener(listener, engine.clone()));
        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = ConnectionProfile {
            id: "client".into(),
            name: "Linux".into(),
            endpoint: format!("ws://{}", proxy_listener.local_addr().unwrap()),
            token: "a".repeat(64),
            device_id: "linux-test".into(),
        };
        Credentials {
            clients: vec![profile.clone()],
        }
        .save(&credentials)
        .unwrap();
        let mut options = ServerOptions::new(upstream_url, credentials.clone());
        options.replay_bytes = replay_bytes;
        let gateway = tokio::spawn(remote::serve(gateway_listener, options.clone()));
        let (network, rx) = watch::channel((0, true));
        let proxy = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = proxy_listener.accept().await else {
                    break;
                };
                if !rx.borrow().1 {
                    continue;
                }
                let mut changes = rx.clone();
                changes.borrow_and_update();
                tokio::spawn(async move {
                    let Ok(mut peer) = tokio::net::TcpStream::connect(gateway_addr).await else {
                        return;
                    };
                    let (a_read, a_write) = socket.split();
                    let (b_read, b_write) = peer.split();
                    tokio::select! {
                        _ = async { tokio::join!(copy_slow(a_read, b_write, delay), copy_slow(b_read, a_write, delay)); } => {},
                        _ = changes.changed() => {}
                    }
                });
            }
        });
        Self {
            _dir: dir,
            credentials,
            profile,
            engine,
            options,
            gateway_addr,
            gateway,
            proxy,
            upstream,
            network,
        }
    }
    fn offline(&self) {
        self.network.send_modify(|(generation, live)| {
            *generation += 1;
            *live = false;
        });
    }
    fn online(&self) {
        self.network.send_modify(|(generation, live)| {
            *generation += 1;
            *live = true;
        });
    }
    async fn mutated(&self) {
        timeout(Duration::from_secs(5), async {
            while self.engine.mutations.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn network_loss_resumes_stream_and_does_not_repeat_a_mutation() {
    let f = Fixture::new().await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    let mut stream = remote
        .client
        .subscribe("Watch", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), stream.recv())
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let client = remote.client.clone();
    let mutation = tokio::spawn(async move { client.call("Mutate", serde_json::json!({})).await });
    f.mutated().await;
    f.offline();
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.engine.release.notify_one();
    tokio::time::sleep(Duration::from_millis(350)).await;
    f.online();
    assert_eq!(
        timeout(Duration::from_secs(8), mutation)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "done"
    );
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 1);
    for n in 1..25 {
        assert_eq!(
            timeout(Duration::from_secs(5), stream.recv())
                .await
                .unwrap()
                .unwrap(),
            n
        );
    }
}

#[tokio::test]
async fn host_restart_reports_uncertainty_instead_of_repeating_work() {
    let mut f = Fixture::new().await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    let client = remote.client.clone();
    let mutation = tokio::spawn(async move { client.call("Mutate", serde_json::json!({})).await });
    f.mutated().await;
    f.offline();
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.gateway.abort();
    let _ = (&mut f.gateway).await;
    let listener = TcpListener::bind(f.gateway_addr).await.unwrap();
    f.gateway = tokio::spawn(remote::serve(listener, f.options.clone()));
    f.online();
    let error = timeout(Duration::from_secs(10), mutation)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("may have completed"), "{error}");
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 1);
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn wrong_key_is_rejected_and_revocation_ends_an_active_session() {
    let f = Fixture::new().await;
    let mut bad = f.profile.clone();
    bad.token = "b".repeat(64);
    let mut remote = remote::connect(bad).unwrap();
    timeout(Duration::from_secs(5), async {
        while *remote.status.borrow_and_update() != remote::ConnectionState::Unauthorized {
            remote.status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(
            Duration::from_secs(1),
            remote.client.call("Echo", serde_json::json!({}))
        )
        .await
        .unwrap()
        .is_err()
    );
    let mut remote = remote::connect(f.profile.clone()).unwrap();
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    Credentials::default().save(&f.credentials).unwrap();
    timeout(Duration::from_secs(10), async {
        while *remote.status.borrow_and_update() != remote::ConnectionState::Unauthorized {
            remote.status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn origin_header_is_rejected_even_with_a_valid_key() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let f = Fixture::new().await;
    let mut req = f.profile.endpoint.clone().into_client_request().unwrap();
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", f.profile.token).parse().unwrap(),
    );
    req.headers_mut()
        .insert("Origin", "https://untrusted.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
}

async fn copy_slow(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    delay: Duration,
) {
    let mut buffer = [0u8; 8192];
    loop {
        let Ok(n) = reader.read(&mut buffer).await else {
            break;
        };
        if n == 0 {
            break;
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if writer.write_all(&buffer[..n]).await.is_err() {
            break;
        }
    }
}

#[tokio::test]
async fn cellular_rate_large_unicode_message_resumes_mid_transfer() {
    // Each direction is limited to 8 KiB / 50 ms, roughly 1.3 Mbps, with
    // at least 100 ms round-trip latency for small request/reply pairs.
    let f = Fixture::with_delay(Duration::from_millis(50)).await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    let value = serde_json::json!({"text": "夜🌙".repeat(50_000)});
    let expected = value.clone();
    let client = remote.client.clone();
    let reply = tokio::spawn(async move { client.call("Echo", value).await });
    f.mutated().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    f.offline();
    tokio::time::sleep(Duration::from_millis(700)).await;
    f.online();
    let received = timeout(Duration::from_secs(20), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received, expected);
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rapid_connection_flapping_resumes_stream_cleanly() {
    let f = Fixture::new().await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    let mut stream = remote
        .client
        .subscribe("Watch", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), stream.recv())
            .await
            .unwrap()
            .unwrap(),
        0
    );
    for _ in 0..3 {
        f.offline();
        tokio::time::sleep(Duration::from_millis(60)).await;
        f.online();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    for n in 1..15 {
        assert_eq!(
            timeout(Duration::from_secs(5), stream.recv())
                .await
                .unwrap()
                .unwrap(),
            n
        );
    }
}

#[tokio::test]
async fn concurrent_calls_queued_during_network_outage_all_complete_once() {
    let f = Fixture::new().await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    f.offline();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let c1 = remote.client.clone();
    let t1 = tokio::spawn(async move { c1.call("Echo", serde_json::json!({"n": 1})).await });
    let c2 = remote.client.clone();
    let t2 = tokio::spawn(async move { c2.call("Echo", serde_json::json!({"n": 2})).await });
    let c3 = remote.client.clone();
    let t3 = tokio::spawn(async move { c3.call("Echo", serde_json::json!({"n": 3})).await });
    let c4 = remote.client.clone();
    let t4 = tokio::spawn(async move { c4.call("Echo", serde_json::json!({"n": 4})).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.online();
    let r1 = timeout(Duration::from_secs(8), t1)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let r2 = timeout(Duration::from_secs(8), t2)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let r3 = timeout(Duration::from_secs(8), t3)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let r4 = timeout(Duration::from_secs(8), t4)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(r1["n"], 1);
    assert_eq!(r2["n"], 2);
    assert_eq!(r3["n"], 3);
    assert_eq!(r4["n"], 4);
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn upstream_engine_crash_reports_uncertainty_and_resets() {
    use futures::SinkExt;
    let mut f = Fixture::new().await;
    f.gateway.abort();
    let _ = (&mut f.gateway).await;
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    f.options.upstream = format!("ws://{}", upstream.local_addr().unwrap());
    let crash = tokio::spawn(async move {
        let (stream, _) = upstream.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"id":0,"ok":{"deviceId":"linux-test"}}).to_string(),
        ))
        .await
        .unwrap();
        loop {
            let message = ws.next().await.unwrap().unwrap();
            if message.is_text() {
                break;
            }
        }
        ws.close(None).await.unwrap();
    });
    let listener = TcpListener::bind(f.gateway_addr).await.unwrap();
    f.gateway = tokio::spawn(remote::serve(listener, f.options.clone()));
    let remote = remote::connect(f.profile.clone()).unwrap();
    let error = timeout(
        Duration::from_secs(10),
        remote.client.call("Mutate", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("may have completed"), "{error}");
    crash.await.unwrap();
}

#[tokio::test]
async fn payload_exceeding_limit_is_rejected_without_dropping_connection() {
    let f = Fixture::new().await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    let huge = serde_json::json!({"data": "x".repeat(33 * 1024 * 1024)});
    let err = remote.client.call("Echo", huge).await.unwrap_err();
    assert!(
        err.to_string().contains("exceeds the 32 MiB limit"),
        "{err}"
    );
    let info = timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(info["deviceId"], "linux-test");
}

#[tokio::test]
async fn client_shutdown_terminates_gracefully() {
    let f = Fixture::new().await;
    for _ in 0..40 {
        let remote = remote::connect(f.profile.clone()).unwrap();
        timeout(
            Duration::from_secs(5),
            remote.client.call("EngineInfo", serde_json::json!({})),
        )
        .await
        .unwrap()
        .unwrap();
        remote.shutdown().await;
        assert!(
            remote
                .client
                .call("EngineInfo", serde_json::json!({}))
                .await
                .is_err()
        );
    }
}

/// Dropping the wrapper must stop the transport even while a cloned
/// `Arc<RpcClient>` keeps the frame channels open: no reconnect, no replacement
/// session, and every later call on the clone fails instead of waiting.
#[tokio::test]
async fn dropping_the_wrapper_stops_reconnects_behind_a_held_client_clone() {
    let f = Fixture::new().await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    let client = remote.client.clone();
    let mut status = remote.status.clone();
    timeout(Duration::from_secs(5), async {
        while *status.borrow_and_update() != remote::ConnectionState::Connected {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let sessions = f.engine.sessions.load(Ordering::SeqCst);
    assert_eq!(sessions, 1, "one session, one engine identity probe");

    drop(remote);

    // The run task owns the status sender: its death is the transport exiting.
    let exited = timeout(Duration::from_secs(2), async {
        while status.changed().await.is_ok() {}
    })
    .await;
    assert!(
        exited.is_ok(),
        "the transport must exit when its wrapper is dropped"
    );

    let result = timeout(
        Duration::from_secs(2),
        client.call("Echo", serde_json::json!({"n": 1})),
    )
    .await
    .expect("a held clone must fail promptly, not wait on a dead transport");
    assert!(result.is_err(), "{result:?}");

    // Two backoff rounds would have dialed a replacement session by now.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        f.engine.sessions.load(Ordering::SeqCst),
        sessions,
        "a dropped wrapper must not spawn a new session"
    );
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 0);
}

/// A stop request must cancel a dial parked in the WebSocket handshake. The peer
/// accepts TCP and never answers, so without cancel-at-dial the transport holds
/// the caller for the whole dial budget and then redials.
#[tokio::test]
async fn shutdown_cancels_a_stalled_handshake_and_stops_redialing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counting = accepted.clone();
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            // Unrelated localhost tools probe random ports (`HEAD / HTTP/1.1`);
            // only a WebSocket upgrade is one of our dials.
            let mut prefix = [0u8; 4];
            if socket.peek(&mut prefix).await.unwrap_or(0) == 4 && &prefix == b"GET " {
                counting.fetch_add(1, Ordering::SeqCst);
            }
            // Never answer: no Authorization header is ever accepted, so no
            // session and no live auth can exist behind this peer.
            held.push(socket);
        }
    });
    let profile = ConnectionProfile {
        id: "client".into(),
        name: "Linux".into(),
        endpoint: format!("ws://{address}"),
        token: "a".repeat(64),
        device_id: "linux-test".into(),
    };
    let remote = remote::connect(profile).unwrap();
    let client = remote.client.clone();
    let mut status = remote.status.clone();
    let pending = tokio::spawn(async move { client.call("Echo", serde_json::json!({})).await });
    timeout(Duration::from_secs(5), async {
        while accepted.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the dial must reach the stalled peer");

    let started = std::time::Instant::now();
    remote.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "stop must cancel the handshake, not wait out the dial: {:?}",
        started.elapsed()
    );

    let result = timeout(Duration::from_secs(2), pending)
        .await
        .expect("the queued call must fail when the transport stops")
        .unwrap();
    assert!(result.is_err(), "{result:?}");

    // The run task owns the status sender: its death is the transport exiting,
    // which is what makes a later redial impossible rather than unlikely.
    let exited = timeout(Duration::from_secs(1), async {
        while status.changed().await.is_ok() {}
    })
    .await;
    assert!(exited.is_ok(), "the transport must exit on a stop request");

    // Let anything already in flight land before sampling, then hold the line:
    // a stopped transport has no dialer left.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let dialed = accepted.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        dialed,
        "a stopped transport must not redial"
    );
    server.abort();
}

/// Explicit live smoke test. Opens a temporary terminal on an existing chat,
/// checks host output, and closes only that terminal. Never submits agent work.
#[tokio::test]
#[ignore = "requires NOCHES_REMOTE_CODE_FILE and an authorized live host"]
async fn live_tailnet_transcript_files_and_terminal() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let code = std::fs::read_to_string(std::env::var("NOCHES_REMOTE_CODE_FILE").unwrap()).unwrap();
    let profile = ConnectionProfile::from_code(&code).unwrap();
    let remote = remote::connect(profile.clone()).unwrap();
    let client = &remote.client;
    let info = timeout(
        Duration::from_secs(20),
        client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(info["deviceId"], profile.device_id);
    let mut chats = client
        .subscribe("WatchChats", serde_json::json!({}))
        .await
        .unwrap();
    let rows = timeout(Duration::from_secs(10), chats.recv())
        .await
        .unwrap()
        .unwrap();
    let chat = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r.get("spaceId").is_some_and(|v| !v.is_null()))
        .expect("a project chat");
    let chat_id = &chat["id"];
    let mut transcript = client
        .subscribe("WatchDocMessages", serde_json::json!({"chatId":chat_id}))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(10), transcript.recv())
            .await
            .unwrap()
            .is_some()
    );
    let directory = client
        .call(
            "ListWorkspaceDirectory",
            serde_json::json!({"chatId":chat_id}),
        )
        .await
        .unwrap();
    assert!(directory["entries"].is_array());
    if let Some(readme) = directory["entries"].as_array().unwrap().iter().find(|e| {
        e["name"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("README.md"))
    }) {
        let file = client
            .call(
                "ReadWorkspaceFile",
                serde_json::json!({"chatId":chat_id,"path":readme["path"]}),
            )
            .await
            .unwrap();
        assert!(file["text"].is_string());
    }
    let terminal = client
        .call(
            "OpenTerminal",
            serde_json::json!({"chatId":chat_id,"cols":80,"rows":24}),
        )
        .await
        .unwrap();
    let id = &terminal["id"];
    let result = timeout(Duration::from_secs(15), async {
        let mut events = client.subscribe("SubscribeTerminal", serde_json::json!({"terminalId":id})).await.unwrap();
        client.call("WriteTerminal", serde_json::json!({"terminalId":id,"data":STANDARD.encode("printf 'noches-native-%s\\n' 7391\n")})).await.unwrap();
        let mut bytes = Vec::new();
        while let Some(event) = events.recv().await {
            if event["type"] == "data" {
                bytes.extend(STANDARD.decode(event["data"].as_str().unwrap()).unwrap());
                if String::from_utf8_lossy(&bytes).contains("noches-native-7391") { return; }
            }
        }
        panic!("terminal stream ended");
    }).await;
    let closed = timeout(
        Duration::from_secs(5),
        client.call("CloseTerminal", serde_json::json!({"terminalId":id})),
    )
    .await;
    assert!(closed.unwrap().is_ok());
    assert!(
        result.is_ok(),
        "terminal did not return the expected output"
    );
}

#[tokio::test]
async fn replay_overflow_ends_watch_then_allows_a_fresh_session() {
    let f = Fixture::with_limits(Duration::ZERO, 512).await;
    let remote = remote::connect(f.profile.clone()).unwrap();
    let mut stream = remote
        .client
        .subscribe_checked("Watch", serde_json::json!({}))
        .await
        .unwrap();
    stream.recv().await.unwrap();
    f.offline();
    tokio::time::sleep(Duration::from_millis(700)).await;
    f.online();
    timeout(Duration::from_secs(8), async {
        while stream.recv().await.is_some() {}
    })
    .await
    .unwrap();
    timeout(
        Duration::from_secs(5),
        remote.client.call("EngineInfo", serde_json::json!({})),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn lost_ack_replayed_request_executes_once_and_control_ping_survives() {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let f = Fixture::new().await;
    let mut request = f.profile.endpoint.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", f.profile.token).parse().unwrap(),
    );
    let session = uuid::Uuid::new_v4().to_string();
    let (mut ws, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();
    ws.send(Message::Text(
        serde_json::json!({"t":"hello","version":1,"session":session,"cursor":0,"resume":false})
            .to_string(),
    ))
    .await
    .unwrap();
    ws.next().await.unwrap().unwrap();
    let data = serde_json::json!({"t":"data","seq":1,"payload":serde_json::json!({"id":1,"method":"Echo","params":{}}).to_string(),"end":true}).to_string();
    ws.send(Message::Text(data.clone())).await.unwrap();
    f.mutated().await;
    // Lose the ACK and reply, then resend the identical sequence.
    drop(ws);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.send(Message::Text(
        serde_json::json!({"t":"hello","version":1,"session":session,"cursor":0,"resume":true})
            .to_string(),
    ))
    .await
    .unwrap();
    let welcome = ws.next().await.unwrap().unwrap();
    let welcome: serde_json::Value = serde_json::from_str(welcome.to_text().unwrap()).unwrap();
    assert_eq!(welcome["received"], 1);
    ws.send(Message::Text(data)).await.unwrap();
    ws.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            if matches!(ws.next().await.unwrap().unwrap(), Message::Pong(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 1);
    ws.send(Message::Text(
        serde_json::json!({"t":"goodbye"}).to_string(),
    ))
    .await
    .unwrap();
}

/// Raw protocol client for the frame-level gateway contract. The gateway's
/// `Frame` enum is private to the crate, so these tests speak the wire shape
/// directly.
struct RawSession {
    socket:
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl RawSession {
    async fn dial(profile: &ConnectionProfile, session: &str, cursor: u64, resume: bool) -> Self {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let mut request = profile.endpoint.clone().into_client_request().unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", profile.token).parse().unwrap(),
        );
        let (mut socket, _) = timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .unwrap()
        .unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({"t":"hello","version":1,"session":session,"cursor":cursor,"resume":resume})
                    .to_string(),
            ))
            .await
            .unwrap();
        Self { socket }
    }

    /// `None` when the gateway closes without answering (a refusal).
    async fn maybe_frame(&mut self) -> Option<serde_json::Value> {
        let message = timeout(Duration::from_secs(5), self.socket.next())
            .await
            .expect("gateway frame deadline");
        let message = message?.ok()?;
        Some(serde_json::from_str(message.to_text().unwrap()).expect("gateway frame json"))
    }

    async fn frame(&mut self) -> serde_json::Value {
        self.maybe_frame().await.expect("gateway answered")
    }

    async fn frame_kind(&mut self, kind: &str) -> serde_json::Value {
        let frame = self.frame().await;
        assert_eq!(frame["t"], serde_json::json!(kind), "{frame}");
        frame
    }

    /// One request as a single `end`-terminated data frame.
    async fn request(&mut self, seq: u64, method: &str, params: serde_json::Value) {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let payload = serde_json::json!({"id": seq, "method": method, "params": params}).to_string();
        self.socket
            .send(Message::Text(
                serde_json::json!({"t":"data","seq":seq,"payload":payload,"end":true}).to_string(),
            ))
            .await
            .unwrap();
    }
}

/// The ndjson RPC frame carried by a gateway `data` frame.
fn rpc_reply(frame: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(frame["payload"].as_str().expect("payload string")).expect("rpc reply json")
}

/// A `hello` with `resume: false` is an explicit "start over" for this exact
/// slot. Reusing a session id after acknowledged traffic used to hand the new
/// reader the retained runtime instead: it reported the prior `received`
/// watermark, so the reader's seq 1 was acked and dropped as a duplicate, and
/// the iOS companion rejected every reconnect in a loop.
#[tokio::test]
async fn fresh_hello_for_a_retained_session_starts_a_new_runtime() {
    let f = Fixture::new().await;
    let session = "9d2b5c14-6f7a-4e0b-8c33-5a1e7d40b9f2";
    let mut first = RawSession::dial(&f.profile, session, 0, false).await;
    let welcome = first.frame_kind("welcome").await;
    assert_eq!(welcome["resumed"], serde_json::json!(false));
    assert_eq!(welcome["received"], serde_json::json!(0));
    // Acknowledged traffic on the retained session, so its replay floor and
    // `received` watermark both sit above zero.
    first
        .request(1, "Echo", serde_json::json!({"n":"old"}))
        .await;
    assert_eq!(first.frame_kind("ack").await["seq"], serde_json::json!(1));
    let reply = first.frame_kind("data").await;
    assert_eq!(reply["seq"], serde_json::json!(1));
    assert_eq!(reply["end"], serde_json::json!(true));
    assert_eq!(rpc_reply(&reply)["ok"]["n"], serde_json::json!("old"));
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 1);

    // The same slot, re-dialed with no cursor: a fresh upstream session.
    let mut second = RawSession::dial(&f.profile, session, 0, false).await;
    let welcome = second.frame_kind("welcome").await;
    assert_eq!(welcome["resumed"], serde_json::json!(false));
    assert_eq!(welcome["received"], serde_json::json!(0), "{welcome}");
    // Nothing of the retired runtime is replayed to a reader with no cursor —
    // neither old frames nor a busy gap loop chasing them.
    assert!(
        timeout(Duration::from_millis(300), second.socket.next())
            .await
            .is_err(),
        "a fresh reader must not receive inherited frames"
    );
    // The retired runtime is invalidated, so its own reader is told to
    // resubscribe instead of holding a session nobody can reach.
    let reset = first.frame_kind("reset").await;
    assert_eq!(reset["t"], serde_json::json!("reset"));

    // seq 1 belongs to the fresh reader; it must execute exactly once.
    second
        .request(1, "Echo", serde_json::json!({"n":"new"}))
        .await;
    assert_eq!(second.frame_kind("ack").await["seq"], serde_json::json!(1));
    let reply = second.frame_kind("data").await;
    assert_eq!(reply["seq"], serde_json::json!(1));
    assert_eq!(rpc_reply(&reply)["ok"]["n"], serde_json::json!("new"));
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 2);
}

/// A fresh hello for a slot that is already retained replaces that slot, so it
/// must not be refused once every other slot is taken.
#[tokio::test]
async fn reopening_a_retained_slot_is_allowed_at_the_session_cap() {
    let f = Fixture::new().await;
    let target = "3a7c1e58-2b94-4d61-9f0a-77c5e6b81d43";
    for index in 0..32u32 {
        let session = if index == 0 {
            target.to_string()
        } else {
            format!("{index:08x}-2222-4333-8444-555566667777")
        };
        // Dropping the reader keeps the session retained, exactly like a phone
        // that went away mid-turn.
        RawSession::dial(&f.profile, &session, 0, false)
            .await
            .frame_kind("welcome")
            .await;
    }
    // Every slot is held: a key that is not retained cannot join. The refusal
    // drops the gateway side of the socket, which the test proxy holds open, so
    // the observable here is that no welcome arrives.
    let mut refused = RawSession::dial(
        &f.profile,
        "00000000-9999-4333-8444-555566667777",
        0,
        false,
    )
    .await;
    assert!(
        timeout(Duration::from_millis(750), refused.frame())
            .await
            .is_err(),
        "a new key at the cap must not be welcomed"
    );
    // The retained key replaces its own slot rather than asking for a new one.
    let mut again = RawSession::dial(&f.profile, target, 0, false).await;
    let welcome = again.frame_kind("welcome").await;
    assert_eq!(welcome["resumed"], serde_json::json!(false));
    assert_eq!(welcome["received"], serde_json::json!(0), "{welcome}");
    again.request(1, "EngineInfo", serde_json::json!({})).await;
    assert_eq!(again.frame_kind("ack").await["seq"], serde_json::json!(1));
    let reply = again.frame_kind("data").await;
    assert_eq!(
        rpc_reply(&reply)["ok"]["deviceId"],
        serde_json::json!("linux-test")
    );
}

/// A fresh hello must not carry a cursor. Accepting a nonzero one would start a
/// session whose earlier replies the reader never receives.
#[tokio::test]
async fn fresh_hello_with_a_stale_cursor_is_reset() {
    let f = Fixture::new().await;
    let session = "b6b8a7c2-1d3f-4e5a-8f9b-0c1d2e3f4a5b";
    let mut retained = RawSession::dial(&f.profile, session, 0, false).await;
    retained.frame_kind("welcome").await;
    retained
        .request(1, "Echo", serde_json::json!({"n":"old"}))
        .await;
    retained.frame_kind("ack").await;
    retained.frame_kind("data").await;

    // `resume: false` with a cursor is not a resume: start over instead of
    // skipping whatever the reader never received.
    let mut stale = RawSession::dial(&f.profile, session, 5, false).await;
    assert_eq!(
        stale.frame_kind("reset").await["t"],
        serde_json::json!("reset")
    );

    // The rejected dial leaves the retained session exactly as it was.
    retained
        .request(2, "Echo", serde_json::json!({"n":"next"}))
        .await;
    assert_eq!(
        retained.frame_kind("ack").await["seq"],
        serde_json::json!(2)
    );
    let reply = retained.frame_kind("data").await;
    assert_eq!(rpc_reply(&reply)["ok"]["n"], serde_json::json!("next"));
    assert_eq!(f.engine.mutations.load(Ordering::SeqCst), 2);
}
