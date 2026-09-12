use std::{
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::Value;

use crate::{
    INSTANCE_HEADER, Manifest, PROTOCOL_VERSION,
    identity::{private_dir, read_manifest, socket_identity},
    server::validate_method,
};

pub type ServerEvent = eventsource_stream::Event;
pub type SubscriptionStream = Pin<Box<dyn Stream<Item = Result<ServerEvent>> + Send>>;

fn transport(socket: &Path) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .unix_socket(socket.to_owned())
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .build()?)
}

pub(crate) async fn raw_health(socket: &Path) -> Result<Manifest> {
    Ok(transport(socket)?
        .get("http://localhost/healthz")
        .timeout(Duration::from_secs(2))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    manifest: Manifest,
}

impl Client {
    /// Explicit socket selection still verifies health against the actual socket inode.
    pub async fn connect(socket: impl AsRef<Path>) -> Result<Self> {
        let socket = socket.as_ref();
        let filename = socket.file_name().context("socket path has no filename")?;
        let parent = socket
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let socket = private_dir(parent, false)?.join(filename);
        let identity = socket_identity(&socket)?;
        let manifest = raw_health(&socket).await?;
        ensure!(
            manifest.protocol_version == PROTOCOL_VERSION
                && !manifest.instance_id.is_nil()
                && crate::identity::valid_name(&manifest.instance_name)
                && manifest.pid > 0
                && manifest.pid <= i32::MAX as u32
                && manifest.socket == identity
                && socket_identity(&socket)? == identity,
            "health identity does not match socket"
        );
        Ok(Self {
            http: transport(&socket)?,
            manifest,
        })
    }

    /// An explicit override never reads data_dir or its manifests.
    pub async fn discover(
        data_dir: &Path,
        socket: Option<&Path>,
        instance: Option<&str>,
    ) -> Result<Self> {
        if let Some(socket) = socket {
            let client = Self::connect(socket).await?;
            if let Some(name) = instance {
                ensure!(
                    client.manifest.instance_name == name,
                    "explicit socket instance name mismatch"
                );
            }
            return Ok(client);
        }
        let instances = discover_instances(data_dir).await?;
        let mut candidates = instances.into_iter().filter(|i| {
            i.error.is_none() && instance.is_none_or(|n| i.manifest.instance_name == n)
        });
        let selected = candidates
            .next()
            .context("no healthy matching instance; launch the app or use --socket PATH")?;
        ensure!(
            candidates.next().is_none(),
            "multiple healthy instances; select --instance NAME or --socket PATH"
        );
        let client = Self::connect(&selected.manifest.socket.path).await?;
        ensure!(
            client.manifest == selected.manifest,
            "instance changed during discovery"
        );
        Ok(client)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub async fn health(&self) -> Result<Manifest> {
        let current = raw_health(&self.manifest.socket.path).await?;
        ensure!(
            current == self.manifest && socket_identity(&current.socket.path)? == current.socket,
            "instance identity changed"
        );
        Ok(current)
    }

    fn call(&self, method: &str, params: &Value, get: bool) -> Result<reqwest::RequestBuilder> {
        let (domain, verb) = validate_method(method)?;
        ensure!(params.is_object(), "params must be a JSON object");
        let url = format!("http://localhost/api/v1/{domain}/{verb}");
        let request = if get {
            self.http.get(url).query(&[("params", params.to_string())])
        } else {
            self.http.post(url).json(params)
        };
        Ok(request.header(INSTANCE_HEADER, self.manifest.instance_id.to_string()))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, params, false).await
    }

    pub async fn get(&self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, params, true).await
    }

    async fn request_inner(&self, method: &str, params: Value, get: bool) -> Result<Value> {
        let response = self
            .call(method, &params, get)?
            .timeout(Duration::from_secs(35))
            .send()
            .await?;
        let status = response.status();
        let envelope: Value = response.json().await?;
        if !status.is_success() || envelope.get("ok") != Some(&Value::Bool(true)) {
            bail!("HTTP {status}: {envelope}");
        }
        envelope
            .get("result")
            .cloned()
            .context("response has no result")
    }

    pub async fn subscribe(&self, method: &str, params: Value) -> Result<SubscriptionStream> {
        let response = tokio::time::timeout(
            Duration::from_secs(35),
            self.call(method, &params, false)?
                .header("accept", "text/event-stream")
                .send(),
        )
        .await??;
        let status = response.status();
        if !status.is_success() {
            let body = tokio::time::timeout(Duration::from_secs(2), response.text()).await??;
            bail!("HTTP {status}: {body}");
        }
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| h.split(';').next() == Some("text/event-stream")),
            "expected SSE response"
        );
        Ok(Box::pin(
            response
                .bytes_stream()
                .eventsource()
                .map(|event| event.map_err(Into::into)),
        ))
    }
}

#[derive(Debug, Serialize)]
pub struct InstanceStatus {
    pub manifest: Manifest,
    /// None means health matched the complete manifest, not just its PID.
    pub error: Option<String>,
}

/// Discovery is read-only. Stale records remain visible and are never selected.
pub async fn discover_instances(data_dir: &Path) -> Result<Vec<InstanceStatus>> {
    let path = data_dir.join("local-api");
    if let Err(e) = std::fs::symlink_metadata(&path) {
        if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(Vec::new());
        }
        return Err(e.into());
    }
    let dir = private_dir(&path, false)?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    paths.sort();
    let mut instances = Vec::new();
    for path in paths
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
    {
        let manifest = read_manifest(&path)?;
        let error = match Client::connect(&manifest.socket.path).await {
            Ok(client) if client.manifest == manifest => None,
            Ok(_) => Some("health identity does not match manifest".into()),
            Err(e) => Some(e.to_string()),
        };
        instances.push(InstanceStatus { manifest, error });
    }
    Ok(instances)
}
