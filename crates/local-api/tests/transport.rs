#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;
use zeron_local_api::{
    Client, ControlPlane, Manifest, PROTOCOL_VERSION, Request, SocketIdentity, discover_instances,
    is_instance_locked,
};

fn temp() -> TempDir {
    tempfile::Builder::new()
        .prefix("noches-")
        .tempdir_in("/tmp")
        .unwrap()
}

async fn start() -> (TempDir, ControlPlane, mpsc::UnboundedReceiver<Request>) {
    let dir = temp();
    let (sender, receiver) = mpsc::unbounded_channel();
    let api = ControlPlane::start(dir.path().into(), "test".into(), sender)
        .await
        .unwrap();
    (dir, api, receiver)
}

fn write_manifest(dir: &Path, manifest: &Manifest) {
    let path = dir
        .join("local-api")
        .join(format!("{}.json", manifest.instance_name));
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    serde_json::to_writer(file, manifest).unwrap();
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn stale(dir: &Path, pid: u32) -> Manifest {
    let api_dir = dir.join("local-api");
    fs::create_dir(&api_dir).unwrap();
    fs::set_permissions(&api_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let path = api_dir.join("test.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let meta = fs::metadata(&path).unwrap();
    let manifest = Manifest {
        protocol_version: PROTOCOL_VERSION,
        instance_id: Uuid::new_v4(),
        instance_name: "test".into(),
        pid,
        socket: SocketIdentity {
            path,
            device: meta.dev(),
            inode: meta.ino(),
            uid: meta.uid(),
        },
    };
    write_manifest(dir, &manifest);
    drop(listener);
    manifest
}

#[tokio::test]
async fn unix_health_forwarding_errors_and_cleanup() {
    let (dir, api, mut receiver) = start().await;
    let expected = api.manifest().clone();
    assert_eq!(
        fs::metadata(dir.path().join("local-api")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(api.socket_path()).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(dir.path().join("local-api/test.json"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    let client = Client::discover(dir.path(), None, None).await.unwrap();
    assert_eq!(client.health().await.unwrap(), expected);
    let bridge = tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            let response = if request.method == "agent.stop" {
                Err("target not found".into())
            } else {
                Ok(json!({"method": request.method, "params": request.params}))
            };
            let _ = request.reply.send(response);
        }
    });
    assert_eq!(
        client
            .request("tab.split-view", json!({"direction":"right", "ui":"chat"}))
            .await
            .unwrap(),
        json!({"method":"tab.split-view", "params":{"direction":"right", "ui":"chat"}})
    );
    assert_eq!(
        client
            .get("coordination-state.get", json!({"key":"a/b", "version":7}))
            .await
            .unwrap(),
        json!({"method":"coordination-state.get", "params":{"key":"a/b", "version":7}})
    );
    let error = client
        .request("agent.stop", json!({"to":"stable-id"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("422") && error.contains("target not found"),
        "{error}"
    );
    let raw = reqwest::Client::builder()
        .unix_socket(api.socket_path().to_owned())
        .no_proxy()
        .build()
        .unwrap();
    let rejected = raw
        .post("http://localhost/api/v1/agent/send")
        .header("x-noches-instance", Uuid::new_v4().to_string())
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 409);
    let invalid = raw
        .post("http://localhost/api/v1/agent/send")
        .json(&json!([]))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert_eq!(
        raw.get("http://localhost/api/v1/agent/send")
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    api.shutdown().await.unwrap();
    assert!(!expected.socket.path.exists());
    assert!(!dir.path().join("local-api/test.json").exists());
    assert!(dir.path().join("local-api/test.lock").exists());
    bridge.await.unwrap();
}

#[tokio::test]
async fn unavailable_bridge_is_not_success() {
    let (_dir, api, receiver) = start().await;
    drop(receiver);
    let client = Client::connect(api.socket_path()).await.unwrap();
    let error = client
        .request("layout.compose", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("503") && error.contains("bridge_closed"),
        "{error}"
    );
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn subscriptions_forward_and_filter_buffered_events() {
    let (_dir, api, mut receiver) = start().await;
    let hub = api.events();
    let bridge = tokio::spawn(async move {
        let request = receiver.recv().await.unwrap();
        assert_eq!(request.method, "agent.subscribe");
        assert_eq!(request.params, json!({"to":"stable-id"}));
        hub.publish("different-target", json!({"private":"not requested"}));
        hub.publish("agent:stable-id", json!({"version":1}));
        request
            .reply
            .send(Ok(
                json!({"topics":["agent:stable-id"],"snapshot":{"version":0}}),
            ))
            .unwrap();
    });
    let client = Client::connect(api.socket_path()).await.unwrap();
    let mut stream = client
        .subscribe("agent.subscribe", json!({"to":"stable-id"}))
        .await
        .unwrap();
    let ready = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ready.event, "ready");
    assert_eq!(
        serde_json::from_str::<Value>(&ready.data).unwrap()["snapshot"],
        json!({"version":0})
    );
    let message = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(message.event, "message");
    assert_eq!(
        serde_json::from_str::<Value>(&message.data).unwrap(),
        json!({"sequence":2,"topic":"agent:stable-id","data":{"version":1}})
    );
    api.shutdown().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
    bridge.await.unwrap();
}

#[tokio::test]
async fn stale_socket_requires_dead_pid_and_matching_identity() {
    let dir = temp();
    let mut manifest = stale(dir.path(), std::process::id());
    let (sender, _receiver) = mpsc::unbounded_channel();
    let error = ControlPlane::start(dir.path().into(), "test".into(), sender.clone())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("PID is alive"), "{error}");
    assert!(manifest.socket.path.exists());
    manifest.pid = dead_pid();
    manifest.socket.inode += 1;
    write_manifest(dir.path(), &manifest);
    let error = ControlPlane::start(dir.path().into(), "test".into(), sender.clone())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("mismatched manifest identity"), "{error}");
    manifest.socket.inode -= 1;
    write_manifest(dir.path(), &manifest);
    let api = ControlPlane::start(dir.path().into(), "test".into(), sender)
        .await
        .unwrap();
    assert_ne!(api.manifest().instance_id, manifest.instance_id);
    Client::connect(api.socket_path()).await.unwrap();
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn orphan_socket_is_never_removed() {
    let dir = temp();
    let manifest = stale(dir.path(), dead_pid());
    fs::remove_file(dir.path().join("local-api/test.json")).unwrap();
    let (sender, _receiver) = mpsc::unbounded_channel();
    let error = ControlPlane::start(dir.path().into(), "test".into(), sender)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("without an identity manifest"), "{error}");
    assert!(manifest.socket.path.exists());
}

#[tokio::test]
async fn discovery_rejects_changed_uuid_and_override_ignores_manifest() {
    let (dir, api, _receiver) = start().await;
    let mut altered = api.manifest().clone();
    altered.instance_id = Uuid::new_v4();
    write_manifest(dir.path(), &altered);
    let listed = discover_instances(dir.path()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0]
            .error
            .as_ref()
            .unwrap()
            .contains("health identity does not match manifest")
    );
    assert!(Client::discover(dir.path(), None, None).await.is_err());
    let explicit = Client::discover(Path::new("/does/not/exist"), Some(api.socket_path()), None)
        .await
        .unwrap();
    assert_eq!(explicit.manifest(), api.manifest());
    write_manifest(dir.path(), api.manifest());
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_listener_is_preserved_even_with_dead_manifest_pid() {
    let dir = temp();
    let mut manifest = stale(dir.path(), dead_pid());
    fs::remove_file(&manifest.socket.path).unwrap();
    let listener = tokio::net::UnixListener::bind(&manifest.socket.path).unwrap();
    fs::set_permissions(&manifest.socket.path, fs::Permissions::from_mode(0o600)).unwrap();
    let meta = fs::metadata(&manifest.socket.path).unwrap();
    manifest.socket.inode = meta.ino();
    write_manifest(dir.path(), &manifest);
    let identity = manifest.clone();
    let router = axum::Router::new().route(
        "/healthz",
        axum::routing::get(move || async move { axum::Json(identity) }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (sender, _receiver) = mpsc::unbounded_channel();
    let error = ControlPlane::start(dir.path().into(), "test".into(), sender)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("already responds"), "{error}");
    assert_eq!(
        Client::connect(&manifest.socket.path)
            .await
            .unwrap()
            .manifest(),
        &manifest
    );
    server.abort();
}

#[tokio::test]
async fn active_registration_and_replacement_cleanup_are_safe() {
    let (dir, api, _receiver) = start().await;
    let (sender, _second) = mpsc::unbounded_channel();
    let error = ControlPlane::start(dir.path().into(), "test".into(), sender)
        .await
        .err()
        .unwrap();
    assert!(is_instance_locked(&error), "{error}");
    assert!(error.to_string().contains("locked"), "{error}");
    let mut replacement = api.manifest().clone();
    fs::remove_file(api.socket_path()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(api.socket_path()).unwrap();
    fs::set_permissions(api.socket_path(), fs::Permissions::from_mode(0o600)).unwrap();
    replacement.socket.inode = fs::metadata(api.socket_path()).unwrap().ino();
    replacement.instance_id = Uuid::new_v4();
    write_manifest(dir.path(), &replacement);
    api.shutdown().await.unwrap();
    assert!(replacement.socket.path.exists());
    let saved: Manifest =
        serde_json::from_slice(&fs::read(dir.path().join("local-api/test.json")).unwrap()).unwrap();
    assert_eq!(saved, replacement);
}

#[tokio::test]
async fn report_get_and_mutation_streams_never_reach_the_bridge() {
    let (_dir, api, mut receiver) = start().await;
    let raw = reqwest::Client::builder().unix_socket(api.socket_path().to_owned()).no_proxy().build().unwrap();
    assert_eq!(raw.get("http://localhost/api/v1/team/report").send().await.unwrap().status(),405);
    assert_eq!(raw.post("http://localhost/api/v1/team/run").header("accept","text/event-stream")
        .json(&json!({})).send().await.unwrap().status(),400);
    assert!(matches!(receiver.try_recv(),Err(mpsc::error::TryRecvError::Empty)));
    api.shutdown().await.unwrap();
}
