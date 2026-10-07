//! Authenticated loopback bridge serving the Anthropic Messages API from ChatGPT-plan models.
use std::{
    collections::VecDeque,
    convert::Infallible,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::stream;
use serde_json::{Value, json};

use crate::catalog::Model;
use crate::response::{Event, ProviderError, Translator};
use crate::upstream::{Events, Transport, Turn, Upstream};

const BODY_LIMIT: usize = 64 * 1024 * 1024;
const ACTIVE_REQUEST_LIMIT: usize = 32;

pub struct Bridge {
    key: String,
    active: Arc<tokio::sync::Semaphore>,
    upstream: Arc<Upstream>,
    models: RwLock<Vec<Model>>,
    log: Option<PathBuf>,
    shutdown: Arc<tokio::sync::Notify>,
}

pub async fn serve(port: u16) -> Result<()> {
    let dir = crate::config::state_dir()?;
    let key = load_or_create_key(&dir)?;
    let config = crate::config::load()?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?;
    let token: crate::upstream::TokenSource =
        Arc::new(|| Box::pin(async { crate::auth::access_token().await }));
    let transport = if config.transport == "http" {
        Transport::Http
    } else {
        Transport::Auto
    };
    let upstream = Upstream::new(http, &config.upstream_base_url, token, transport);
    let bridge = Arc::new(Bridge {
        key,
        active: Arc::new(tokio::sync::Semaphore::new(ACTIVE_REQUEST_LIMIT)),
        upstream: upstream.clone(),
        models: RwLock::new(crate::catalog::cached().unwrap_or_default()),
        log: Some(dir.join("bridge.log")),
        shutdown: Arc::new(tokio::sync::Notify::new()),
    });
    upstream.warm();
    let refresh = bridge.clone();
    tokio::spawn(async move {
        if let Ok(models) = crate::catalog::fetch().await {
            *refresh.models.write().unwrap() = models;
        }
    });
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let shutdown = bridge.shutdown.clone();
    axum::serve(listener, router(bridge))
        .with_graceful_shutdown(async move { shutdown.notified().await })
        .await?;
    Ok(())
}

pub fn router(state: Arc<Bridge>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/shutdown", post(shutdown))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(models))
        .with_state(state)
}

fn authenticated(headers: &HeaderMap, key: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let api_key = headers.get("x-api-key").and_then(|h| h.to_str().ok());
    let Some(token) = bearer.or(api_key) else {
        return false;
    };
    // Always inspect every byte of a correctly sized token.
    token.len() == key.len()
        && token
            .bytes()
            .zip(key.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn error(status: u16, kind: &str, message: &str) -> Response {
    // Plan limits and rejected requests do not clear on retry.
    let retry = !(kind == "rate_limit_error" && message.contains("plan limit"))
        && matches!(status, 408 | 409 | 429 | 500..);
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        [("x-should-retry", if retry { "true" } else { "false" })],
        axum::Json(json!({"type":"error","error":{"type":kind,"message":message}})),
    )
        .into_response()
}

fn unauthorized() -> Response {
    error(401, "authentication_error", "Invalid bridge credentials")
}

async fn health(State(state): State<Arc<Bridge>>, headers: HeaderMap) -> Response {
    if !authenticated(&headers, &state.key) {
        return unauthorized();
    }
    axum::Json(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})).into_response()
}

/// Lets a newer launcher replace this bridge; requires the bridge key.
async fn shutdown(State(state): State<Arc<Bridge>>, headers: HeaderMap) -> Response {
    if !authenticated(&headers, &state.key) {
        return unauthorized();
    }
    state.shutdown.notify_one();
    axum::Json(json!({"status": "stopping"})).into_response()
}

async fn models(State(state): State<Arc<Bridge>>, headers: HeaderMap) -> Response {
    if !authenticated(&headers, &state.key) {
        return unauthorized();
    }
    let models = state.models.read().unwrap().clone();
    let data: Vec<Value> = models
        .iter()
        .filter(|m| m.listed)
        .map(|m| json!({"type": "model", "id": m.slug, "display_name": m.display_name, "created_at": "2026-01-01T00:00:00Z"}))
        .collect();
    axum::Json(json!({
        "data": data, "has_more": false,
        "first_id": data.first().map(|m| m["id"].clone()),
        "last_id": data.last().map(|m| m["id"].clone()),
    }))
    .into_response()
}

