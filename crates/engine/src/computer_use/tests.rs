use super::*;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn managed_driver_explicitly_enables_the_reviewed_hyprland_input_route() {
    let command = Driver::base_command(Path::new("/usr/bin/cua-driver"));
    let environment: BTreeMap<String, Option<String>> = command
        .as_std()
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();

    assert_eq!(
        environment.get("CUA_DRIVER_PERMISSION_MODE"),
        Some(&Some("standard".into()))
    );
    assert_eq!(
        environment.get("CUA_DRIVER_RS_ENABLE_WAYLAND"),
        Some(&Some("1".into()))
    );
    assert_eq!(
        environment.get("CUA_HYPRLAND_OPEN_INPUT"),
        Some(&Some("1".into()))
    );
    assert!(
        !environment.contains_key("CUA_DRIVER_EXPERIMENTAL_HYPRLAND_INPUT"),
        "the managed driver must not select CUA's test-only input protocol"
    );
}

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
    manager_with_initialize(
        dir,
        r#"{"protocolVersion":"2024-11-05","serverInfo":{"name":"python-fixture","version":"1.2.3"},"capabilities":{"tools":{}},"instructions":"FIXTURE_INSTRUCTIONS_SENTINEL"}"#,
    )
}

fn manager_with_initialize(dir: &Path, initialize: &str) -> ComputerUseManager {
    let path = dir.join("driver.py");
    std::fs::write(&path, r#"#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
here = Path(__file__).parent
log = Path(__file__).with_suffix('.log')
init = json.loads((here / 'driver.init.json').read_text())
for line in sys.stdin:
    req = json.loads(line)
    with log.open('a') as f:
        f.write(json.dumps(dict(req, pid=os.getpid())) + '\n')
    if 'id' not in req: continue
    if req['method'] == 'initialize': result = init
    elif req['method'] == 'tools/list':
        result = {'tools':[{'name':'click','inputSchema':{'type':'object'}}, {'name':'list_windows','inputSchema':{'type':'object'}}, {'name':'set_config'}],
                  'schema_version':'1', 'capability_version':'1',
                  'enforcement_adapters':[{'id':'fixture.adapter','state':'active'}]}
    else:
        assert req['method'] == 'tools/call'
        args = req['params']['arguments']
        if args.get('block'): time.sleep(60)
        if isinstance(args.get('refusal'), dict):
            structured = dict(args['refusal'])
        else:
            structured = {'args':args,'pid':os.getpid()}
        result = {'content':[{'type':'text','text':'你好'}, {'type':'image','mimeType':'image/png','data':'cG5n'}],
                  'structuredContent':structured, 'isError':args.get('refuse', False)}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#).unwrap();
    std::fs::write(dir.join("driver.init.json"), initialize).unwrap();
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

/// Fixture request count by MCP method, as observed by the driver process.
fn request_count(dir: &Path, method: &str) -> usize {
    std::fs::read_to_string(dir.join("driver.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("method").and_then(Value::as_str) == Some(method))
        .count()
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
        call(&a, "click", json!({"pid":42,"window_id":7,"refuse":true})).await["isError"],
        true
    );
    assert_eq!(
        call(
            &a,
            "click",
            json!({"pid":42,"window_id":7,"delivery_mode":"foreground"})
        )
        .await["isError"],
        true
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "a forbidden foreground request must not ask for another grant"
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
async fn structured_refusal_normalizes_to_error_and_preserves_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let (socket, bridge) = manager
        .start_bridge(
            "refuse",
            "refuse",
            approval(true, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // Replays the audited noches_session_export browser refusal verbatim: the
    // driver returned status=refused without setting the tool-level
    // execution-error flag, and nested every refusal field.
    let refusal = json!({
        "status": "refused",
        "refusal": {
            "code": "browser_consent_required",
            "message": "this standalone browser profile requires explicit existing-profile approval before Cua can inspect its DevTools endpoint",
            "detail": {
                "next_action": "browser_prepare",
                "reason": "consumer_profile_endpoint_requires_grant",
                "supported_strategies": ["existing_profile"],
            },
        },
    });
    let result = call(&socket, "get_browser_state", json!({"refusal": refusal})).await;
    assert_eq!(
        result["isError"], true,
        "a structured refusal is a failed tool execution: {result}"
    );
    assert_eq!(
        result["structuredContent"], refusal,
        "the nested refusal payload must be preserved verbatim"
    );
    assert_eq!(
        result["content"][0]["text"], "你好",
        "normalization must not replace the driver's evidence"
    );
    bridge.finish().await;
}

#[tokio::test]
async fn effect_refused_normalizes_with_its_driver_error_flag() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let (socket, bridge) = manager
        .start_bridge(
            "effect-refuse",
            "effect-refuse",
            approval(true, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // Replays the audited `effect: refused` trace, which arrived with isError
    // already set; the refusal payload must survive normalization untouched.
    let outcome = json!({
        "code": "background_unavailable",
        "detail": "client_not_qualified",
        "effect": "refused",
        "ok": false,
        "reason": "client_not_qualified",
        "route": "synthetic_events",
        "verified": false,
    });
    let result = call(
        &socket,
        "click",
        json!({"pid":42,"window_id":7,"refusal": outcome, "refuse": true}),
    )
    .await;
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(result["structuredContent"], outcome);
    bridge.finish().await;
}

#[tokio::test]
async fn partial_unknown_and_unverifiable_outcomes_are_not_errors() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let (socket, bridge) = manager
        .start_bridge(
            "uncertain",
            "uncertain",
            approval(true, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for status in ["partial", "unknown", "unverifiable"] {
        let result = call(
            &socket,
            "click",
            json!({"pid":42,"window_id":7,"refusal": {"status": status, "delivery_id": "d1"}}),
        )
        .await;
        assert_ne!(
            result["isError"], true,
            "{status} delivery is uncertain, not a failed execution: {result}"
        );
        assert_eq!(result["structuredContent"]["status"], status);
        assert_eq!(result["structuredContent"]["delivery_id"], "d1");
    }
    // `effect: unverifiable` is the documented outcome-only variant.
    let effect = call(
        &socket,
        "click",
        json!({"pid":42,"window_id":7,"refusal": {"effect": "unverifiable", "delivery_id": "d2"}}),
    )
    .await;
    assert_ne!(effect["isError"], true, "{effect}");
    assert_eq!(effect["structuredContent"]["effect"], "unverifiable");
    bridge.finish().await;
}

#[test]
fn driver_outcome_precedence_is_consistent_for_is_error_and_status() {
    use DriverOutcome::*;
    let cases = [
        (json!({"structuredContent": {"status": "refused"}}), Refused),
        (
            json!({"isError": true, "structuredContent": {"status": "refused"}}),
            Refused,
        ),
        (
            json!({"isError": true, "structuredContent": {"effect": "refused"}}),
            Refused,
        ),
        (
            json!({"isError": true, "structuredContent": {"refused": true}}),
            Refused,
        ),
        (json!({"structuredContent": {"status": "error"}}), Error),
        (json!({"structuredContent": {"status": "failed"}}), Error),
        (json!({"isError": true}), Error),
        (
            json!({"structuredContent": {"status": "cancelled"}}),
            Cancelled,
        ),
        (
            json!({"isError": true, "structuredContent": {"status": "cancelled"}}),
            Cancelled,
        ),
        (json!({"structuredContent": {"status": "partial"}}), Partial),
        (
            json!({"isError": true, "structuredContent": {"status": "partial"}}),
            Error,
        ),
        (json!({"structuredContent": {"status": "unknown"}}), Unknown),
        (
            json!({"isError": true, "structuredContent": {"status": "unknown"}}),
            Error,
        ),
        (
            json!({"structuredContent": {"status": "unverifiable"}}),
            Unverifiable,
        ),
        (
            json!({"isError": true, "structuredContent": {"effect": "unverifiable"}}),
            Error,
        ),
        (
            json!({"structuredContent": {"status": "delivered"}}),
            Delivered,
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(
            classify_driver_outcome(&result),
            expected,
            "result: {result}"
        );
    }
}

#[test]
fn normalizer_promotes_only_refusals_and_errors() {
    let refused = normalize_driver_outcome(json!({
        "structuredContent": {"status": "refused", "refusal": {"code": "denied"}}
    }));
    assert_eq!(refused["isError"], true);
    let uncertain = normalize_driver_outcome(json!({
        "structuredContent": {"effect": "unverifiable", "delivery_id": "d1"}
    }));
    assert!(
        uncertain.get("isError").is_none(),
        "uncertain delivery must stay non-error: {uncertain}"
    );
}

#[test]
fn agent_seat_marker_parses_json_and_legacy_payloads() {
    // Current vendor payload: identity header, then one JSON payload line.
    let marker = parse_agent_seat_marker(concat!(
        "noches-gpui-agent-seat-v1\n4242\n77\n9\n11\n",
        "{\"state\":\"primary_client_busy\",\"reason\":\"physical_seat_present\",\"pid\":4242}\n"
    ))
    .unwrap();
    assert_eq!(marker["state"], "primary_client_busy");
    assert_eq!(marker["reason"], "physical_seat_present");
    assert_eq!(marker["pid"], 4242);

    // Backward compatibility: a bare legacy state token still parses, with
    // no invented reason or pid.
    let legacy =
        parse_agent_seat_marker("noches-gpui-agent-seat-v1\n1\n2\n3\n4\nprimary_client_busy\n")
            .unwrap();
    assert_eq!(legacy["state"], "primary_client_busy");
    assert!(legacy.get("reason").is_none());
    assert!(legacy.get("pid").is_none());
    let ready = parse_agent_seat_marker("ready\n").unwrap();
    assert_eq!(ready["state"], "ready");

    // Empty, unknown-token and malformed payloads are no evidence at all.
    assert_eq!(parse_agent_seat_marker(""), None);
    assert_eq!(parse_agent_seat_marker("\n \n"), None);
    assert_eq!(parse_agent_seat_marker("qualified_somewhere_else\n"), None);
    assert_eq!(
        parse_agent_seat_marker("{\"state\":\"ready\" truncated\n"),
        None
    );
    // JSON without a usable state token is rejected.
    assert_eq!(parse_agent_seat_marker("{\"pid\":1}\n"), None);
}

#[test]
fn refused_result_without_a_marker_is_left_untouched() {
    // No GPUI process owns pid u64::MAX, so the marker read misses and the
    // refusal evidence must survive verbatim.
    let refused = json!({
        "structuredContent": {"status": "refused", "code": "background_unavailable"},
    });
    assert_eq!(
        attach_seat_marker_evidence(refused.clone(), Some(u64::MAX)),
        refused
    );
    // Delivered results never gain a marker, whatever the target is.
    let delivered = json!({"structuredContent": {"status": "delivered"}});
    assert_eq!(
        attach_seat_marker_evidence(delivered.clone(), Some(u64::MAX)),
        delivered
    );
}

#[tokio::test]
async fn help_retains_initialize_metadata_and_exact_executable_hash() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let exe = dir.path().join("driver.py");
    let canonical_exe = std::fs::canonicalize(&exe).unwrap();
    let expected_sha = format!(
        "{:x}",
        Sha256::digest(std::fs::read(&canonical_exe).unwrap())
    );
    let (socket, bridge) = manager
        .start_bridge(
            "metadata",
            "metadata",
            approval(false, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let help = call(&socket, "help", json!({})).await;
    assert_ne!(help["isError"], true, "{help}");
    let driver = &help["structuredContent"]["driver"];
    assert_eq!(driver["protocolVersion"], "2024-11-05");
    assert_eq!(driver["serverInfo"]["name"], "python-fixture");
    assert_eq!(driver["serverInfo"]["version"], "1.2.3");
    assert_eq!(driver["capabilities"]["tools"], json!({}));
    assert_eq!(driver["executable"]["path"], json!(exe.to_string_lossy()));
    assert_eq!(
        driver["executable"]["canonicalPath"],
        json!(canonical_exe.to_string_lossy())
    );
    assert_eq!(driver["executable"]["sha256"], json!(expected_sha));
    assert_eq!(driver["instructions"]["present"], true);
    assert!(driver["instructions"]["bytes"].as_u64().unwrap() > 0);
    assert!(
        !serde_json::to_string(&help)
            .unwrap()
            .contains("FIXTURE_INSTRUCTIONS_SENTINEL"),
        "raw server instructions must not reach the result: {help}"
    );
    // health_report carries the same trusted metadata for diagnostics.
    let health = call(&socket, "health_report", json!({})).await;
    assert_ne!(health["isError"], true, "{health}");
    assert_eq!(
        health["structuredContent"]["driver"]["executable"]["sha256"],
        json!(expected_sha)
    );
    bridge.finish().await;
}

#[tokio::test]
async fn invalid_initialize_metadata_fails_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_with_initialize(
        dir.path(),
        r#"{"serverInfo":{"name":"fixture"},"capabilities":{}}"#,
    );
    let (socket, bridge) = manager
        .start_bridge(
            "bad-init",
            "bad-init",
            approval(true, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let result = call(&socket, "list_windows", json!({})).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("protocolVersion"),
        "handshake failure must name the invalid field: {text}"
    );
    bridge.finish().await;
}

#[tokio::test]
async fn unsupported_negotiated_protocol_version_fails_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_with_initialize(
        dir.path(),
        r#"{"protocolVersion":"1999-01-01","serverInfo":{"name":"python-fixture","version":"1.2.3"},"capabilities":{"tools":{"listChanged":false}}}"#,
    );
    let (socket, bridge) = manager
        .start_bridge(
            "bad-protocol",
            "bad-protocol",
            approval(true, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let result = call(&socket, "help", json!({})).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("unsupported MCP protocol version 1999-01-01"),
        "handshake failure must name the negotiated version: {text}"
    );
    assert!(
        text.contains("2025-06-18"),
        "the failure must name a supported version: {text}"
    );
    bridge.finish().await;
}

#[tokio::test]
async fn symlinked_driver_reports_invocation_and_canonical_executable_paths() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("driver.py");
    let invocation = dir.path().join("driver-link");
    let mut manager = manager(dir.path());
    std::os::unix::fs::symlink(&target, &invocation).unwrap();
    manager.driver_path = Some(invocation.clone());
    let canonical = std::fs::canonicalize(&target).unwrap();
    let expected_sha = format!("{:x}", Sha256::digest(std::fs::read(&target).unwrap()));

    let (socket, bridge) = manager
        .start_bridge(
            "symlink",
            "symlink",
            approval(false, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let help = call(&socket, "help", json!({})).await;
    assert_ne!(help["isError"], true, "{help}");
    let executable = &help["structuredContent"]["driver"]["executable"];
    assert_eq!(executable["path"], json!(invocation.to_string_lossy()));
    assert_eq!(
        executable["canonicalPath"],
        json!(canonical.to_string_lossy()),
        "canonical target must be reported, not only the symlink path"
    );
    assert_eq!(
        executable["sha256"],
        json!(expected_sha),
        "the digest must cover the symlink target"
    );
    bridge.finish().await;
}

#[tokio::test]
async fn stable_tool_list_is_cached_with_contract_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let (socket, bridge) = manager
        .start_bridge(
            "cache",
            "cache",
            approval(false, Arc::new(AtomicUsize::new(0))),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let help = call(&socket, "help", json!({})).await;
    assert_ne!(help["isError"], true, "{help}");
    let structured = &help["structuredContent"];
    assert_eq!(structured["schema_version"], "1");
    assert_eq!(structured["capability_version"], "1");
    assert_eq!(
        structured["enforcement_adapters"][0]["id"],
        "fixture.adapter"
    );
    assert_eq!(request_count(dir.path(), "tools/list"), 1);

    let describe = call(
        &socket,
        "describe",
        json!({"names": ["list_windows", "click"]}),
    )
    .await;
    assert_ne!(describe["isError"], true, "{describe}");
    assert_eq!(describe["structuredContent"]["schema_version"], "1");
    assert_eq!(describe["structuredContent"]["capability_version"], "1");
    assert_eq!(
        describe["structuredContent"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let help_again = call(&socket, "help", json!({})).await;
    assert_ne!(help_again["isError"], true, "{help_again}");
    assert_eq!(
        request_count(dir.path(), "tools/list"),
        1,
        "an absent listChanged defaults to false and reuses one tools/list per live driver"
    );

    // The cache belongs to the driver, not the bridge: a fresh turn on the
    // same bridge rediscovers the schema.
    bridge.turn_ended().await;
    bridge.turn_started();
    let restarted = call(&socket, "help", json!({})).await;
    assert_ne!(restarted["isError"], true, "{restarted}");
    assert_eq!(
        request_count(dir.path(), "tools/list"),
        2,
        "a fresh driver must discover tools/list again"
    );
    bridge.finish().await;
}

#[tokio::test]
async fn tool_list_capability_semantics_drive_the_cache() {
    // MCP omits `listChanged` when the server cannot change its list, so the
    // real cua-driver `capabilities: {tools: {}}` handshake caches. An absent
    // tools capability or `listChanged: true` must keep discovering.
    for (capabilities, expected) in [
        (json!({"tools": {"listChanged": true}}), 2),
        (json!({}), 2),
        (json!({"tools": {"listChanged": false}}), 1),
        (json!({"tools": {}}), 1),
    ] {
        let label = capabilities.to_string();
        let dir = tempfile::tempdir().unwrap();
        let initialize = json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": {"name": "python-fixture", "version": "1.2.3"},
            "capabilities": capabilities.clone(),
        })
        .to_string();
        let manager = manager_with_initialize(dir.path(), &initialize);
        let (socket, bridge) = manager
            .start_bridge(
                "capability",
                "capability",
                approval(false, Arc::new(AtomicUsize::new(0))),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            let help = call(&socket, "help", json!({})).await;
            assert_ne!(help["isError"], true, "{help}");
        }
        assert_eq!(
            request_count(dir.path(), "tools/list"),
            expected,
            "capabilities {label} must produce {expected} tools/list requests"
        );
        bridge.finish().await;
    }
}

#[tokio::test]
async fn running_executable_identity_and_hash_follow_the_executed_image() {
    let current = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let expected = format!("{:x}", Sha256::digest(std::fs::read(&current).unwrap()));
    let pid = std::process::id();
    assert_eq!(
        running_executable_sha256(pid, &current).await.unwrap(),
        expected,
        "the reported digest must come from the running image"
    );
    let other = current.with_file_name("definitely-not-the-running-binary");
    let error = running_executable_sha256(pid, &other).await.unwrap_err();
    assert!(
        error.contains("instead of"),
        "a process running another file must be refused: {error}"
    );
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
        .write_all(b"{\"action\":\"click\",\"args\":{\"pid\":42,\"window_id\":7,\"block\":true}}\n")
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
    let res1 = call(&socket1, "click", json!({"pid": 42,"window_id":7})).await;
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
    let res2 = call(&socket2, "click", json!({"pid": 42,"window_id":7})).await;
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
    let res3 = call(&socket3, "click", json!({"pid": 42,"window_id":7})).await;
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
    let res = call(&socket, "click", json!({"pid": 10,"window_id":7})).await;
    assert_eq!(res["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Second action in same bridge uses cached approval without prompt.
    let res2 = call(&socket, "click", json!({"pid": 10,"window_id":7})).await;
    assert_eq!(res2["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Revoke approval synchronously.
    assert!(manager.forget_computer_use_approval("chat-revoke"));
    assert!(!manager.forget_computer_use_approval("chat-revoke"));

    // Existing bridge's next non-metadata action prompts again.
    let res3 = call(&socket, "click", json!({"pid": 10,"window_id":7})).await;
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

    let res4 = call(&socket2, "click", json!({"pid": 10,"window_id":7})).await;
    assert_eq!(res4["isError"], false);
    assert_eq!(count.load(Ordering::SeqCst), 3);

    bridge2.finish().await;
}

fn assert_match_pointer_digest(text: &Value) {
    let s = text.as_str().unwrap();
    assert!(
        s.contains(NON_DISRUPTIVE_POLICY),
        "expected strict policy in {s}"
    );
    assert!(!s.contains("disruption approval"));
}

#[tokio::test]
async fn strict_policy_denies_before_driver_lease_and_approval_even_after_grant() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let (socket, bridge) = manager
        .start_bridge(
            "strict",
            "strict",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut denied = vec![
        (
            "click",
            json!({"pid":42,"window_id":7,"delivery_mode":" ForeGround "}),
        ),
        (
            "click",
            json!({"pid":42,"window_id":7,"delivery_mode":"automatic"}),
        ),
        (
            "click",
            json!({"pid":42,"window_id":7,"delivery_mode":null}),
        ),
        (
            "click",
            json!({"pid":42,"window_id":7,"allow_user_input_disruption":true}),
        ),
        ("list_windows", json!({"allow_user_input_disruption":false})),
        ("click", json!({"pid":42})),
        ("click", json!({"pid":42,"window_id":0})),
        ("click", json!({"pid":42,"window_id":7,"scope":"automatic"})),
        (
            "click",
            json!({"pid":42,"window_id":7,"target":{"kind":"window","window_id":8}}),
        ),
        (
            "click",
            json!({"pid":42,"window_id":7,"target":{"kind":"unknown"}}),
        ),
        ("click", json!({"pid":42,"window_id":7,"display_id":"DP-1"})),
        (
            "browser_prepare",
            json!({"pid":42,"window_id":7,"strategy":{"kind":"existing_profile"},"allow_launch":false}),
        ),
        (
            "browser_prepare",
            json!({"pid":42,"attach_only":true,"auto_setup":false}),
        ),
        (
            "browser_prepare",
            json!({"allow_launch":true,"profile":{"mode":"isolated_new"}}),
        ),
        (
            "browser_dialog",
            json!({"action":"accept","delivery_mode":"background"}),
        ),
        ("browser_dialog", json!({"action":"dismiss"})),
        ("browser_activate", json!({})),
        ("get_desktop_state", json!({"delivery_mode":"FOREGROUND"})),
    ];
    for action in WINDOW_INPUT_ACTIONS.iter().chain(["move_cursor"].iter()) {
        denied.push((*action, json!({"pid":42,"window_id":7,"scope":"DeSkToP"})));
        denied.push((
            *action,
            json!({"pid":42,"window_id":7,"target":{"kind":" Desktop "}}),
        ));
    }
    for action in BLOCKED_ACTIONS {
        denied.push((
            *action,
            json!({"pid":42,"window_id":7,"delivery_mode":"background"}),
        ));
    }
    for granted in [false, true] {
        let before = request_count(dir.path(), "tools/call");
        for (action, args) in &denied {
            let result = call(&socket, action, args.clone()).await;
            assert_eq!(result["isError"], true, "{action} {args}: {result}");
        }
        assert_eq!(
            request_count(dir.path(), "tools/call"),
            before,
            "denials must not dispatch"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            usize::from(granted),
            "denials must not ask approval"
        );
        if !granted {
            assert_eq!(
                request_count(dir.path(), "initialize"),
                0,
                "denials must not start a driver"
            );
            assert!(
                lock(&manager.lease).is_none(),
                "denials must not take the lease"
            );
            assert_eq!(
                call(&socket, "list_windows", json!({})).await["isError"],
                false
            );
        }
    }
    bridge.finish().await;
}

#[tokio::test]
async fn supported_background_calls_and_read_only_desktop_capture_reach_driver() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let (socket, bridge) = manager
        .start_bridge(
            "safe",
            "safe",
            approval(true, count.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for action in WINDOW_INPUT_ACTIONS {
        let args = json!({"pid":42,"window_id":7,"delivery_mode":" BackGROUND ","x":10,"y":20,"text":"hello","key":"a","keys":["ctrl","a"],"direction":"down","from_x":10,"from_y":20,"to_x":30,"to_y":40});
        let result = call(&socket, action, args).await;
        assert_eq!(result["isError"], false, "{action}: {result}");
        assert_eq!(
            result["structuredContent"]["args"]["delivery_mode"],
            "background"
        );
    }
    let result = call(&socket, "click", json!({"pid":42,"window_id":7})).await;
    assert_eq!(
        result["structuredContent"]["args"]["delivery_mode"], "background",
        "omitted delivery must be pinned, not delegated to driver defaults"
    );
    for (action, args) in [
        (
            "get_desktop_state",
            json!({"scope":"DESKTOP","display_id":"DP-1"}),
        ),
        ("get_screen_size", json!({})),
        ("clipboard_read", json!({})),
        (
            "browser_navigate",
            json!({"target_id":"b1","tab_id":"t1","url":"https://example.com"}),
        ),
        (
            "browser_dialog",
            json!({"target_id":"b1","tab_id":"t1","action":"inspect"}),
        ),
        (
            "set_value",
            json!({"pid":42,"window_id":7,"element_token":"s1:1","value":"hello"}),
        ),
        ("move_cursor", json!({"pid":42,"window_id":7,"x":10,"y":20})),
    ] {
        assert_eq!(
            call(&socket, action, args).await["isError"],
            false,
            "{action}"
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    bridge.finish().await;
}

#[test]
fn managed_schemas_remove_driver_foreground_fallback_advice() {
    let schema = managed_schema(
        json!({"name":"click","description":"Retry foreground", "inputSchema":{"properties":{"delivery_mode":{"description":"Activate first", "enum":["foreground","background"]},"scope":{"enum":["window","desktop"]}, "key":{"description":"Try bring_to_front"}}}}),
    );
    assert_eq!(schema["description"], NON_DISRUPTIVE_POLICY);
    assert_eq!(
        schema["inputSchema"]["properties"]["delivery_mode"]["enum"],
        json!(["background"])
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["scope"]["enum"],
        json!(["window"])
    );
    assert!(
        schema["inputSchema"]["properties"]["key"]
            .get("description")
            .is_none()
    );
    let dialog = managed_schema(
        json!({"name":"browser_dialog", "inputSchema":{"properties":{"action":{"enum":["inspect","accept","dismiss"]}}}}),
    );
    assert_eq!(
        dialog["inputSchema"]["properties"]["action"]["enum"],
        json!(["inspect"])
    );
    let question = approval_question("host", "click");
    assert!(question.contains("forbidden even after approval"));
    assert!(!question.contains("including visible desktop control"));
}
