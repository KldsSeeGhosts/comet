use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

fn manager(dir: &Path) -> ComputerUseManager {
    let path = dir.join("driver.py");
    std::fs::write(&path, r#"#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
log = Path(__file__).with_suffix('.log')
for line in sys.stdin:
    req = json.loads(line)
    with log.open('a') as f:
        f.write(json.dumps(dict(req, pid=os.getpid())) + '\n')
    if 'id' not in req: continue
    if req['method'] == 'initialize': result = {'protocolVersion':'2024-11-05'}
    elif req['method'] == 'tools/list':
        result = {'tools':[{'name':'click','inputSchema':{'type':'object'}}, {'name':'set_config'}]}
    else:
        assert req['method'] == 'tools/call'
        args = req['params']['arguments']
        if args.get('block'): time.sleep(60)
        result = {'content':[{'type':'text','text':'你好'}, {'type':'image','mimeType':'image/png','data':'cG5n'}],
                  'structuredContent':{'args':args,'pid':os.getpid()}, 'isError':args.get('refuse', False)}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut manager = ComputerUseManager::new("test-host".into());
    manager.driver_path = Some(path);
    manager
}

fn approval(allow: bool, count: Arc<AtomicUsize>) -> RequestInput {
    Arc::new(move |questions| {
        count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        tx.send(vec![UserInputAnswer {
            question_id: questions[0].id.clone(),
            labels: vec![if allow { "Allow" } else { "Deny" }.into()],
        }])
        .unwrap();
        rx
    })
}

async fn call(socket: &Path, action: &str, args: Value) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let mut bytes = serde_json::to_vec(&json!({"action":action,"args":args})).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
        read_frame(&mut BufReader::new(stream), RESPONSE_LIMIT)
            .await
            .unwrap()
    })
    .await
    .expect("test bridge timed out")
}

#[tokio::test]
async fn full_results_permissions_and_turn_lease() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let (a, bridge_a) = manager
        .start_bridge(
            "a",
            "a",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let (b, bridge_b) = manager
        .start_bridge(
            "b",
            "b",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let help = call(&a, "help", json!({})).await;
    assert_eq!(
        help["structuredContent"]["tools"].as_array().unwrap().len(),
        1
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let result = call(
        &a,
        "get_window_state",
        json!({"pid":42,"session":"untrusted"}),
    )
    .await;
    assert_eq!(result["content"][0]["text"], "你好");
    assert_eq!(result["content"][1]["data"], "cG5n");
    assert_ne!(result["structuredContent"]["args"]["session"], "untrusted");
    let pid = result["structuredContent"]["pid"].as_u64().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(call(&b, "list_windows", json!({})).await["isError"], true);
    assert_eq!(
        call(&a, "click", json!({"pid":42,"refuse":true})).await["isError"],
        true
    );
    call(&a, "click", json!({"pid":42,"delivery_mode":"foreground"})).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "one grant covers the whole turn, including foreground delivery"
    );
    bridge_a.turn_ended().await;
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "driver must be reaped before releasing its lease"
    );
    assert_eq!(call(&a, "list_windows", json!({})).await["isError"], true);
    assert_eq!(call(&b, "list_windows", json!({})).await["isError"], false);
    bridge_b.turn_ended().await;
    bridge_a.turn_started();
    assert_eq!(call(&a, "list_windows", json!({})).await["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 3, "a fresh turn asks again");
    bridge_a.finish().await;
    bridge_b.finish().await;
    assert!(!a.exists());
}

#[tokio::test]
async fn denial_and_unreviewed_actions_fail_closed_without_holding_lease() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let (socket, bridge) = manager
        .start_bridge(
            "a",
            "a",
            approval(false, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for action in ["set_config", "end_session", "made_up_action"] {
        assert_eq!(call(&socket, action, json!({})).await["isError"], true);
    }
    assert_eq!(
        call(&socket, "click", json!({"_session_id":"other"})).await["isError"],
        true
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        assert_eq!(
            call(&socket, "list_windows", json!({})).await["isError"],
            true
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(lock(&manager.lease).is_none());
    assert!(!dir.path().join("driver.log").exists());
    bridge.finish().await;
}

async fn blocked_call(socket: &Path, dir: &Path) -> (UnixStream, u64) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(b"{\"action\":\"click\",\"args\":{\"block\":true}}\n")
        .await
        .unwrap();
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let log = std::fs::read_to_string(dir.join("driver.log")).unwrap_or_default();
            if let Some(value) = log
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v.pointer("/params/arguments/block") == Some(&json!(true)))
            {
                break value["pid"].as_u64().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (stream, pid)
}

#[tokio::test]
async fn interrupt_and_disconnect_reap_inflight_driver_without_replaying() {
    for disconnect in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let interrupt = CancellationToken::new();
        let (socket, bridge) = manager
            .start_bridge(
                "a",
                "a",
                approval(true, Arc::new(AtomicUsize::new(0))),
                interrupt.clone(),
            )
            .await
            .unwrap();
        let (stream, pid) = blocked_call(&socket, dir.path()).await;
        if disconnect {
            drop(stream);
        } else {
            interrupt.cancel();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while PathBuf::from(format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("blocked driver was not reaped promptly");
        assert!(lock(&manager.lease).is_none());
        let log = std::fs::read_to_string(dir.path().join("driver.log")).unwrap();
        assert_eq!(
            log.matches("\"block\": true").count(),
            1,
            "never replay uncertain input"
        );
        bridge.finish().await;
    }
}

#[tokio::test]
async fn frame_limits_and_partial_clients_do_not_block_teardown() {
    let bytes = vec![b'x'; 100];
    assert!(
        read_frame(&mut BufReader::new(bytes.as_slice()), 32)
            .await
            .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let (socket, bridge) = manager
        .start_bridge(
            "a",
            "a",
            approval(false, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let _partial = UnixStream::connect(&socket).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), bridge.finish())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires an installed Linux cua-driver; metadata only, no desktop input"]
async fn installed_driver_metadata_smoke() {
    let manager = ComputerUseManager::new("dev-metadata-smoke".into());
    let (socket, bridge) = manager
        .start_bridge(
            "smoke",
            "smoke",
            approval(false, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let health = call(&socket, "health_report", json!({})).await;
    assert_ne!(health["isError"], true, "{health}");
    assert!(health["structuredContent"].is_object(), "{health}");
    let schema = call(&socket, "describe", json!({"name":"get_window_state"})).await;
    assert_eq!(
        schema["structuredContent"]["tools"][0]["name"], "get_window_state",
        "{schema}"
    );
    bridge.turn_ended().await;
    assert!(lock(&manager.lease).is_none());
    bridge.finish().await;
}
