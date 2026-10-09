//! Responses transport: pooled WebSockets with turn continuation, HTTP/SSE fallback.
//!
//! A connection remembers the conversation it last served. When the next request in that
//! session extends it, only the new messages are sent with `previous_response_id`.
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::response::ProviderError;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;
type TokenFuture = Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>;
pub type TokenSource = Arc<dyn Fn() -> TokenFuture + Send + Sync>;

/// Connections idle longer than this are not reused.
const IDLE_LIMIT: Duration = Duration::from_secs(240);
const MAX_AGE: Duration = Duration::from_secs(50 * 60);
const PER_SESSION: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Auto,
    Http,
}

/// The error for a stream that ended before the response completed.
pub const STREAM_LOST: &str = "OpenAI stream ended before the response completed";

#[derive(Clone)]
pub struct Turn {
    pub session: Option<String>,
    /// Responses body without `input`.
    pub body: Map<String, Value>,
    /// Input items that precede the conversation (tool definitions).
    pub prefix: Vec<Value>,
    /// Normalized Anthropic messages for this request.
    pub messages: Vec<Value>,
    pub model: String,
    pub thinking: bool,
}

impl Turn {
    fn key(&self) -> String {
        Value::Object(self.body.clone()).to_string()
    }

    fn tools(&self) -> Vec<Value> {
        self.prefix
            .iter()
            .flat_map(|item| item["tools"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    fn full_input(&self) -> Vec<Value> {
        let mut input = self.prefix.clone();
        input.extend(crate::request::translate_messages(
            &self.messages,
            &self.model,
            self.thinking,
        ));
        input
    }
}

struct Continuation {
    key: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
    response_id: String,
}

struct Conn {
    socket: Socket,
    created: Instant,
    idle_since: Instant,
    state: Option<Continuation>,
}

impl Conn {
    fn fresh(&self) -> bool {
        self.idle_since.elapsed() < IDLE_LIMIT && self.created.elapsed() < MAX_AGE
    }
}

pub struct Upstream {
    http: reqwest::Client,
    http_url: String,
    ws_url: String,
    token: TokenSource,
    transport: Transport,
    pool: Mutex<HashMap<String, Vec<Conn>>>,
    spare: Mutex<Option<Conn>>,
    warming: Mutex<bool>,
}

/// What the transport did, for the bridge log.
#[derive(Default, Clone, Debug)]
pub struct Route {
    pub transport: &'static str,
    pub reused: bool,
    pub continued: bool,
    pub sent_items: usize,
    pub retried: bool,
    /// Why a pooled conversation could not be continued.
    pub miss: Option<String>,
}

enum Source {
    Ws {
        conn: Option<Box<Conn>>,
        session: Option<String>,
        key: String,
        tools: Vec<Value>,
        messages: Vec<Value>,
    },
    Http {
        body: ByteStream,
        decoder: crate::sse::SseDecoder,
        queue: VecDeque<Value>,
        ended: bool,
    },
}

/// One upstream response as a pull stream of Responses events.
pub struct Events {
    upstream: Arc<Upstream>,
    source: Source,
    pub route: Route,
    first: Option<Value>,
    response_id: Option<String>,
    done: bool,
    completed: bool,
}

impl Upstream {
    pub fn new(
        http: reqwest::Client,
        base_url: &str,
        token: TokenSource,
        transport: Transport,
    ) -> Arc<Self> {
        let base = base_url.trim_end_matches('/');
        let ws_url = format!(
            "{}/responses",
            base.replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1)
        );
        Arc::new(Self {
            http,
            http_url: format!("{base}/responses"),
            ws_url,
            token,
            transport,
            pool: Mutex::new(HashMap::new()),
            spare: Mutex::new(None),
            warming: Mutex::new(false),
        })
    }

    /// Keep one connected socket ready so a new session skips the handshake.
    pub fn warm(self: &Arc<Self>) {
        if self.transport == Transport::Http {
            return;
        }
        {
            let mut warming = self.warming.lock().unwrap();
            let spare_ok = self.spare.lock().unwrap().as_ref().is_some_and(Conn::fresh);
            if *warming || spare_ok {
                return;
            }
            *warming = true;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let conn = this.connect().await.ok();
            *this.spare.lock().unwrap() = conn;
            *this.warming.lock().unwrap() = false;
        });
    }

    async fn connect(&self) -> Result<Conn, ProviderError> {
        let token = (self.token)().await.map_err(|e| {
            ProviderError::new(
                401,
                "authentication_error",
                format!("ChatGPT sign-in unavailable: {e}. Run byom login."),
            )
        })?;
        let mut request = self.ws_url.as_str().into_client_request().map_err(|e| {
            ProviderError::new(500, "api_error", format!("invalid upstream URL: {e}"))
        })?;
        let auth = format!("Bearer {token}")
            .parse()
            .map_err(|_| ProviderError::new(401, "authentication_error", "invalid access token"))?;
        request.headers_mut().insert("authorization", auth);
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), true);
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok((socket, _))) => Ok(Conn {
                socket,
                created: Instant::now(),
                idle_since: Instant::now(),
                state: None,
            }),
            Ok(Err(tungstenite::Error::Http(response))) => {
                let status = response.status().as_u16();
                let body = response
                    .body()
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                    .unwrap_or(Value::Null);
                let mut error =
                    ProviderError::from_event(&json!({"error": body["error"], "status": status}));
                if body["error"].is_null() {
                    error = ProviderError::from_openai(
                        "websocket_rejected",
                        &format!("HTTP {status}"),
                        Some(status),
                    );
                }
                Err(error)
            }
            Ok(Err(e)) => Err(ProviderError::new(
                502,
                "api_error",
                format!("WebSocket connect failed: {e}"),
            )),
            Err(_) => Err(ProviderError::new(
                504,
                "api_error",
                "WebSocket connect timed out",
            )),
        }
    }

    fn take(&self, turn: &Turn) -> Option<(Conn, bool)> {
        if let Some(session) = &turn.session {
            let mut pool = self.pool.lock().unwrap();
            if let Some(conns) = pool.get_mut(session) {
                conns.retain(Conn::fresh);
                if let Some(i) = pick(conns, turn) {
                    return Some((conns.swap_remove(i), true));
                }
            }
        }
        let spare = self.spare.lock().unwrap().take().filter(Conn::fresh);
        spare.map(|c| (c, false))
    }

    fn put(&self, session: Option<String>, conn: Conn) {
        match session {
            Some(session) => {
                let mut pool = self.pool.lock().unwrap();
                let conns = pool.entry(session).or_default();
                if conns.len() < PER_SESSION {
                    conns.push(conn);
                }
                pool.retain(|_, conns| {
                    conns.retain(Conn::fresh);
                    !conns.is_empty()
                });
            }
            None => {
                let mut spare = self.spare.lock().unwrap();
                if spare.is_none() {
                    *spare = Some(conn);
                }
            }
        }
    }

    /// Start a response. Fails only before any event has been received, so the caller can
    /// still return a proper HTTP error.
    pub async fn start(self: &Arc<Self>, turn: Turn) -> Result<Events, ProviderError> {
        if self.transport == Transport::Auto {
            let mut retried = false;
            let mut pooled = self.take(&turn);
            self.warm();
            loop {
                let (conn, reused) = match pooled.take() {
                    Some(found) => found,
                    None => match self.connect().await {
                        Ok(conn) => (conn, false),
                        Err(e) if e.status == 502 || e.status == 504 => break,
                        Err(e) => return Err(e),
                    },
                };
                match self.send_ws(conn, reused, &turn).await {
                    Ok(mut events) => {
                        events.route.retried = retried;
                        return Ok(events);
                    }
                    Err(SendError::Retry) if !retried => retried = true,
                    Err(SendError::Retry) => break,
                    Err(SendError::Fatal(e)) => return Err(e),
                }
            }
        }
        self.send_http(&turn).await
    }

    async fn send_ws(
        self: &Arc<Self>,
        mut conn: Conn,
        reused: bool,
        turn: &Turn,
    ) -> Result<Events, SendError> {
        let key = turn.key();
        let mut request = turn.body.clone();
        let mut route = Route {
            transport: "websocket",
            reused,
            ..Route::default()
        };
        let planned = conn.state.take().map(|state| {
            let plan = continuation(&state, turn);
            (state, plan)
        });
        let input = match planned {
            Some((state, Ok(added))) => {
                route.continued = true;
                request.insert("previous_response_id".into(), json!(state.response_id));
                let mut input = Vec::new();
                if !added.is_empty() {
                    input.push(
                        json!({"type": "additional_tools", "role": "developer", "tools": added}),
                    );
                }
                input.extend(crate::request::translate_messages(
                    &turn.messages[state.messages.len()..],
                    &turn.model,
                    turn.thinking,
                ));
                input
            }
            Some((_, Err(miss))) => {
                route.miss = Some(miss);
                turn.full_input()
            }
            None => turn.full_input(),
        };
        route.sent_items = input.len();
        request.insert("input".into(), Value::Array(input));
        request.insert("type".into(), json!("response.create"));
        if conn
            .socket
            .send(Message::text(Value::Object(request).to_string()))
            .await
            .is_err()
        {
            return Err(SendError::Retry);
        }
        // Wait for the first event so connection and continuation failures can be retried.
        let first = match tokio::time::timeout(EVENT_TIMEOUT, next_ws(&mut conn.socket)).await {
            Ok(Some(Ok(event))) => event,
            Ok(_) => return Err(SendError::Retry),
            Err(_) => {
                return Err(SendError::Fatal(ProviderError::new(
                    504,
                    "api_error",
                    "No response from OpenAI",
                )));
            }
        };
        if matches!(first["type"].as_str(), Some("error" | "response.failed")) {
            let error = ProviderError::from_event(&first);
            let code = first["error"]["code"].as_str().unwrap_or("");
            // A rejected continuation is retried once with the full conversation.
            if route.continued && (code.contains("previous_response") || error.status == 400) {
                return Err(SendError::Retry);
            }
            return Err(SendError::Fatal(error));
        }
        let events = Events {
            upstream: self.clone(),
            source: Source::Ws {
                conn: Some(Box::new(conn)),
                session: turn.session.clone(),
                key,
                tools: turn.tools(),
                messages: turn.messages.clone(),
            },
            route,
            first: Some(first),
            response_id: None,
            done: false,
            completed: false,
        };
        Ok(events)
    }

    async fn send_http(self: &Arc<Self>, turn: &Turn) -> Result<Events, ProviderError> {
        let token = (self.token)().await.map_err(|e| {
            ProviderError::new(
                401,
                "authentication_error",
                format!("ChatGPT sign-in unavailable: {e}. Run byom login."),
            )
        })?;
        let mut request = turn.body.clone();
        let input = turn.full_input();
        let sent_items = input.len();
        request.insert("input".into(), Value::Array(input));
        let response = self
            .http
            .post(&self.http_url)
            .bearer_auth(token)
            .header("accept", "text/event-stream")
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(
                    502,
                    "api_error",
                    format!(
                        "OpenAI request failed: {}",
                        if e.is_timeout() {
                            "timeout"
                        } else {
                            "connection error"
                        }
                    ),
                )
            })?;
        let status = response.status().as_u16();
        if status >= 400 {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            return Err(ProviderError::from_event(
                &json!({"error": body["error"], "status": status}),
            ));
        }
        Ok(Events {
            upstream: self.clone(),
            source: Source::Http {
                body: Box::pin(response.bytes_stream()),
                decoder: crate::sse::SseDecoder::default(),
                queue: VecDeque::new(),
                ended: false,
            },
            route: Route {
                transport: "http",
                sent_items,
                ..Route::default()
            },
            first: None,
            response_id: None,
            done: false,
            completed: false,
        })
    }
}

