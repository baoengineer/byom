//! Forwarding for Anthropic-protocol routes: the Claude relay and Anthropic-compatible
//! providers. Requests and responses stream through; status and headers are preserved.
use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde_json::Value;

use crate::providers::{Auth, Provider};

/// Headers that describe one hop, not the message.
const HOP: &[&str] = &[
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

pub const BRIDGE_KEY_HEADER: &str = "x-byoclaude-key";

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
    let mut headers = HeaderMap::new();
    for (name, value) in request.headers {
        let lower = name.as_str();
        if HOP.contains(&lower) || lower == BRIDGE_KEY_HEADER {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    match provider.auth {
        Auth::ClaudeCode if api_key.is_none() => {
            // The bridge key in Authorization means Claude Code is not signed in to Claude.
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
            if carries_bridge_key || !has_credential {
                return Err(Refusal {
                    status: 401,
                    kind: "authentication_error",
                    message: "Claude models need Claude Code signed in to Claude (run `claude auth login`), or an Anthropic API key: byoclaude login anthropic".into(),
                });
            }
        }
        Auth::None => {
            headers.remove("authorization");
            headers.remove("x-api-key");
        }
        _ => {
            headers.remove("authorization");
            headers.remove("x-api-key");
            // Compatible servers reject Claude-only beta flags.
            if provider.id != crate::providers::CLAUDE_PROVIDER {
                headers.remove("anthropic-beta");
            }
            let key = api_key.ok_or_else(|| Refusal {
                status: 401,
                kind: "authentication_error",
                message: format!(
                    "No API key for {}. Run: byoclaude login {}",
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
    let body = match upstream_model {
        Some(model) => rewrite_model(&request.body, model),
        None => request.body.clone(),
    };
    Ok((url, headers, body))
}

/// Replace the request's model; the original bytes are kept when it already matches.
fn rewrite_model(body: &Bytes, model: &str) -> Bytes {
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    if map.get("model").and_then(Value::as_str) == Some(model) {
        return body.clone();
    }
    map.insert("model".into(), Value::String(model.into()));
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
        .map_err(|e| Refusal {
            status: if e.is_timeout() { 504 } else { 502 },
            kind: "api_error",
            message: format!(
                "Could not reach {}: {}",
                reqwest::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default(),
                if e.is_connect() {
                    "connection failed"
                } else if e.is_timeout() {
                    "timed out"
                } else {
                    "request failed"
                }
            ),
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
    fn compatible_provider_swaps_credential_model_and_betas() {
        let h = headers(&[
            ("authorization", "Bearer sk-ant-oat-x"),
            ("anthropic-beta", "b"),
            ("user-agent", "claude-cli/2"),
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
        assert_eq!(
            serde_json::from_slice::<Value>(&sent).unwrap()["model"],
            "kimi-k3"
        );
        assert!(prepare(&provider("kimi", Auth::ApiKey), None, None, "k", &req).is_err());
    }
}
