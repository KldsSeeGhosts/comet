use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{INSTANCE_HEADER, Manifest, identity::Registration};

/// Drain this receiver on the UI thread. Resolve reply only after the operation has run.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<std::result::Result<Value, String>>,
}

/// A subscribe/watch bridge reply. Topic names are opaque and chosen by the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub topics: Vec<String>,
    #[serde(default)]
    pub snapshot: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublishedEvent {
    pub sequence: u64,
    pub topic: String,
    pub data: Value,
}

struct HubInner {
    sequence: u64,
    sender: broadcast::Sender<PublishedEvent>,
}

#[derive(Clone)]
pub struct EventHub(Arc<Mutex<HubInner>>);

impl Default for EventHub {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HubInner {
            sequence: 0,
            sender: broadcast::channel(256).0,
        })))
    }
}

impl EventHub {
    /// Events are transient. Lagging clients receive a gap and must take a new snapshot.
    pub fn publish(&self, topic: impl Into<String>, data: Value) -> u64 {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.sequence += 1;
        let sequence = inner.sequence;
        let _ = inner.sender.send(PublishedEvent {
            sequence,
            topic: topic.into(),
            data,
        });
        sequence
    }

    fn subscribe(&self) -> broadcast::Receiver<PublishedEvent> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sender
            .subscribe()
    }
}

#[derive(Clone)]
struct AppState {
    manifest: Manifest,
    sender: mpsc::UnboundedSender<Request>,
    events: EventHub,
    stop: watch::Receiver<bool>,
}

pub struct ControlPlane {
    manifest: Manifest,
    events: EventHub,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}

impl ControlPlane {
    pub async fn start(
        data_dir: PathBuf,
        instance_name: String,
        sender: mpsc::UnboundedSender<Request>,
    ) -> Result<Self> {
        let (registration, listener) = Registration::bind(&data_dir, instance_name).await?;
        let manifest = registration.manifest.clone();
        let events = EventHub::default();
        let (stop, receiver) = watch::channel(false);
        let state = AppState {
            manifest: manifest.clone(),
            sender,
            events: events.clone(),
            stop: receiver.clone(),
        };
        let app = Router::new()
            .route("/healthz", get(health))
            .route("/api/v1/{domain}/{verb}", get(get_call).post(post_call))
            .with_state(state);
        let mut graceful = receiver.clone();
        let mut deadline = receiver;
        let task = tokio::spawn(async move {
            let _registration = registration;
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = graceful.wait_for(|stop| *stop).await;
            });
            tokio::select! {
                result = server => { result?; Ok(()) },
                _ = async {
                    let _ = deadline.wait_for(|stop| *stop).await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } => anyhow::bail!("control plane shutdown timed out"),
            }
        });
        Ok(Self {
            manifest,
            events,
            stop,
            task: Some(task),
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn socket_path(&self) -> &std::path::Path {
        &self.manifest.socket.path
    }
    pub fn events(&self) -> EventHub {
        self.events.clone()
    }

    /// Waits for cleanup. Drop requests the same shutdown without waiting.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for ControlPlane {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn health(State(state): State<AppState>) -> Json<Manifest> {
    Json(state.manifest)
}

fn error(status: StatusCode, code: &str, message: impl ToString) -> Response {
    (
        status,
        Json(json!({"ok": false, "error": {"code": code, "message": message.to_string()}})),
    )
        .into_response()
}

async fn get_call(
    State(state): State<AppState>,
    Path((domain, verb)): Path<(String, String)>,
    headers: HeaderMap,
    query: std::result::Result<
        Query<std::collections::HashMap<String, String>>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Response {
    if !matches!(
        verb.as_str(),
        "get"
            | "list"
            | "views"
            | "read"
            | "status"
            | "should-stop"
            | "subscribe"
            | "watch"
            | "inspect"
    ) {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "read_only_get",
            "use POST for this verb",
        );
    }
    let query = match query {
        Ok(Query(q)) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, "invalid_params", e),
    };
    let params = if let Some(raw) = query.get("params") {
        if query.len() != 1 {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "params cannot be mixed with other query keys",
            );
        }
        match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_REQUEST, "invalid_params", e),
        }
    } else {
        Value::Object(
            query
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    };
    dispatch(state, domain, verb, headers, params).await
}

