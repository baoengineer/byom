//! Forwarding for Anthropic-protocol routes: the Claude relay and Anthropic-compatible
//! providers. Requests and responses stream through; status and headers are preserved.
use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde_json::Value;

use crate::providers::{Auth, Provider};

/// Headers that describe one hop, not the message. Compression is negotiated per hop too:
/// the bridge reads usage from the stream, and loopback gains nothing from compression.
const HOP: &[&str] = &[
    "accept-encoding",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "proxy-connection",
    "upgrade",
    "te",
    "trailer",
];

/// Request headers sent to providers other than the Claude relay. Compatible servers reject
/// Claude-only beta flags, so `anthropic-beta` goes only to Anthropic.
const FORWARDED: &[&str] = &["content-type", "accept", "anthropic-version", "user-agent"];

pub const BRIDGE_KEY_HEADER: &str = "x-byom-key";

pub struct Outbound<'a> {
    pub method: Method,
    /// Path and query as received, such as `/v1/messages?beta=true`.
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    pub body: Bytes,
}

/// Why a request could not be forwarded, as an Anthropic error.
pub struct Refusal {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
}

/// Build the upstream request URL, headers and body for a provider.
pub fn prepare(
    provider: &Provider,
    upstream_model: Option<&str>,
    api_key: Option<&str>,
    bridge_key: &str,
    request: &Outbound,
) -> Result<(String, HeaderMap, Bytes), Refusal> {
    let url = format!(
        "{}{}",
        provider.base_url.trim_end_matches('/'),
        request.path
    );
    // Claude Code's own credential goes to Anthropic as sent; a saved key is used only when
    // Claude Code is not signed in, and sends the bridge key instead.
    let own_credential = provider.auth == Auth::ClaudeCode && {
        let carries_bridge_key = [
            request.headers.get("authorization"),
            request.headers.get("x-api-key"),
        ]
        .into_iter()
        .flatten()
        .any(|v| {
            v.to_str()
                .is_ok_and(|v| v.trim_start_matches("Bearer ") == bridge_key)
        });
        let has_credential = request.headers.contains_key("authorization")
            || request.headers.contains_key("x-api-key");
        has_credential && !carries_bridge_key
    };
    let mut headers = HeaderMap::new();
    for (name, value) in request.headers {
        let lower = name.as_str();
        let keep = if own_credential {
            !HOP.contains(&lower) && lower != BRIDGE_KEY_HEADER
        } else {
            FORWARDED.contains(&lower)
                || (lower == "anthropic-beta" && provider.id == crate::providers::CLAUDE_PROVIDER)
        };
        if keep {
            headers.append(name.clone(), value.clone());
        }
    }
    match provider.auth {
        Auth::ClaudeCode if own_credential => {}
        Auth::ClaudeCode if api_key.is_none() => {
            return Err(Refusal {
                status: 401,
                kind: "authentication_error",
                message: "Claude models need Claude Code signed in to Claude (run `claude auth login`), or an Anthropic API key: byom login anthropic".into(),
            });
        }
        Auth::None => {}
        _ => {
            let key = api_key.ok_or_else(|| Refusal {
                status: 401,
                kind: "authentication_error",
                message: format!(
                    "No API key for {}. Run: byom login {}",
                    provider.name, provider.id
                ),
            })?;
            let bearer = format!("Bearer {key}").parse().map_err(|_| Refusal {
                status: 401,
                kind: "authentication_error",
                message: format!(
                    "The API key for {} contains invalid characters",
                    provider.id
                ),
            })?;
            headers.insert("authorization", bearer);
            if let Ok(value) = key.parse() {
                headers.insert(HeaderName::from_static("x-api-key"), value);
            }
        }
    }
    // OpenRouter's app attribution: requests count toward byom in its public app rankings.
    if provider.id == "openrouter" {
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static("https://github.com/baoengineer/byom"),
        );
        headers.insert(
            HeaderName::from_static("x-title"),
            HeaderValue::from_static("byom"),
        );
    }
    let claude = provider.id == crate::providers::CLAUDE_PROVIDER;
    let body = rewrite_body(&request.body, upstream_model, !claude);
    Ok((url, headers, body))
}

