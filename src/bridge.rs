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
use crate::config::Config;
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
    /// Config at startup, used when config.json cannot be read later.
    startup: Config,
    /// Client for forwarded routes; no overall timeout, as streams can be long.
    forward: reqwest::Client,
    /// `!command` key output by command, so each command runs once per bridge.
    keys: KeyCache,
}

type KeyCache = std::sync::Mutex<std::collections::HashMap<String, String>>;

/// A provider's API key; only `!command` output is cached, so saved and literal keys
/// follow config.json and auth.json as they change.
fn cached_key(
    keys: &KeyCache,
    config: &Config,
    provider: &crate::providers::Provider,
) -> Result<Option<String>> {
    let reference = config
        .providers
        .get(&provider.id)
        .map(|c| c.api_key.as_str())
        .unwrap_or("");
    if !reference.starts_with('!') {
        return crate::providers::api_key(config, provider);
    }
    if let Some(key) = keys.lock().unwrap().get(reference) {
        return Ok(Some(key.clone()));
    }
    let key = crate::providers::resolve_key(reference)?;
    keys.lock()
        .unwrap()
        .insert(reference.to_owned(), key.clone());
    Ok(Some(key))
}

pub async fn serve(port: u16) -> Result<()> {
    let dir = crate::config::state_dir()?;
    let key = load_or_create_key(&dir)?;
    crate::store::private_dir(&crate::store::logs_dir()?)?;
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
    let openai = crate::providers::find(&config, "openai")
        .map(|p| p.base_url)
        .unwrap_or_else(|| config.upstream_base_url.clone());
    let upstream = Upstream::new(http.clone(), &openai, token, transport);
    let bridge = Arc::new(Bridge {
        key,
        active: Arc::new(tokio::sync::Semaphore::new(ACTIVE_REQUEST_LIMIT)),
        upstream: upstream.clone(),
        models: RwLock::new(crate::catalog::cached().unwrap_or_default()),
        log: Some(crate::store::log_path()?),
        shutdown: Arc::new(tokio::sync::Notify::new()),
        startup: config,
        forward: http,
        keys: Default::default(),
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
        .fallback(passthrough)
        .with_state(state)
}

fn same(token: &str, key: &str) -> bool {
    // Always inspect every byte of a correctly sized token.
    token.len() == key.len()
        && token
            .bytes()
            .zip(key.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// The bridge key arrives in `x-byom-key` (when Claude Code keeps its own credential in
/// `Authorization`) or as the bearer token or API key.
fn authenticated(headers: &HeaderMap, key: &str) -> bool {
    let value = |name: &str| headers.get(name).and_then(|h| h.to_str().ok());
    let bearer = value("authorization").and_then(|v| v.strip_prefix("Bearer "));
    [
        value(crate::relay::BRIDGE_KEY_HEADER),
        bearer,
        value("x-api-key"),
    ]
    .into_iter()
    .flatten()
    .any(|token| same(token, key))
}

fn error(status: u16, kind: &str, message: &str) -> Response {
    // Plan limits and rejected requests do not clear on retry.
    let retry = !(kind == "rate_limit_error" && message.contains("plan limit"))
        && !message.starts_with(crate::providers::NOT_RUNNING)
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
        .map(|m| json!({"type": "model", "id": crate::providers::canonical(&m.slug), "display_name": m.display_name, "created_at": "2026-01-01T00:00:00Z"}))
        .collect();
    axum::Json(json!({
        "data": data, "has_more": false,
        "first_id": data.first().map(|m| m["id"].clone()),
        "last_id": data.last().map(|m| m["id"].clone()),
    }))
    .into_response()
}

#[derive(Clone)]
struct Incoming {
    method: axum::http::Method,
    path: String,
    headers: HeaderMap,
    bytes: Bytes,
}

async fn read_body(request: Request) -> Result<Incoming, Box<Response>> {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, BODY_LIMIT).await.map_err(|_| {
        Box::new(error(
            413,
            "request_too_large",
            "Request body exceeds the bridge limit",
        ))
    })?;
    Ok(Incoming {
        method: parts.method,
        path: parts
            .uri
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| parts.uri.path().to_owned()),
        headers: parts.headers,
        bytes,
    })
}

fn parse_json(bytes: &[u8]) -> Result<Value, Box<Response>> {
    match serde_json::from_slice(bytes) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(Box::new(error(
            400,
            "invalid_request_error",
            "Request body must be a JSON object",
        ))),
        Err(_) => Err(Box::new(error(
            400,
            "invalid_request_error",
            "Invalid JSON request",
        ))),
    }
}

/// Whether the request carries a credential other than the bridge key (Claude Code's own).
fn has_claude_credential(headers: &HeaderMap, key: &str) -> bool {
    let value = |name: &str| headers.get(name).and_then(|h| h.to_str().ok());
    [
        value("authorization").map(|v| v.trim_start_matches("Bearer ")),
        value("x-api-key"),
    ]
    .into_iter()
    .flatten()
    .any(|token| !same(token, key))
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
    let incoming = match read_body(request).await {
        Ok(incoming) => incoming,
        Err(response) => return *response,
    };
    let body = match parse_json(&incoming.bytes) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let config = state.config();
    let model = body["model"].as_str().unwrap_or("").to_owned();
    if let Some((provider, name)) = crate::providers::route(&config, &model)
        && provider.protocol == crate::providers::Protocol::Anthropic
        && provider.id == crate::providers::CLAUDE_PROVIDER
    {
        return state
            .forward(&config, &provider, Some(&name), incoming, &model)
            .await;
    }
    axum::Json(json!({"input_tokens": estimate_tokens(&body)})).into_response()
}

struct Log {
    started: Instant,
    first_token_ms: Option<u128>,
    session: String,
    model: String,
}

impl Bridge {
    fn config(&self) -> Config {
        crate::config::load().unwrap_or_else(|_| self.startup.clone())
    }

    fn api_key(
        &self,
        config: &Config,
        provider: &crate::providers::Provider,
    ) -> Result<Option<String>> {
        cached_key(&self.keys, config, provider)
    }

    /// Forward an Anthropic-protocol request and stream the answer back unchanged.
    async fn forward(
        &self,
        config: &Config,
        provider: &crate::providers::Provider,
        upstream_model: Option<&str>,
        incoming: Incoming,
        model: &str,
    ) -> Response {
        let log = Log {
            started: Instant::now(),
            first_token_ms: None,
            session: incoming
                .headers
                .get("x-claude-code-session-id")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned(),
            model: model.to_owned(),
        };
        let route = crate::upstream::Route {
            transport: if provider.id == crate::providers::CLAUDE_PROVIDER {
                "relay"
            } else {
                "anthropic"
            },
            ..Default::default()
        };
        let api_key = match self.api_key(config, provider) {
            Ok(key) => key,
            Err(e) => {
                let message = format!("API key for {}: {e:#}", provider.id);
                self.write_log(&log, Some(&route), &message, &Value::Null);
                return error(401, "authentication_error", &message);
            }
        };
        let outbound = crate::relay::Outbound {
            method: incoming.method.clone(),
            path: &incoming.path,
            headers: &incoming.headers,
            body: incoming.bytes.clone(),
        };
        let prepared = crate::relay::prepare(
            provider,
            upstream_model,
            api_key.as_deref(),
            &self.key,
            &outbound,
        );
        let result = match prepared {
            Ok((url, headers, body)) => {
                crate::relay::send(&self.forward, incoming.method, &url, headers, body).await
            }
            Err(refusal) => Err(refusal),
        };
        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                let mut log = log;
                log.first_token_ms = Some(log.started.elapsed().as_millis());
                let outcome = if status < 400 {
                    "ok".to_owned()
                } else {
                    format!("HTTP {status}")
                };
                let streaming = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("text/event-stream"));
                if status >= 400 || !streaming {
                    self.write_log(&log, Some(&route), &outcome, &Value::Null);
                    return response;
                }
                // Read usage from the stream as it passes; log once it ends.
                let (parts, body) = response.into_parts();
                let scanner = UsageScanner::default();
                let path = self.log.clone();
                let transport = route.transport;
                let stream = stream::unfold(
                    Some((body.into_data_stream(), scanner, log, path)),
                    move |slot| async move {
                        use futures_util::StreamExt;
                        let (mut body, mut scanner, log, path) = slot?;
                        match body.next().await {
                            Some(Ok(bytes)) => {
                                scanner.push(&bytes);
                                Some((
                                    Ok::<_, axum::Error>(bytes),
                                    Some((body, scanner, log, path)),
                                ))
                            }
                            Some(Err(e)) => {
                                append_log(
                                    path.as_deref(),
                                    &log,
                                    transport,
                                    "stream broke off",
                                    &scanner.usage(),
                                );
                                Some((Err(e), None))
                            }
                            None => {
                                append_log(
                                    path.as_deref(),
                                    &log,
                                    transport,
                                    "ok",
                                    &scanner.usage(),
                                );
                                None
                            }
                        }
                    },
                );
                Response::from_parts(parts, Body::from_stream(stream))
            }
            Err(refusal) => {
                self.write_log(&log, Some(&route), &refusal.message, &Value::Null);
                error(refusal.status, refusal.kind, &refusal.message)
            }
        }
    }

    /// Serve a request from an OpenAI Chat Completions provider.
    #[allow(clippy::too_many_arguments)]
    async fn chat(
        &self,
        config: &Config,
        provider: &crate::providers::Provider,
        upstream_model: &str,
        requested: &str,
        input: Value,
        headers: &HeaderMap,
        permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Response {
        let mut log = Log {
            started: Instant::now(),
            first_token_ms: None,
            session: headers
                .get("x-claude-code-session-id")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned(),
            model: requested.to_owned(),
        };
        let route = crate::upstream::Route {
            transport: "openai-chat",
            ..Default::default()
        };
        let key = match self.api_key(config, provider) {
            Ok(key) => key,
            Err(e) => {
                return error(
                    401,
                    "authentication_error",
                    &format!("API key for {}: {e:#}", provider.id),
                );
            }
        };
        if provider.auth == crate::providers::Auth::ApiKey && key.is_none() {
            let message = format!(
                "No API key for {}. Run: byom login {}",
                provider.name, provider.id
            );
            self.write_log(&log, Some(&route), &message, &Value::Null);
            return error(401, "authentication_error", &message);
        }
        let streaming = input["stream"].as_bool() == Some(true);
        let thinking = matches!(
            input["thinking"]["type"].as_str(),
            Some("enabled" | "adaptive")
        );
        // Mistral rejects unknown parameters such as stream_options.
        let body = crate::chat::translate_request(&input, upstream_model, provider.id != "mistral");
        let url = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let mut request = self
            .forward
            .post(&url)
            .header("accept", "text/event-stream")
            .json(&body);
        if let Some(key) = &key {
            request = request.bearer_auth(key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) if e.is_connect() && !crate::providers::reachable(provider) => {
                let message = format!(
                    "{} {} for {}. {}",
                    crate::providers::NOT_RUNNING,
                    provider.base_url,
                    provider.name,
                    crate::providers::NOT_RUNNING_HINT
                );
                self.write_log(&log, Some(&route), &message, &Value::Null);
                return error(503, "api_error", &message);
            }
            Err(e) => {
                let message = format!(
                    "Could not reach {}: {}",
                    provider.name,
                    if e.is_connect() {
                        "connection failed"
                    } else {
                        "request failed"
                    }
                );
                self.write_log(&log, Some(&route), &message, &Value::Null);
                return error(502, "api_error", &message);
            }
        };
        let status = response.status().as_u16();
        if status >= 400 {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let detail = if body["error"].is_object() {
                &body["error"]
            } else {
                &body
            };
            let e = ProviderError::from_openai(
                detail["code"]
                    .as_str()
                    .or(detail["type"].as_str())
                    .unwrap_or("upstream_error"),
                detail["message"].as_str().unwrap_or(""),
                Some(status),
            );
            self.write_log(&log, Some(&route), &e.message, &Value::Null);
            return error(e.status, e.kind, &e.message);
        }
        let mut source = ChatSource {
            body: Box::pin(response.bytes_stream()),
            decoder: crate::sse::SseDecoder::default(),
            queue: VecDeque::new(),
            ended: false,
            done: false,
        };
        let mut translator = crate::chat::ChatTranslator::new(requested, thinking);
        let mut frames = VecDeque::new();
        while !translator.has_content() && !translator.finished() {
            match source.pull(&mut translator).await {
                Ok(out) => frames.extend(out),
                Err(e) => {
                    self.write_log(&log, Some(&route), &e.message, &Value::Null);
                    return error(e.status, e.kind, &e.message);
                }
            }
        }
        log.first_token_ms = Some(log.started.elapsed().as_millis());
        if !streaming {
            while !translator.finished() {
                if let Err(e) = source.pull(&mut translator).await {
                    self.write_log(&log, Some(&route), &e.message, &Value::Null);
                    return error(e.status, e.kind, &e.message);
                }
            }
            self.write_log(&log, Some(&route), "ok", &translator.usage);
            return axum::Json(translator.message()).into_response();
        }
        let log_path = self.log.clone();
        let output = stream::unfold(
            Some((source, translator, frames, log, log_path, permit)),
            |slot| async move {
                let (mut source, mut translator, mut frames, log, log_path, permit) = slot?;
                loop {
                    if let Some(event) = frames.pop_front() {
                        return Some((
                            Ok::<_, Infallible>(Bytes::from(event.sse())),
                            Some((source, translator, frames, log, log_path, permit)),
                        ));
                    }
                    if translator.finished() {
                        append_log(
                            log_path.as_deref(),
                            &log,
                            "openai-chat",
                            "ok",
                            &translator.usage,
                        );
                        return None;
                    }
                    match source.pull(&mut translator).await {
                        Ok(out) => frames.extend(out),
                        Err(e) => {
                            append_log(
                                log_path.as_deref(),
                                &log,
                                "openai-chat",
                                &e.message,
                                &Value::Null,
                            );
                            return Some((
                                Ok(Bytes::from(
                                    Event {
                                        name: "error",
                                        data: e.event(),
                                    }
                                    .sse(),
                                )),
                                None,
                            ));
                        }
                    }
                }
            },
        );
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(output))
            .expect("static response headers")
    }

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
            "outcome": short(outcome),
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
    let incoming = match read_body(request).await {
        Ok(incoming) => incoming,
        Err(response) => return *response,
    };
    let input = match parse_json(&incoming.bytes) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    let config = state.config();
    let first = crate::providers::canonical(input["model"].as_str().unwrap_or(""));
    // The requested model, then its configured fallbacks; a fallback is tried only when the
    // previous model failed before producing any output.
    let mut chain = vec![first.clone()];
    chain.extend(
        config
            .fallbacks
            .get(&first)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|m| crate::providers::canonical(m)),
    );
    let permit = Arc::new(permit);
    let mut response = None;
    for (attempt, model) in chain.iter().enumerate() {
        let mut input = input.clone();
        if attempt > 0 {
            input["model"] = Value::String(model.clone());
        }
        let result = dispatch(
            state.clone(),
            &config,
            model,
            incoming.clone(),
            input,
            permit.clone(),
        )
        .await;
        let failed = result.status().as_u16() >= 400;
        response = Some(result);
        if !failed {
            break;
        }
    }
    response.unwrap_or_else(|| error(404, "not_found_error", "No model requested"))
}