/// Choose a pooled connection: one that can continue this turn, else an empty one. A
/// connection holding another conversation (a background request or subagent in the same
/// session) is reused only when the session is at capacity.
fn pick(conns: &[Conn], turn: &Turn) -> Option<usize> {
    let states: Vec<Option<&Continuation>> = conns.iter().map(|c| c.state.as_ref()).collect();
    pick_state(&states, turn)
}

fn pick_state(states: &[Option<&Continuation>], turn: &Turn) -> Option<usize> {
    states
        .iter()
        .position(|s| s.is_some_and(|s| continuation(s, turn).is_ok()))
        .or_else(|| states.iter().position(Option::is_none))
        .or_else(|| (states.len() >= PER_SESSION).then_some(0))
}

/// Plan a continuation of the remembered conversation: the tools added since, or why the
/// request cannot continue it.
fn continuation(state: &Continuation, turn: &Turn) -> Result<Vec<Value>, String> {
    let key = turn.key();
    if state.key != key {
        let (Ok(Value::Object(old)), Ok(Value::Object(new))) = (
            serde_json::from_str::<Value>(&state.key),
            serde_json::from_str::<Value>(&key),
        ) else {
            return Err("request settings changed".into());
        };
        let mut changed: Vec<&String> = old
            .keys()
            .chain(new.keys())
            .filter(|f| old.get(*f) != new.get(*f))
            .collect();
        changed.sort();
        changed.dedup();
        let changed: Vec<&str> = changed.iter().map(|s| s.as_str()).collect();
        return Err(format!("request settings changed: {}", changed.join(", ")));
    }
    let tools = turn.tools();
    if state.tools.iter().any(|t| !tools.contains(t)) {
        return Err("tools removed or changed".into());
    }
    let messages = &turn.messages;
    if messages.len() <= state.messages.len() {
        return Err(format!(
            "history not extended ({} <= {})",
            messages.len(),
            state.messages.len()
        ));
    }
    if let Some(index) = state
        .messages
        .iter()
        .zip(messages)
        .position(|(a, b)| a != b)
    {
        let shape = |m: &Value| {
            let blocks: Vec<String> = m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| {
                    let keys: Vec<&str> = b
                        .as_object()
                        .map(|o| o.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    format!("{}{:?}", b["type"].as_str().unwrap_or("?"), keys)
                })
                .collect();
            format!("{}:{}", m["role"].as_str().unwrap_or("?"), blocks.join(","))
        };
        let got: Vec<String> = messages[index..].iter().take(3).map(shape).collect();
        return Err(format!(
            "message {index} differs; remembered {} got {}",
            shape(&state.messages[index]),
            got.join(" | ")
        ));
    }
    Ok(tools
        .into_iter()
        .filter(|t| !state.tools.contains(t))
        .collect())
}