/// Set the provider's model name and, for other providers, drop Claude-only tools; the
/// original bytes are kept when nothing changes.
fn rewrite_body(body: &Bytes, model: Option<&str>, strip: bool) -> Bytes {
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut changed = false;
    if let Some(model) = model
        && map.get("model").and_then(Value::as_str) != Some(model)
    {
        map.insert("model".into(), Value::String(model.into()));
        changed = true;
    }
    if strip && let Some(Value::Array(tools)) = map.get_mut("tools") {
        let before = tools.len();
        tools.retain(|t| !crate::providers::claude_only_tool(t));
        changed |= tools.len() != before;
    }
    if !changed {
        return body.clone();
    }
    Bytes::from(serde_json::to_vec(&map).unwrap_or_else(|_| body.to_vec()))
}

/// Send the prepared request and stream the response back unchanged.
pub async fn send(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Refusal> {
    let method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in &headers {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    let response = client
        .request(method, url)
        .headers(out)
        .body(body)
        .send()
        .await
        .map_err(|e| {
            let parsed = reqwest::Url::parse(url).ok();
            let host = parsed
                .as_ref()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default();
            if e.is_connect() && crate::providers::is_local_host(&host) {
                let port = parsed
                    .and_then(|u| u.port())
                    .map(|p| format!(":{p}"))
                    .unwrap_or_default();
                return Refusal {
                    status: 503,
                    kind: "api_error",
                    message: format!(
                        "{} {host}{port}. {}",
                        crate::providers::NOT_RUNNING,
                        crate::providers::NOT_RUNNING_HINT
                    ),
                };
            }
            Refusal {
                status: if e.is_timeout() { 504 } else { 502 },
                kind: "api_error",
                message: format!(
                    "Could not reach {}: {}",
                    host,
                    if e.is_connect() {
                        "connection failed"
                    } else if e.is_timeout() {
                        "timed out"
                    } else {
                        "request failed"
                    }
                ),
            }
        })?;
    let mut builder = Response::builder().status(
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
    );
    for (name, value) in response.headers() {
        if HOP.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    builder
        .body(Body::from_stream(stream))
        .map_err(|_| Refusal {
            status: 502,
            kind: "api_error",
            message: "Invalid upstream response headers".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Protocol;

    fn provider(id: &str, auth: Auth) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Anthropic,
            base_url: "https://up.example/api/".into(),
            auth,
            signup: String::new(),
            note: String::new(),
            models: Vec::new(),
            builtin: true,
            models_dev: String::new(),
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn relay_keeps_claude_credential_and_bytes() {
        let h = headers(&[
            ("authorization", "Bearer sk-ant-oat-x"),
            (BRIDGE_KEY_HEADER, "k"),
            ("anthropic-beta", "b"),
            ("host", "127.0.0.1"),
            ("accept-encoding", "gzip, br"),
            ("x-claude-code-session-id", "s1"),
        ]);
        let body = Bytes::from_static(br#"{"model":"claude-opus-5-5",  "x":1}"#);
        let req = Outbound {
            method: Method::POST,
            path: "/v1/messages?beta=true",
            headers: &h,
            body: body.clone(),
        };
        let (url, out, sent) = prepare(
            &provider("anthropic", Auth::ClaudeCode),
            Some("claude-opus-5-5"),
            None,
            "k",
            &req,
        )
        .ok()
        .unwrap();
        assert_eq!(url, "https://up.example/api/v1/messages?beta=true");
        assert_eq!(out["authorization"], "Bearer sk-ant-oat-x");
        assert_eq!(out["anthropic-beta"], "b");
        assert!(out.get(BRIDGE_KEY_HEADER).is_none() && out.get("host").is_none());
        assert!(out.get("accept-encoding").is_none());
        assert_eq!(out["x-claude-code-session-id"], "s1");
        assert_eq!(sent, body);
    }

    #[test]
    fn relay_refuses_without_claude_sign_in() {
        let h = headers(&[("authorization", "Bearer k")]);
        let req = Outbound {
            method: Method::POST,
            path: "/v1/messages",
            headers: &h,
            body: Bytes::new(),
        };
        assert_eq!(
            prepare(
                &provider("anthropic", Auth::ClaudeCode),
                None,
                None,
                "k",
                &req
            )
            .err()
            .unwrap()
            .status,
            401
        );
    }

    #[test]
    fn openrouter_requests_carry_app_attribution() {
        let h = headers(&[("authorization", "Bearer k")]);
        let req = Outbound {
            method: Method::POST,
            path: "/v1/messages",
            headers: &h,
            body: Bytes::new(),
        };
        let (_, out, _) = prepare(
            &provider("openrouter", Auth::ApiKey),
            None,
            Some("or-key"),
            "k",
            &req,
        )
        .ok()
        .unwrap();
        assert_eq!(out["x-title"], "byom");
        assert_eq!(out["http-referer"], "https://github.com/baoengineer/byom");
    }

    #[test]
    fn claude_only_tools_are_dropped_for_other_providers() {
        let body = Bytes::from_static(
            br#"{"model":"kimi/k3","tools":[{"name":"Read"},{"name":"ToolSearch"},{"name":"DeferredToolPlaceholder","defer_loading":true}]}"#,
        );
        let out: Value = serde_json::from_slice(&rewrite_body(&body, Some("k3"), true)).unwrap();
        assert_eq!(out["model"], "k3");
        assert_eq!(out["tools"], serde_json::json!([{"name": "Read"}]));
        assert_eq!(rewrite_body(&body, None, false), body);
    }

    #[tokio::test]
    async fn local_server_down_is_not_running() {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = reqwest::Client::new();
        let refusal = send(
            &client,
            Method::POST,
            &format!("http://127.0.0.1:{port}/v1/messages"),
            HeaderMap::new(),
            Bytes::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(refusal.status, 503);
        assert!(refusal.message.starts_with(crate::providers::NOT_RUNNING));
        assert!(refusal.message.contains(&format!("127.0.0.1:{port}")));
    }

    #[test]
    fn claude_sign_in_wins_over_a_saved_key() {
        let signed_in = headers(&[("authorization", "Bearer sk-ant-oat-x")]);
        let bridge_only = headers(&[
            ("authorization", "Bearer k"),
            (BRIDGE_KEY_HEADER, "k"),
            ("anthropic-beta", "b"),
            ("anthropic-version", "2023-06-01"),
            ("x-claude-code-session-id", "s1"),
        ]);
        let outbound = |h| Outbound {
            method: Method::POST,
            path: "/v1/messages",
            headers: h,
            body: Bytes::new(),
        };
        let anthropic = provider("anthropic", Auth::ClaudeCode);
        let (_, out, _) = prepare(
            &anthropic,
            None,
            Some("sk-ant-api-saved"),
            "k",
            &outbound(&signed_in),
        )
        .ok()
        .unwrap();
        assert_eq!(out["authorization"], "Bearer sk-ant-oat-x");
        assert!(out.get("x-api-key").is_none());
        let (_, out, _) = prepare(
            &anthropic,
            None,
            Some("sk-ant-api-saved"),
            "k",
            &outbound(&bridge_only),
        )
        .ok()
        .unwrap();
        assert_eq!(out["x-api-key"], "sk-ant-api-saved");
        assert_eq!(out["authorization"], "Bearer sk-ant-api-saved");
        assert_eq!(out["anthropic-beta"], "b");
        assert_eq!(out["anthropic-version"], "2023-06-01");
        assert!(out.get("x-claude-code-session-id").is_none());
        assert!(out.get(BRIDGE_KEY_HEADER).is_none());
    }

    #[test]
    fn compatible_provider_swaps_credential_model_and_betas() {
        let h = headers(&[
            ("authorization", "Bearer sk-ant-oat-x"),
            ("anthropic-beta", "b"),
            ("user-agent", "claude-cli/2"),
            ("content-type", "application/json"),
            ("x-claude-code-session-id", "s1"),
            ("x-stainless-os", "MacOS"),
            ("cookie", "c=1"),
        ]);
        let req = Outbound {
            method: Method::POST,
            path: "/v1/messages",
            headers: &h,
            body: Bytes::from_static(br#"{"model":"kimi/kimi-k3"}"#),
        };
        let (_, out, sent) = prepare(
            &provider("kimi", Auth::ApiKey),
            Some("kimi-k3"),
            Some("key1"),
            "k",
            &req,
        )
        .ok()
        .unwrap();
        assert_eq!(out["authorization"], "Bearer key1");
        assert_eq!(out["x-api-key"], "key1");
        assert!(out.get("anthropic-beta").is_none());
        assert_eq!(out["user-agent"], "claude-cli/2");
        assert_eq!(out["content-type"], "application/json");
        assert_eq!(out.len(), 4);
        assert_eq!(
            serde_json::from_slice::<Value>(&sent).unwrap()["model"],
            "kimi-k3"
        );
        assert!(prepare(&provider("kimi", Auth::ApiKey), None, None, "k", &req).is_err());
        let (_, out, _) = prepare(&provider("local", Auth::None), None, None, "k", &req)
            .ok()
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.get("authorization").is_none());
    }
}
