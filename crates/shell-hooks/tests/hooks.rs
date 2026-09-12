#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    },
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;
use zeron_shell_hooks::*;

fn binding(root: &Path, id: &str, provider: Provider) -> Binding {
    Binding {
        session_id: id.into(),
        terminal_id: format!("terminal-{id}"),
        worktree_path: root.canonicalize().unwrap(),
        provider,
        provider_session_id: None,
    }
}

fn request(reg: &Registration, id: &str, name: &str, payload: Value) -> Vec<u8> {
    serde_json::to_vec(&HookEnvelope {
        binding: reg.binding.clone(),
        event_id: id.into(),
        event_name: name.into(),
        payload,
    })
    .unwrap()
}

fn event(
    receiver: &mut HookReceiver,
    reg: &Registration,
    id: &str,
    name: &str,
    payload: Value,
) -> AcceptedEvent {
    match receiver
        .handle(Peer::Unix, &reg.token, &request(reg, id, name, payload))
        .unwrap()
    {
        Outcome::Accepted(e) => e,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn dialects_do_not_confuse_tool_steps_with_idle_or_permission() {
    for (name, kind) in [
        ("UserPromptSubmit", TurnEvent::PromptSubmitted),
        ("user_prompt_submit", TurnEvent::PromptSubmitted),
        ("beforeSubmitPrompt", TurnEvent::PromptSubmitted),
        ("userPromptSubmit", TurnEvent::PromptSubmitted),
        ("Stop", TurnEvent::TurnCompleted),
        ("turn-complete", TurnEvent::TurnCompleted),
        ("agent-turn-complete", TurnEvent::TurnCompleted),
        ("agent_settled", TurnEvent::TurnCompleted),
        ("session.idle", TurnEvent::TurnCompleted),
        ("after_agent_response", TurnEvent::ResponseCompleted),
        ("SubagentStop", TurnEvent::SubagentStopped),
        ("PermissionRequest", TurnEvent::PermissionRequested),
        ("permission.asked", TurnEvent::PermissionRequested),
        ("permission.replied", TurnEvent::PermissionResolved),
        ("ui_prompt_start", TurnEvent::PermissionRequested),
        ("agent_end", TurnEvent::RunEnded),
        ("turn_end", TurnEvent::TurnStepCompleted),
        ("PreToolUse", TurnEvent::ToolStarted),
    ] {
        assert_eq!(normalize(name), Some(kind), "{name}");
    }
    for unknown in ["made-up-idle", "../../Stop", "停止", ""] {
        assert_eq!(normalize(unknown), None);
    }
}

#[test]
fn authentication_is_exact_and_rejections_do_not_consume_dedupe() {
    let dir = TempDir::new().unwrap();
    let mut receiver = HookReceiver::default();
    let a = receiver
        .register(binding(dir.path(), "a", Provider::Claude))
        .unwrap();
    let b = receiver
        .register(binding(dir.path(), "b", Provider::Claude))
        .unwrap();
    let body = request(&a, "one", "Stop", json!({}));
    assert!(receiver.handle(Peer::Unix, &b.token, &body).is_err());
    assert!(receiver.handle(Peer::Unix, &"0".repeat(64), &body).is_err());
    assert!(
        receiver
            .handle(Peer::Tcp("192.0.2.1".parse().unwrap()), &a.token, &body)
            .is_err()
    );
    for key in ["session_id", "terminal_id", "provider", "worktree_path"] {
        let mut bad: Value = serde_json::from_slice(&body).unwrap();
        bad["binding"][key] = json!(if key == "provider" { "codex" } else { "wrong" });
        assert!(
            receiver
                .handle(Peer::Unix, &a.token, &serde_json::to_vec(&bad).unwrap())
                .is_err()
        );
    }
    assert!(matches!(
        receiver
            .handle(Peer::Tcp("127.0.0.1".parse().unwrap()), &a.token, &body)
            .unwrap(),
        Outcome::Accepted(_)
    ));
    assert_eq!(
        receiver.handle(Peer::Unix, &a.token, &body).unwrap(),
        Outcome::Duplicate
    );
    assert!(matches!(
        receiver
            .handle(Peer::Unix, &b.token, &request(&b, "one", "Stop", json!({})))
            .unwrap(),
        Outcome::Accepted(_)
    ));
    receiver.revoke("a");
    assert!(receiver.handle(Peer::Unix, &a.token, &body).is_err());
}

#[test]
fn native_identity_and_paths_cannot_escape_binding() {
    let dir = TempDir::new().unwrap();
    let worktree = dir.path().join("worktree");
    fs::create_dir(&worktree).unwrap();
    let mut receiver = HookReceiver::default();
    let mut identity = binding(&worktree, "a", Provider::Codex);
    identity.provider_session_id = Some("native-a".into());
    let reg = receiver.register(identity).unwrap();
    for payload in [
        json!({}),
        json!({"thread-id":"native-b"}),
        json!({"thread-id":"native-a", "cwd": dir.path()}),
        json!({"thread-id":"native-a", "cwd":format!("{}/../worktree", worktree.display())}),
    ] {
        assert!(
            receiver
                .handle(
                    Peer::Unix,
                    &reg.token,
                    &request(&reg, "id", "Stop", payload)
                )
                .is_err()
        );
    }
    event(
        &mut receiver,
        &reg,
        "id",
        "Stop",
        json!({"thread-id":"native-a", "cwd":worktree}),
    );
    let mut identity = binding(&worktree, "b", Provider::Pi);
    identity.worktree_path = worktree.join(".");
    assert!(receiver.register(identity).is_err());
    let alias = dir.path().join("alias");
    symlink(&worktree, &alias).unwrap();
    let mut identity = binding(&worktree, "c", Provider::Pi);
    identity.worktree_path = alias;
    assert!(receiver.register(identity).is_err());
}

#[test]
fn background_stops_are_demoted_without_decrementing_live_tasks() {
    let dir = TempDir::new().unwrap();
    let mut receiver = HookReceiver::new(Limits {
        background_per_session: 1,
        ..Limits::default()
    })
    .unwrap();
    let reg = receiver
        .register(binding(dir.path(), "a", Provider::Claude))
        .unwrap();
    event(
        &mut receiver,
        &reg,
        "start",
        "SubagentStart",
        json!({"agent_id":"child"}),
    );
    assert_eq!(
        receiver
            .handle(
                Peer::Unix,
                &reg.token,
                &request(&reg, "start", "SubagentStart", json!({"agent_id":"child"}))
            )
            .unwrap(),
        Outcome::Duplicate
    );
    let stop = event(&mut receiver, &reg, "parent", "Stop", json!({}));
    assert_eq!(stop.kind, TurnEvent::SubagentStopped);
    assert!(stop.demoted);
    assert_eq!(stop.background_tasks, 1);
    event(
        &mut receiver,
        &reg,
        "overflow",
        "SubagentStart",
        json!({"agent_id":"another"}),
    );
    event(
        &mut receiver,
        &reg,
        "end",
        "SubagentStop",
        json!({"agent_id":"child"}),
    );
    assert!(event(&mut receiver, &reg, "parent2", "Stop", json!({})).background_unknown);
    receiver.reconcile_background("a", &[]).unwrap();
    assert_eq!(
        event(&mut receiver, &reg, "parent3", "Stop", json!({})).kind,
        TurnEvent::TurnCompleted
    );
    event(&mut receiver, &reg, "anonymous", "SubagentStart", json!({}));
    assert!(event(&mut receiver, &reg, "parent4", "Stop", json!({})).demoted);
}

#[test]
fn limits_bound_sessions_events_bodies_and_expiration() {
    let dir = TempDir::new().unwrap();
    let mut receiver = HookReceiver::new(Limits {
        sessions: 1,
        dedupe_per_session: 2,
        body_bytes: 1024,
        ..Limits::default()
    })
    .unwrap();
    let reg = receiver
        .register(binding(dir.path(), "a", Provider::Pi))
        .unwrap();
    assert!(
        receiver
            .register(binding(dir.path(), "b", Provider::Pi))
            .is_err()
    );
    for id in ["a", "b", "c", "a"] {
        event(&mut receiver, &reg, id, "Stop", json!({}));
    }
    assert!(
        receiver
            .handle(Peer::Unix, &reg.token, &vec![b' '; 1025])
            .is_err()
    );
    assert_eq!(receiver.session_count(), 1);
    let mut receiver = HookReceiver::new(Limits {
        idle_ttl: Duration::from_millis(1),
        ..Limits::default()
    })
    .unwrap();
    let reg = receiver
        .register(binding(dir.path(), "a", Provider::Pi))
        .unwrap();
    std::thread::sleep(Duration::from_millis(3));
    receiver.prune();
    assert_eq!(receiver.session_count(), 0);
    assert!(
        receiver
            .handle(
                Peer::Unix,
                &reg.token,
                &request(&reg, "a", "Stop", json!({}))
            )
            .is_err()
    );
}

fn options(dir: &Path, custom: bool) -> GenerationOptions {
    GenerationOptions {
        parent_dir: dir.canonicalize().unwrap(), hook_socket: dir.join("hooks.sock"),
        python: "/usr/bin/python3".into(), provider_executable: "/bin/true".into(),
        instructions: "Keep changes in this worktree.\n".into(), history_path: Some(dir.join("history.jsonl")),
        path_prefix: vec![],
        notifier_argv: custom.then(|| vec!["/usr/bin/python3".into(), "-c".into(),
            "import json,sys; e=json.load(sys.stdin); assert sys.argv[2] == \"quote ' $() @ROOT@\"; f=open(sys.argv[1],'a'); f.write(json.dumps(e)+'\\n'); f.close(); sys.exit(1 if e['event_name']=='Poison' else 0)".into(),
            dir.join("received.jsonl").to_string_lossy().into_owned(), "quote ' $() @ROOT@".into()]),
    }
}

fn notify(hooks: &GeneratedHooks, args: &[&str]) -> std::process::Output {
    Command::new("/bin/sh")
        .arg(&hooks.notifier)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn received(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join("received.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn queue_bounds_drains_and_skips_poison_and_symlinks() {
    let dir = TempDir::new().unwrap();
    let mut receiver = HookReceiver::default();
    let reg = receiver
        .register(binding(dir.path(), "a", Provider::Claude))
        .unwrap();
    let hooks = generate(&reg, &options(dir.path(), true)).unwrap();
    fs::create_dir(hooks.directory.join("drain.lock")).unwrap();
    for n in 0..15 {
        assert!(
            notify(
                &hooks,
                &[
                    if n == 0 { "Poison" } else { "Stop" },
                    &format!("{{\"noches_event_id\":\"{n}\"}}")
                ]
            )
            .status
            .success()
        );
    }
    let outside = dir.path().join("outside");
    fs::write(&outside, "untouched").unwrap();
    symlink(
        &outside,
        hooks
            .directory
            .join("queue/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json"),
    )
    .unwrap();
    fs::write(
        hooks
            .directory
            .join("queue/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.json"),
        "not JSON",
    )
    .unwrap();
    fs::write(
        hooks.directory.join("queue/a name;$(exit 9).json"),
        "poison",
    )
    .unwrap();
    fs::remove_dir(hooks.directory.join("drain.lock")).unwrap();
    assert!(notify(&hooks, &["--drain"]).status.success());
    let first = received(dir.path());
    assert_eq!(first.len(), 10, "at most ten notifier invocations");
    for (n, e) in first.iter().enumerate() {
        assert_eq!(e["event_id"], n.to_string());
    }
    assert!(notify(&hooks, &["--drain"]).status.success());
    assert_eq!(received(dir.path()).len(), 15);
    assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
    assert_eq!(
        fs::read_dir(hooks.directory.join("queue")).unwrap().count(),
        0
    );
    assert_eq!(
        fs::read_dir(hooks.directory.join("errors"))
            .unwrap()
            .count(),
        4
    );
    for n in 0..40 {
        assert!(
            notify(
                &hooks,
                &["Poison", &format!("{{\"noches_event_id\":\"bad{n}\"}}")]
            )
            .status
            .success()
        );
    }
    assert_eq!(
        fs::read_dir(hooks.directory.join("errors"))
            .unwrap()
            .count(),
        32
    );
    assert!(!notify(&hooks, &["../../outside", "{}"]).status.success());
    assert!(!notify(&hooks, &["Stop", "not-json"]).status.success());
}

#[test]
fn generated_assets_are_scoped_private_and_quote_hostile_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("quote ' $() @ROOT@");
    fs::create_dir(&dir).unwrap();
    let mut receiver = HookReceiver::default();
    for provider in [
        Provider::Claude,
        Provider::Pi,
        Provider::Codex,
        Provider::OpenCode,
        Provider::Cursor,
        Provider::Kiro,
    ] {
        let reg = receiver
            .register(binding(&dir, provider.as_str(), provider))
            .unwrap();
        let hooks = generate(&reg, &options(&dir, true)).unwrap();
        assert_eq!(
            fs::metadata(&hooks.directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for asset in &hooks.assets {
            assert!(asset.starts_with(&hooks.directory));
            assert_eq!(fs::metadata(asset).unwrap().permissions().mode() & 0o077, 0);
            if asset.extension().is_some_and(|x| x == "json") {
                serde_json::from_slice::<Value>(&fs::read(asset).unwrap()).unwrap();
            }
        }
        assert!(
            Command::new("/bin/sh")
                .arg("-n")
                .arg(&hooks.launcher)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("/bin/sh")
                .arg("-n")
                .arg(&hooks.notifier)
                .status()
                .unwrap()
                .success()
        );
        assert!(notify(&hooks, &["Stop", "{}"]).status.success());
        assert_eq!(
            hooks.automatic_activation,
            !matches!(provider, Provider::Kiro | Provider::OpenCode)
        );
    }
    assert_eq!(received(&dir).len(), 6);
    assert!(!dir.join(".claude").exists());
    assert!(!dir.join(".kiro").exists());
    assert!(!dir.join(".cursor").exists());
}

#[test]
fn default_sender_reaches_authenticated_uds_handler() {
    let dir = TempDir::new().unwrap();
    let mut receiver = HookReceiver::default();
    let reg = receiver
        .register(binding(dir.path(), "a", Provider::Claude))
        .unwrap();
    let opts = options(dir.path(), false);
    let server = UnixListener::bind(&opts.hook_socket).unwrap();
    let hooks = generate(&reg, &opts).unwrap();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut byte = [0];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() < 8192);
        }
        let headers = String::from_utf8(bytes).unwrap();
        assert!(headers.starts_with("POST /hooks HTTP/1.1\r\n"));
        let token = headers
            .lines()
            .find_map(|l| l.strip_prefix("Authorization: Bearer "))
            .unwrap();
        let length: usize = headers
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert!(length <= 65536);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let outcome = receiver.handle(Peer::Unix, token, &body).unwrap();
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        outcome
    });
    assert!(notify(&hooks, &["UserPromptSubmit", "{}"]).status.success());
    assert!(matches!(
        worker.join().unwrap(),
        Outcome::Accepted(AcceptedEvent {
            kind: TurnEvent::PromptSubmitted,
            ..
        })
    ));
}

#[test]
fn codex_notify_adapter_reuses_native_turn_id() {
    let dir = TempDir::new().unwrap();
    let mut receiver = HookReceiver::default();
    let reg = receiver
        .register(binding(dir.path(), "a", Provider::Codex))
        .unwrap();
    let hooks = generate(&reg, &options(dir.path(), true)).unwrap();
    for _ in 0..2 {
        assert!(
            Command::new("/usr/bin/python3")
                .arg(hooks.directory.join("codex-watcher.py"))
                .arg(
                    json!({"type":"agent-turn-complete", "thread-id":"native", "turn-id":"turn"})
                        .to_string()
                )
                .env("NOCHES_NOTIFY", &hooks.notifier)
                .status()
                .unwrap()
                .success()
        );
    }
    let events = received(dir.path());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event_id"], events[1]["event_id"]);
    assert!(matches!(
        receiver
            .handle(
                Peer::Unix,
                &reg.token,
                &serde_json::to_vec(&events[0]).unwrap()
            )
            .unwrap(),
        Outcome::Accepted(_)
    ));
    assert_eq!(
        receiver
            .handle(
                Peer::Unix,
                &reg.token,
                &serde_json::to_vec(&events[1]).unwrap()
            )
            .unwrap(),
        Outcome::Duplicate
    );
}