enum SendError {
    Retry,
    Fatal(ProviderError),
}

async fn next_ws(socket: &mut Socket) -> Option<Result<Value, ProviderError>> {
    loop {
        match socket.next().await? {
            Ok(Message::Text(text)) => {
                return Some(serde_json::from_str(&text).map_err(|_| {
                    ProviderError::new(502, "api_error", "OpenAI sent an invalid event")
                }));
            }
            Ok(Message::Binary(bytes)) => {
                return Some(serde_json::from_slice(&bytes).map_err(|_| {
                    ProviderError::new(502, "api_error", "OpenAI sent an invalid event")
                }));
            }
            Ok(Message::Close(_)) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

impl Events {
    /// Next Responses event; `None` after the terminal event.
    pub async fn next(&mut self) -> Option<Result<Value, ProviderError>> {
        if self.done {
            return None;
        }
        let event = if let Some(first) = self.first.take() {
            Ok(first)
        } else {
            match self.read().await? {
                Ok(event) => Ok(event),
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        };
        if let Ok(event) = &event {
            if let Some(id) = event["response"]["id"].as_str() {
                self.response_id = Some(id.to_owned());
            }
            match event["type"].as_str() {
                Some("response.completed" | "response.incomplete") => {
                    self.done = true;
                    self.completed = true;
                }
                Some("response.failed" | "error") => self.done = true,
                _ => {}
            }
        }
        Some(event)
    }

    async fn read(&mut self) -> Option<Result<Value, ProviderError>> {
        let lost = || ProviderError::new(502, "api_error", STREAM_LOST);
        match &mut self.source {
            Source::Ws { conn, .. } => {
                let socket = &mut conn.as_mut()?.socket;
                match tokio::time::timeout(EVENT_TIMEOUT, next_ws(socket)).await {
                    Ok(Some(result)) => Some(result),
                    Ok(None) => {
                        *conn = None;
                        Some(Err(lost()))
                    }
                    Err(_) => {
                        *conn = None;
                        Some(Err(ProviderError::new(
                            504,
                            "api_error",
                            "OpenAI stream stalled",
                        )))
                    }
                }
            }
            Source::Http {
                body,
                decoder,
                queue,
                ended,
            } => loop {
                if let Some(event) = queue.pop_front() {
                    return Some(Ok(event));
                }
                if *ended {
                    return Some(Err(lost()));
                }
                let chunk = match tokio::time::timeout(EVENT_TIMEOUT, body.next()).await {
                    Ok(Some(Ok(chunk))) => decoder.push(&chunk),
                    Ok(Some(Err(_))) => return Some(Err(lost())),
                    Ok(None) => {
                        *ended = true;
                        decoder.finish()
                    }
                    Err(_) => {
                        return Some(Err(ProviderError::new(
                            504,
                            "api_error",
                            "OpenAI stream stalled",
                        )));
                    }
                };
                let Ok(frames) = chunk else {
                    return Some(Err(ProviderError::new(
                        502,
                        "api_error",
                        "OpenAI sent a malformed stream",
                    )));
                };
                for frame in frames {
                    if frame.data == "[DONE]" {
                        continue;
                    }
                    match serde_json::from_str(&frame.data) {
                        Ok(event) => queue.push_back(event),
                        Err(_) => {
                            return Some(Err(ProviderError::new(
                                502,
                                "api_error",
                                "OpenAI sent an invalid event",
                            )));
                        }
                    }
                }
            },
        }
    }

    /// Return the connection to the pool, remembering the conversation including the
    /// assistant message just produced, so the next turn can continue it.
    pub fn finish(mut self, assistant: &[Value]) {
        if !self.done {
            return;
        }
        if let Source::Ws {
            conn,
            session,
            key,
            tools,
            messages,
        } = &mut self.source
            && let Some(mut conn) = conn.take().map(|c| *c)
        {
            if self.completed
                && let Some(response_id) = self.response_id.take()
            {
                let mut messages = std::mem::take(messages);
                messages.push(crate::request::normalize_message(
                    &json!({"role": "assistant", "content": assistant}),
                ));
                let messages = crate::request::merge_roles(messages);
                conn.state = Some(Continuation {
                    key: std::mem::take(key),
                    tools: std::mem::take(tools),
                    messages,
                    response_id,
                });
            }
            conn.idle_since = Instant::now();
            self.upstream.put(session.take(), conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(model: &str, messages: usize) -> Turn {
        Turn {
            session: Some("s".into()),
            body: Map::from_iter([("model".to_owned(), json!(model))]),
            prefix: Vec::new(),
            messages: (0..messages).map(|i| json!({"role": "user", "content": [{"type": "text", "text": i.to_string()}]})).collect(),
            model: model.into(),
            thinking: false,
        }
    }

    fn state(model: &str, messages: usize) -> Continuation {
        let t = turn(model, messages);
        Continuation {
            key: t.key(),
            tools: Vec::new(),
            messages: t.messages,
            response_id: "r".into(),
        }
    }

    #[test]
    fn background_requests_do_not_take_the_main_conversation() {
        let main = state("main", 2);
        let small = state("small", 1);
        // A title request on another model must not consume the main conversation.
        assert_eq!(pick_state(&[Some(&main)], &turn("small", 1)), None);
        // The main conversation continues on its own connection.
        assert_eq!(
            pick_state(&[Some(&small), Some(&main)], &turn("main", 3)),
            Some(1)
        );
        // An empty connection is preferred over evicting a conversation.
        assert_eq!(pick_state(&[Some(&main), None], &turn("small", 1)), Some(1));
        // At capacity, a stateful connection is reused.
        let full = [Some(&main), Some(&main), Some(&main), Some(&main)];
        assert_eq!(pick_state(&full, &turn("small", 1)), Some(0));
    }

    #[test]
    fn continuation_sends_only_added_tools() {
        let mut t = turn("m", 3);
        let mut s = state("m", 2);
        s.tools = vec![json!({"name": "A"})];
        t.prefix =
            vec![json!({"type": "additional_tools", "tools": [{"name": "A"}, {"name": "B"}]})];
        assert_eq!(continuation(&s, &t).unwrap(), vec![json!({"name": "B"})]);
        t.prefix = vec![json!({"type": "additional_tools", "tools": [{"name": "B"}]})];
        assert!(continuation(&s, &t).is_err());
        assert!(continuation(&s, &turn("m", 2)).is_err());
    }
}