/// Serve one model: route it to its provider and translate as needed.
async fn dispatch(
    state: Arc<Bridge>,
    config: &Config,
    requested: &str,
    incoming: Incoming,
    mut input: Value,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Response {
    let config = config.clone();
    let requested = requested.to_owned();
    let Some((provider, upstream_model)) = crate::providers::route(&config, &requested) else {
        return error(
            404,
            "not_found_error",
            &format!("Unknown model {requested:?}. List models with: byom models"),
        );
    };
    match (provider.protocol, provider.auth) {
        (crate::providers::Protocol::Anthropic, _) => {
            return state
                .forward(
                    &config,
                    &provider,
                    Some(&upstream_model),
                    incoming,
                    &requested,
                )
                .await;
        }
        (crate::providers::Protocol::OpenAiResponses, crate::providers::Auth::ChatGpt) => {
            input["model"] = Value::String(upstream_model);
        }
        (crate::providers::Protocol::OpenAiChat, _) => {
            return state
                .chat(
                    &config,
                    &provider,
                    &upstream_model,
                    &requested,
                    input,
                    &incoming.headers,
                    permit,
                )
                .await;
        }
        (protocol, _) => {
            return error(
                501,
                "api_error",
                &format!(
                    "{} uses the {} protocol, which this byom does not support yet",
                    provider.id,
                    protocol.as_str()
                ),
            );
        }
    }
    let headers = incoming.headers;
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
    let again = turn.clone();
    let mut events = match state.upstream.start(turn).await {
        Ok(events) => events,
        Err(e) => {
            state.write_log(&log, None, &e.message, &Value::Null);
            return error(e.status, e.kind, &e.message);
        }
    };
    // Hold headers until content arrives so early failures keep their HTTP status.
    let mut frames = VecDeque::new();
    let mut reconnected = false;
    while !translator.has_content() && !translator.finished() {
        match pull(&mut events, &mut translator).await {
            Ok(out) => frames.extend(out),
            // Nothing has reached Claude Code yet, so a dropped stream starts over once.
            Err(e) if !reconnected && e.message == crate::upstream::STREAM_LOST => {
                reconnected = true;
                events = match state.upstream.start(again.clone()).await {
                    Ok(events) => events,
                    Err(e) => {
                        state.write_log(&log, None, &e.message, &Value::Null);
                        return error(e.status, e.kind, &e.message);
                    }
                };
                events.route.retried = true;
                translator = Translator::new(&translated.model, translated.thinking);
                frames.clear();
            }
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
            crate::upstream::STREAM_LOST,
        )),
    }
}