async fn read_json(request: Request) -> Result<(HeaderMap, Value), Box<Response>> {
    let headers = request.headers().clone();
    let bytes = to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| {
            Box::new(error(
                413,
                "request_too_large",
                "Request body exceeds the bridge limit",
            ))
        })?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(error(400, "invalid_request_error", "Invalid JSON request")))?;
    Ok((headers, value))
}

/// Local estimate; the plan route has no token-counting endpoint.
pub fn estimate_tokens(request: &Value) -> u64 {
    fn walk(value: &Value, total: &mut u64) {
        match value {
            Value::String(s) => *total += s.len() as u64 / 4 + 1,
            Value::Array(items) => items.iter().for_each(|v| walk(v, total)),
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("image") {
                    *total += 1600;
                    return;
                }
                map.values().for_each(|v| walk(v, total));
            }
            _ => *total += 1,
        }
    }
    let mut total = 0;
    for field in ["system", "messages", "tools"] {
        walk(&request[field], &mut total);
    }
    total
}

async fn count_tokens(State(state): State<Arc<Bridge>>, request: Request) -> Response {
    if !authenticated(request.headers(), &state.key) {
        return unauthorized();
    }
    match read_json(request).await {
        Ok((_, body)) => {
            axum::Json(json!({"input_tokens": estimate_tokens(&body)})).into_response()
        }
        Err(response) => *response,
    }
}

struct Log {
    started: Instant,
    first_token_ms: Option<u128>,
    session: String,
    model: String,
}

impl Bridge {
    fn write_log(
        &self,
        log: &Log,
        route: Option<&crate::upstream::Route>,
        outcome: &str,
        usage: &Value,
    ) {
        let Some(path) = &self.log else {
            return;
        };
        let line = json!({
            "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "session": log.session.chars().take(8).collect::<String>(),
            "model": log.model,
            "transport": route.map(|r| r.transport),
            "reused": route.map(|r| r.reused),
            "continued": route.map(|r| r.continued),
            "sent_items": route.map(|r| r.sent_items),
            "retried": route.map(|r| r.retried),
            "miss": route.and_then(|r| r.miss.clone()),
            "first_token_ms": log.first_token_ms,
            "total_ms": log.started.elapsed().as_millis(),
            "outcome": outcome,
            "usage": usage,
        });
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{line}");
        }
    }
}

