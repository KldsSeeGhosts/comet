use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn real_daemon_args_carry_serve_standard_mode_and_existing_profile_grant() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("cua-driver");
    std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
    let socket = dir.path().join("driver.sock");

    let build_args = |grant: bool| {
        let command = Driver::serve_command(&exe, &socket, grant);
        let program = command.as_std().get_program().to_string_lossy();
        assert!(program.ends_with("cua-driver"), "program: {program}");
        command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<String>>()
    };

    // grant=false: metadata daemons run without the profile grant.
    let ungranted = build_args(false);
    let serve_pos = ungranted
        .iter()
        .position(|a| a == "serve")
        .expect("serve arg");
    assert_eq!(serve_pos, 0, "serve must be the subcommand, first arg");
    let mode = ungranted
        .iter()
        .position(|a| a == "--permission-mode")
        .expect("--permission-mode flag");
    assert_eq!(ungranted[mode + 1], "standard", "mode must stay standard");
    let socket_arg = ungranted
        .iter()
        .position(|a| a == "--socket")
        .expect("--socket flag");
    assert_eq!(ungranted[socket_arg + 1], socket.to_string_lossy());
    assert!(
        !ungranted.iter().any(|a| a == "--grant"),
        "ungranted daemon must not carry --grant: {ungranted:?}"
    );

    // grant=true: exactly one existing-profile grant.
    let granted = build_args(true);
    assert_eq!(
        granted.len(),
        ungranted.len() + 2,
        "grant adds only its flag pair"
    );
    let grants: Vec<usize> = granted
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--grant")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        grants,
        vec![ungranted.len()],
        "exactly one trailing --grant"
    );
    assert_eq!(granted[grants[0] + 1], "existing-profile", "grant value");
}

#[test]
fn mcp_fixture_command_has_no_daemon_flags() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("driver.py");
    let socket = dir.path().join("driver.sock");

    // Real-host path: the mcp child proxies through the daemon socket and
    // must carry only mcp + --socket, never serve-side policy flags.
    let proxied = Driver::mcp_command(&exe, Some(&socket));
    let args: Vec<String> = proxied
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args.first().map(String::as_str), Some("mcp"));
    for flag in ["serve", "--permission-mode", "--grant"] {
        assert!(
            !args.iter().any(|a| a == flag),
            "proxied mcp command must not carry daemon flag {flag}: {args:?}"
        );
    }
    assert_eq!(args.iter().filter(|a| *a == "--socket").count(), 1);

    // Injected fixture path: single-process stdio mcp, no socket at all.
    let plain = Driver::mcp_command(&exe, None);
    let args: Vec<String> = plain
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, vec!["mcp"], "fixture mcp command must stay bare");
}

#[test]
fn approval_question_discloses_existing_profile_devtools_attachment() {
    let text = approval_question("test-host", "list_windows");
    assert!(text.contains("Allow computer use on host test-host for this session?"));
    assert!(
        text.contains("attach DevTools to an existing logged-in Chromium-family browser profile"),
        "question must disclose existing-profile DevTools attachment: {text}"
    );
}

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
        result = {'tools':[{'name':'click','inputSchema':{'type':'object'}}, {'name':'list_windows','inputSchema':{'type':'object'}}, {'name':'set_config'}]}
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

#[test]
fn stale_daemon_socket_is_removed_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("driver.sock");
    std::fs::write(&socket, b"stale").unwrap();

    remove_socket_if_present(&socket).unwrap();
    remove_socket_if_present(&socket).unwrap();

    assert!(!socket.exists());
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
        2
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
    let resumed = call(&a, "list_windows", json!({})).await;
    assert_eq!(resumed["isError"], false);
    assert_ne!(
        resumed["structuredContent"]["pid"].as_u64().unwrap(),
        pid,
        "a resumed turn re-acquires the lease behind a fresh driver"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "a session grant survives turn boundaries; only the driver and lease reset"
    );
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
    bridge.turn_ended().await;
    bridge.turn_started();
    assert_eq!(
        call(&socket, "list_windows", json!({})).await["isError"],
        true
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "a denial is per-turn; a fresh turn asks again"
    );
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

    bridge.turn_started();
    let restarted = call(&socket, "health_report", json!({})).await;
    assert_ne!(restarted["isError"], true, "{restarted}");
    bridge.turn_ended().await;
    bridge.finish().await;
}