async fn post_call(
    State(state): State<AppState>,
    Path((domain, verb)): Path<(String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<Value>, JsonRejection>,
) -> Response {
    match body {
        Ok(Json(params)) => dispatch(state, domain, verb, headers, params).await,
        Err(e) => error(e.status(), "invalid_params", e.body_text()),
    }
}

async fn dispatch(
    mut state: AppState,
    domain: String,
    verb: String,
    headers: HeaderMap,
    params: Value,
) -> Response {
    if !crate::identity::valid_name(&domain)
        || !crate::identity::valid_name(&verb)
        || !params.is_object()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "domain/verb must be identifiers and params must be a JSON object",
        );
    }
    if let Some(expected) = headers.get(INSTANCE_HEADER)
        && expected.to_str().ok() != Some(state.manifest.instance_id.to_string().as_str())
    {
        return error(
            StatusCode::CONFLICT,
            "instance_changed",
            "socket now belongs to a different instance",
        );
    }
    if *state.stop.borrow() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "control plane is stopping",
        );
    }
    let streaming = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|v| v.trim() == "text/event-stream"));
    if streaming && !matches!(verb.as_str(), "watch" | "subscribe") {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_subscription",
            "only watch/subscribe methods may stream",
        );
    }
    if streaming && headers.contains_key("last-event-id") {
        return error(
            StatusCode::BAD_REQUEST,
            "replay_unsupported",
            "subscribe again and read a fresh snapshot",
        );
    }
    // Attach before forwarding so publications during UI subscription setup aren't lost.
    let mut receiver = streaming.then(|| state.events.subscribe());
    let (reply, result) = oneshot::channel();
    if state
        .sender
        .send(Request {
            method: format!("{domain}.{verb}"),
            params,
            reply,
        })
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge_closed",
            "UI request receiver is closed",
        );
    }
    let value = tokio::select! {
        _ = state.stop.wait_for(|stop| *stop) => return error(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", "control plane is stopping"),
        result = tokio::time::timeout(Duration::from_secs(30), result) => match result {
            Err(_) => return error(StatusCode::GATEWAY_TIMEOUT, "bridge_timeout", "UI reply timed out; the operation may still have run"),
            Ok(Err(_)) => return error(StatusCode::SERVICE_UNAVAILABLE, "reply_dropped", "UI dropped the reply"),
            Ok(Ok(Err(message))) => return error(StatusCode::UNPROCESSABLE_ENTITY, "domain_error", message),
            Ok(Ok(Ok(value))) => value,
        },
    };
    let Some(mut receiver) = receiver.take() else {
        return Json(json!({"ok": true, "result": value})).into_response();
    };
    let subscription: Subscription =
        match serde_json::from_value(value.clone()).and_then(|s: Subscription| {
            if s.topics.is_empty() || s.topics.iter().any(|t| t.is_empty()) {
                use serde::de::Error;
                return Err(serde_json::Error::custom(
                    "subscription requires nonempty topics",
                ));
            }
            Ok(s)
        }) {
            Ok(s) => s,
            Err(e) => return error(StatusCode::BAD_GATEWAY, "invalid_subscription", e),
        };
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Event::default().event("ready").data(value.to_string()));
        loop {
            tokio::select! {
                _ = async { let _ = state.stop.wait_for(|stop| *stop).await; } => break,
                event = receiver.recv() => match event {
                    Ok(event) if subscription.topics.contains(&event.topic) => {
                        yield Ok(Event::default().event("message").id(event.sequence.to_string())
                            .data(serde_json::to_string(&event).expect("JSON event")));
                    },
                    Ok(_) => {},
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        yield Ok(Event::default().event("gap").data(json!({"missed": missed}).to_string()));
                        break;
                    },
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

pub(crate) fn validate_method(method: &str) -> Result<(&str, &str)> {
    let (domain, verb) = method
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("method must be domain.verb"))?;
    ensure!(
        crate::identity::valid_name(domain) && crate::identity::valid_name(verb),
        "invalid method"
    );
    Ok((domain, verb))
}