/// Other Anthropic API paths Claude Code calls while signed in go to Anthropic unchanged.
async fn passthrough(State(state): State<Arc<Bridge>>, request: Request) -> Response {
    if !authenticated(request.headers(), &state.key) {
        return unauthorized();
    }
    if !has_claude_credential(request.headers(), &state.key) {
        return error(404, "not_found_error", "Not found");
    }
    let incoming = match read_body(request).await {
        Ok(incoming) => incoming,
        Err(response) => return *response,
    };
    let config = state.config();
    let Some(provider) = crate::providers::find(&config, crate::providers::CLAUDE_PROVIDER) else {
        return error(404, "not_found_error", "Not found");
    };
    let label = incoming.path.split('?').next().unwrap_or("").to_owned();
    state
        .forward(&config, &provider, None, incoming, &label)
        .await
}

type ByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// A Chat Completions SSE response, decoded into chunks on demand.
struct ChatSource {
    body: ByteStream,
    decoder: crate::sse::SseDecoder,
    queue: VecDeque<Value>,
    ended: bool,
    done: bool,
}

impl ChatSource {
    async fn pull(
        &mut self,
        translator: &mut crate::chat::ChatTranslator,
    ) -> Result<Vec<Event>, ProviderError> {
        use futures_util::StreamExt;
        loop {
            if let Some(chunk) = self.queue.pop_front() {
                return translator.handle(&chunk);
            }
            if self.ended {
                return translator.finish(self.done);
            }
            let next = tokio::time::timeout(crate::upstream::EVENT_TIMEOUT, self.body.next()).await;
            let frames = match next {
                Ok(Some(Ok(bytes))) => self.decoder.push(&bytes),
                Ok(Some(Err(_))) => {
                    return Err(ProviderError::new(
                        502,
                        "api_error",
                        "The provider's stream broke off",
                    ));
                }
                Ok(None) => {
                    self.ended = true;
                    self.decoder.finish()
                }
                Err(_) => {
                    return Err(ProviderError::new(
                        504,
                        "api_error",
                        "The provider's stream stalled",
                    ));
                }
            };
            let frames = frames.map_err(|_| {
                ProviderError::new(502, "api_error", "The provider sent a malformed stream")
            })?;
            for frame in frames {
                if frame.data.trim() == "[DONE]" {
                    self.ended = true;
                    self.done = true;
                    break;
                }
                match serde_json::from_str(&frame.data) {
                    Ok(chunk) => self.queue.push_back(chunk),
                    Err(_) => {
                        return Err(ProviderError::new(
                            502,
                            "api_error",
                            "The provider sent an invalid chunk",
                        ));
                    }
                }
            }
        }
    }
}