async fn messages(State(state): State<Arc<Bridge>>, request: Request) -> Response {
    if !authenticated(request.headers(), &state.key) {
        return unauthorized();
    }
    let Ok(permit) = state.active.clone().try_acquire_owned() else {
        return error(
            429,
            "rate_limit_error",
            "Bridge active request limit reached",
        );
    };
    let (headers, input) = match read_json(request).await {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let streaming = input["stream"].as_bool() == Some(true);
    let session = headers
        .get("x-claude-code-session-id")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .or_else(|| crate::request::metadata_session(&input));
    let model = input["model"].as_str().unwrap_or("").to_owned();
    let effort_levels = crate::catalog::find(&state.models.read().unwrap(), &model)
        .map(|m| m.effort_levels.clone())
        .unwrap_or_default();
    let options = crate::request::Options {
        session: session.as_deref(),
        effort_levels: &effort_levels,
    };
    let translated = match crate::request::translate(&input, &options) {
        Ok(t) => t,
        Err(e) => return error(400, "invalid_request_error", &e.to_string()),
    };
    let mut log = Log {
        started: Instant::now(),
        first_token_ms: None,
        session: session.clone().unwrap_or_default(),
        model: translated.model.clone(),
    };
    let tools = translated
        .input
        .iter()
        .take_while(|i| i["type"] == "additional_tools")
        .count();
    let turn = Turn {
        session,
        body: translated.body,
        prefix: translated.input[..tools].to_vec(),
        messages: translated.messages,
        model: translated.model.clone(),
        thinking: translated.thinking,
    };
    let mut translator = Translator::new(&translated.model, translated.thinking);
    let mut events = match state.upstream.start(turn).await {
        Ok(events) => events,
        Err(e) => {
            state.write_log(&log, None, &e.message, &Value::Null);
            return error(e.status, e.kind, &e.message);
        }
    };
    // Hold headers until content arrives so early failures keep their HTTP status.
    let mut frames = VecDeque::new();
    while !translator.has_content() && !translator.finished() {
        match pull(&mut events, &mut translator).await {
            Ok(out) => frames.extend(out),
            Err(e) => {
                state.write_log(&log, Some(&events.route), &e.message, &Value::Null);
                return error(e.status, e.kind, &e.message);
            }
        }
    }
    log.first_token_ms = Some(log.started.elapsed().as_millis());

    if !streaming {
        while !translator.finished() {
            if let Err(e) = pull(&mut events, &mut translator).await {
                state.write_log(&log, Some(&events.route), &e.message, &Value::Null);
                return error(e.status, e.kind, &e.message);
            }
        }
        state.write_log(&log, Some(&events.route), "ok", &translator.usage);
        let message = translator.message();
        events.finish(&translator.content);
        return axum::Json(message).into_response();
    }

    let output = stream::unfold(
        Some((events, translator, frames, state, log, permit)),
        |slot| async move {
            let (mut events, mut translator, mut frames, state, log, permit) = slot?;
            loop {
                if let Some(event) = frames.pop_front() {
                    let bytes = Bytes::from(event.sse());
                    return Some((
                        Ok::<_, Infallible>(bytes),
                        Some((events, translator, frames, state, log, permit)),
                    ));
                }
                if translator.finished() {
                    state.write_log(&log, Some(&events.route), "ok", &translator.usage);
                    events.finish(&translator.content);
                    return None;
                }
                match pull(&mut events, &mut translator).await {
                    Ok(out) => frames.extend(out),
                    Err(e) => {
                        state.write_log(&log, Some(&events.route), &e.message, &Value::Null);
                        let bytes = Bytes::from(
                            Event {
                                name: "error",
                                data: e.event(),
                            }
                            .sse(),
                        );
                        return Some((Ok(bytes), None));
                    }
                }
            }
        },
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .expect("static response headers")
}

async fn pull(
    events: &mut Events,
    translator: &mut Translator,
) -> Result<Vec<Event>, ProviderError> {
    match events.next().await {
        Some(Ok(event)) => translator.handle(&event),
        Some(Err(e)) => Err(e),
        None => Err(ProviderError::new(
            502,
            "api_error",
            "OpenAI stream ended before the response completed",
        )),
    }
}

fn load_or_create_key(dir: &Path) -> Result<String> {
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(dir)
        .context("creating bridge state directory")?;
    let directory = fs::symlink_metadata(dir)?;
    if !directory.is_dir() || directory.file_type().is_symlink() {
        bail!("bridge state directory must be a real directory");
    }
    #[cfg(unix)]
    if directory.permissions().mode() & 0o022 != 0 {
        bail!("bridge state directory must not be writable by other users");
    }
    let path = dir.join("bridge.key");
    if !path.try_exists()? {
        let random: [u8; 32] = rand::random();
        let key: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = dir.join(format!(".bridge-key-{:032x}", rand::random::<u128>()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .context("creating protected bridge key")?;
        let result = (|| -> Result<()> {
            file.write_all(key.as_bytes())?;
            file.sync_all()?;
            match fs::hard_link(&temporary, &path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(error.into()),
            }
        })();
        let cleanup = fs::remove_file(&temporary);
        result?;
        cleanup?;
    }
    let entry = fs::symlink_metadata(&path).context("checking bridge key")?;
    if !entry.is_file() || entry.file_type().is_symlink() {
        bail!("bridge key must be a regular file");
    }
    let file = fs::File::open(&path).context("opening bridge key")?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o600
        || metadata.ino() != entry.ino()
        || metadata.dev() != entry.dev()
    {
        bail!("bridge key must be protected with mode 0600 and must not be replaced while opening");
    }
    let mut key = String::new();
    file.take(65).read_to_string(&mut key)?;
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid bridge key");
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_counts_text_and_images() {
        let request = json!({"system": "x".repeat(400), "messages": [{"role": "user", "content": [{"type": "image", "source": {"data": "A".repeat(100000)}}]}]});
        let estimate = estimate_tokens(&request);
        assert!((1600..1800).contains(&estimate), "{estimate}");
    }

    #[test]
    fn key_is_stable_across_concurrent_startups() {
        let dir = tempfile::tempdir().unwrap();
        let keys = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| load_or_create_key(dir.path()).unwrap()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(keys.iter().all(|key| key == &keys[0]));
    }
    #[cfg(unix)]
    #[test]
    fn rejects_public_and_symlink_keys() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.key");
        load_or_create_key(dir.path()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_create_key(dir.path()).is_err());
        fs::remove_file(&path).unwrap();
        symlink(dir.path().join("missing"), &path).unwrap();
        assert!(load_or_create_key(dir.path()).is_err());
    }
}
