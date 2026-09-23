//! Manual probe: run the real driver through the engine bridge and move the
//! agent cursor so the overlay surface can be observed via `hyprctl layers`.
//! Ignored by default - needs the installed driver on a live Wayland session.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use zeron_engine::computer_use::ComputerUseManager;
use zeron_harness::CancellationToken;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

async fn call(socket: &Path, action: &str, args: Value) -> Value {
    let mut stream = BufReader::new(UnixStream::connect(socket).await.unwrap());
    let mut bytes = serde_json::to_vec(&json!({"action":action,"args":args})).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    let mut line = Vec::new();
    stream.read_until(b'\n', &mut line).await.unwrap();
    serde_json::from_slice(&line).unwrap()
}

#[tokio::test]
#[ignore = "manual probe: moves the real cursor overlay on a live session"]
async fn serve_proxy_renders_agent_cursor() {
    let manager = ComputerUseManager::new("probe-host".into());
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let approval = Arc::new(move |qs: Vec<UserInputQuestion>| {
        c.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        tx.send(vec![UserInputAnswer {
            question_id: qs[0].id.clone(),
            labels: vec!["Allow".into()],
        }])
        .unwrap();
        rx
    });
    let (socket, bridge) = manager
        .start_bridge("c", "r", approval, CancellationToken::new())
        .await
        .unwrap();
    // First call grants approval + spawns serve+mcp.
    let r = call(&socket, "list_windows", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // Move the synthetic agent cursor; serve should bind the overlay.
    let r = call(
        &socket,
        "move_cursor",
        json!({"x":900,"y":500,"scope":"window"}),
    )
    .await;
    println!("MOVE: {}", serde_json::to_string(&r).unwrap_or_default());
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    // Give time to observe `hyprctl layers` for cua-agent-cursor.
    bridge.turn_ended().await;
    bridge.finish().await;
}