/// Collects token usage from an Anthropic SSE stream without keeping its content.
#[derive(Default)]
struct UsageScanner {
    decoder: crate::sse::SseDecoder,
    input: u64,
    cached: u64,
    written: u64,
    output: u64,
    seen: bool,
}

impl UsageScanner {
    fn push(&mut self, bytes: &[u8]) {
        let Ok(events) = self.decoder.push(bytes) else {
            return;
        };
        for event in events {
            if !event.data.contains("usage") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let usage = match value["type"].as_str() {
                Some("message_start") => &value["message"]["usage"],
                Some("message_delta") => &value["usage"],
                _ => continue,
            };
            self.seen = true;
            if let Some(n) = usage["input_tokens"].as_u64() {
                self.input = self.input.max(n);
            }
            if let Some(n) = usage["cache_read_input_tokens"].as_u64() {
                self.cached = self.cached.max(n);
            }
            if let Some(n) = usage["cache_creation_input_tokens"].as_u64() {
                self.written = self.written.max(n);
            }
            if let Some(n) = usage["output_tokens"].as_u64() {
                self.output = self.output.max(n);
            }
        }
    }

    fn usage(&self) -> Value {
        if !self.seen {
            return Value::Null;
        }
        json!({
            "input_tokens": self.input,
            "cache_read_input_tokens": self.cached,
            "cache_creation_input_tokens": self.written,
            "output_tokens": self.output,
        })
    }
}