#[tokio::test]
async fn positive_grant_persists_across_bridge_recreation_per_chat() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let count = Arc::new(AtomicUsize::new(0));

    // Chat 1, Bridge 1: first action triggers prompt (count -> 1).
    let (socket1, bridge1) = manager
        .start_bridge(
            "chat-1",
            "run-1",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let res1 = call(&socket1, "click", json!({"pid": 42})).await;
    assert_eq!(res1["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Destroy bridge 1.
    bridge1.finish().await;
    assert!(!socket1.exists());
    assert!(
        lock(&manager.lease).is_none(),
        "lease must be released when bridge is destroyed"
    );

    // Chat 1, Bridge 2: recreation inherits approval from manager; no second prompt (count stays 1).
    let (socket2, bridge2) = manager
        .start_bridge(
            "chat-1",
            "run-2",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let res2 = call(&socket2, "click", json!({"pid": 42})).await;
    assert_eq!(res2["isError"], false);
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "second bridge for the same chat inherits approval without prompting"
    );
    bridge2.finish().await;
    assert!(!socket2.exists());
    assert!(
        lock(&manager.lease).is_none(),
        "lease must not be held merely because a chat has approval"
    );

    // Chat 2, Bridge 3: another chat is isolated; it must still prompt (count -> 2).
    let (socket3, bridge3) = manager
        .start_bridge(
            "chat-2",
            "run-1",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let res3 = call(&socket3, "click", json!({"pid": 42})).await;
    assert_eq!(res3["isError"], false);
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "another chat must prompt independently"
    );
    bridge3.finish().await;
    assert!(lock(&manager.lease).is_none());
}

#[tokio::test]
async fn describe_accepts_name_and_names_and_rejects_invalid_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let (socket, bridge) = manager
        .start_bridge(
            "chat-desc",
            "run-desc",
            approval(false, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // 1. One name returns single schema
    let res1 = call(&socket, "describe", json!({"name": "click"})).await;
    assert_ne!(res1["isError"], true);
    let tools1 = res1["structuredContent"]["tools"].as_array().unwrap();
    assert_eq!(tools1.len(), 1);
    assert_eq!(tools1[0]["name"], "click");
    assert_match_pointer_digest(&res1["content"][0]["text"]);

    // 2. Bounded names array returns multiple schemas in one tools/list response
    let res2 = call(
        &socket,
        "describe",
        json!({"names": ["click", "list_windows"]}),
    )
    .await;
    assert_ne!(res2["isError"], true);
    let tools2 = res2["structuredContent"]["tools"].as_array().unwrap();
    assert_eq!(tools2.len(), 2);
    assert_eq!(tools2[0]["name"], "click");
    assert_eq!(tools2[1]["name"], "list_windows");
    assert_match_pointer_digest(&res2["content"][0]["text"]);

    // Preserves ordering
    let res_rev = call(
        &socket,
        "describe",
        json!({"names": ["list_windows", "click"]}),
    )
    .await;
    assert_ne!(res_rev["isError"], true);
    let tools_rev = res_rev["structuredContent"]["tools"].as_array().unwrap();
    assert_eq!(tools_rev.len(), 2);
    assert_eq!(tools_rev[0]["name"], "list_windows");
    assert_eq!(tools_rev[1]["name"], "click");

    // 3. Rejects unexposed action in name
    let unexp1 = call(&socket, "describe", json!({"name": "set_config"})).await;
    assert_eq!(unexp1["isError"], true);

    // 4. Rejects unexposed action in names
    let unexp2 = call(
        &socket,
        "describe",
        json!({"names": ["click", "set_config"]}),
    )
    .await;
    assert_eq!(unexp2["isError"], true);

    // 5. Rejects unknown action name
    let unk1 = call(&socket, "describe", json!({"name": "nonexistent"})).await;
    assert_eq!(unk1["isError"], true);
    let unk2 = call(
        &socket,
        "describe",
        json!({"names": ["click", "nonexistent"]}),
    )
    .await;
    assert_eq!(unk2["isError"], true);

    // 6. Rejects mixed shapes (both name and names)
    let mixed = call(
        &socket,
        "describe",
        json!({"name": "click", "names": ["click"]}),
    )
    .await;
    assert_eq!(mixed["isError"], true);

    // 7. Rejects missing name and names
    let empty_args = call(&socket, "describe", json!({})).await;
    assert_eq!(empty_args["isError"], true);

    // 8. Rejects empty names array
    let empty_names = call(&socket, "describe", json!({"names": []})).await;
    assert_eq!(empty_names["isError"], true);

    // 9. Rejects non-string name
    let bad_name = call(&socket, "describe", json!({"name": 123})).await;
    assert_eq!(bad_name["isError"], true);

    // 10. Rejects non-array names
    let bad_names = call(&socket, "describe", json!({"names": "click"})).await;
    assert_eq!(bad_names["isError"], true);

    // 11. Rejects non-string element in names
    let bad_elem = call(&socket, "describe", json!({"names": ["click", 123]})).await;
    assert_eq!(bad_elem["isError"], true);

    // 12. Rejects unexpected extra args
    let extra = call(
        &socket,
        "describe",
        json!({"name": "click", "unexpected": true}),
    )
    .await;
    assert_eq!(extra["isError"], true);

    // 13. Rejects bounded limit overflow (> 32)
    let overflow = (0..33).map(|_| "click").collect::<Vec<_>>();
    let bad_limit = call(&socket, "describe", json!({"names": overflow})).await;
    assert_eq!(bad_limit["isError"], true);

    bridge.finish().await;
}

#[tokio::test]
async fn revocation_clears_stored_grant_and_forces_existing_and_new_bridges_to_reprompt() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let count = Arc::new(AtomicUsize::new(0));

    let (socket, bridge) = manager
        .start_bridge(
            "chat-revoke",
            "run-1",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // First action prompts and grants session approval.
    let res = call(&socket, "click", json!({"pid": 10})).await;
    assert_eq!(res["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Second action in same bridge uses cached approval without prompt.
    let res2 = call(&socket, "click", json!({"pid": 10})).await;
    assert_eq!(res2["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Revoke approval synchronously.
    assert!(manager.forget_computer_use_approval("chat-revoke"));
    assert!(!manager.forget_computer_use_approval("chat-revoke"));

    // Existing bridge's next non-metadata action prompts again.
    let res3 = call(&socket, "click", json!({"pid": 10})).await;
    assert_eq!(res3["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 2);

    // Tear down bridge and create a new bridge for the same chat after another revocation.
    bridge.finish().await;
    assert!(manager.forget_computer_use_approval("chat-revoke"));

    let (socket2, bridge2) = manager
        .start_bridge(
            "chat-revoke",
            "run-2",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let res4 = call(&socket2, "click", json!({"pid": 10})).await;
    assert_eq!(res4["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 3);

    bridge2.finish().await;
}

fn assert_match_pointer_digest(text: &Value) {
    let s = text.as_str().unwrap();
    assert!(
        s.contains("Ordinary window-scoped pointer actions use the synthetic agent cursor and do not move the user's pointer"),
        "expected pointer digest in {s}"
    );
    assert!(
        s.contains("only scope=desktop with explicit disruption approval may use the real seat"),
        "expected disruption guidance in {s}"
    );
}