/// Error text kept in the log; providers sometimes echo request content in long messages.
fn short(outcome: &str) -> String {
    outcome.chars().take(160).collect()
}

/// Append one request line to the bridge log (no content).
fn append_log(path: Option<&Path>, log: &Log, transport: &str, outcome: &str, usage: &Value) {
    let Some(path) = path else {
        return;
    };
    let line = json!({
        "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "session": log.session.chars().take(8).collect::<String>(),
        "model": log.model,
        "transport": transport,
        "first_token_ms": log.first_token_ms,
        "total_ms": log.started.elapsed().as_millis(),
        "outcome": short(outcome),
        "usage": usage,
    });
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
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
    fn usage_scanner_reads_anthropic_stream_usage() {
        let mut scanner = UsageScanner::default();
        assert_eq!(scanner.usage(), Value::Null);
        let stream = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"usage\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n";
        let (a, b) = stream.split_at(70);
        scanner.push(a.as_bytes());
        scanner.push(b.as_bytes());
        assert_eq!(
            scanner.usage(),
            json!({"input_tokens": 12, "cache_read_input_tokens": 30, "cache_creation_input_tokens": 0, "output_tokens": 42})
        );
    }

    #[test]
    fn json_bodies_must_be_objects() {
        assert!(parse_json(b"{}").is_ok());
        for body in [&b"[1]"[..], b"null", b"\"x\"", b"{"] {
            assert_eq!(parse_json(body).err().unwrap().status(), 400);
        }
    }

    #[test]
    fn only_command_keys_are_cached() {
        let provider = crate::providers::find(&Config::default(), "groq").unwrap();
        let keys = KeyCache::default();
        let with_key = |reference: &str| {
            let mut config = Config::default();
            config
                .providers
                .entry(provider.id.clone())
                .or_default()
                .api_key = reference.into();
            config
        };
        let literal = |r| cached_key(&keys, &with_key(r), &provider).unwrap();
        assert_eq!(literal("sk-1").as_deref(), Some("sk-1"));
        assert_eq!(literal("sk-2").as_deref(), Some("sk-2"));
        assert!(keys.lock().unwrap().is_empty());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        fs::write(&file, "sk-a").unwrap();
        let command = format!("!cat '{}'", file.display());
        assert_eq!(literal(&command).as_deref(), Some("sk-a"));
        fs::write(&file, "sk-b").unwrap();
        assert_eq!(literal(&command).as_deref(), Some("sk-a"));
        assert_eq!(literal(&format!("{command} ")).as_deref(), Some("sk-b"));
    }

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
