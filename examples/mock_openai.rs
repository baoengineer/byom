//! Scripted stand-in for the OpenAI Responses WebSocket API, used by `tests/e2e/run.sh` to
//! drive the real Claude Code through the bridge without spending plan usage.
//!
//! Script: the first prompt gets reasoning plus one Bash call; a prompt containing
//! SEARCHTEST gets one WebSearch call; requests carrying the hosted web_search tool get a
//! search result; everything else gets a text answer. Each request is logged as one JSON
//! line to the file named by `MOCK_LOG`.
//!
//! `MOCK_THINKING`, `MOCK_COMMAND` and `MOCK_ANSWER` replace the scripted reasoning
//! summary, Bash command and final answer; the README demo uses them.
//!
//! Usage: cargo run --example mock_openai -- <port>
use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .expect("usage: mock_openai <port>");
    let log = std::env::var("MOCK_LOG").expect("MOCK_LOG must name a log file");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock port");
    let issued = Arc::new(Mutex::new(HashSet::<&'static str>::new()));
    let mut connections = 0u32;
    println!("mock listening {port}");
    while let Ok((stream, _)) = listener.accept().await {
        connections += 1;
        let (log, issued, conn) = (log.clone(), issued.clone(), connections);
        tokio::spawn(async move {
            let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let body: Value = serde_json::from_str(&text).unwrap_or_default();
                for event in respond(&body, conn, &issued, &log) {
                    if socket.send(Message::text(event.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
}

fn respond(
    body: &Value,
    conn: u32,
    issued: &Mutex<HashSet<&'static str>>,
    log: &str,
) -> Vec<Value> {
    let input = body["input"].as_array().cloned().unwrap_or_default();
    let types: Vec<&str> = input
        .iter()
        .map(|i| i["type"].as_str().or(i["role"].as_str()).unwrap_or("?"))
        .collect();
    let hosted: Vec<&str> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["type"].as_str())
        .collect();
    let record = json!({
        "conn": conn, "session": body["prompt_cache_key"], "model": body["model"],
        "previous": body["previous_response_id"], "items": input.len(), "types": types,
        "hosted": hosted, "reasoning": body["reasoning"],
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(file, "{record}");
    }

    let id = format!("resp_{conn}_{}", input.len());
    let text = Value::Array(input.clone()).to_string();
    let has_tools = input.iter().any(|i| i["type"] == "additional_tools");
    let mut issued = issued.lock().unwrap();
    let mut events =
        vec![json!({"type": "response.created", "response": {"id": id, "status": "in_progress"}})];
    if hosted.contains(&"web_search") {
        let action = json!({"type": "search", "query": "mock query", "sources": [{"type": "url", "url": "https://example.com/mock-source"}]});
        events.push(json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "web_search_call", "id": "ws1"}}));
        events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "web_search_call", "id": "ws1", "status": "completed", "action": action}}));
        events.extend(message(1, "Mock search answer."));
    } else if text.contains("SEARCHTEST") && issued.insert("search") {
        events.extend(call(
            0,
            &format!("call_ws_{conn}"),
            "WebSearch",
            r#"{"query":"mock query"}"#,
        ));
    } else if issued.contains("bash") || text.contains("SEARCHTEST") || !has_tools {
        events.extend(message(0, &script("MOCK_ANSWER", "Mock done.")));
    } else {
        issued.insert("bash");
        let thinking = script("MOCK_THINKING", "Planning a shell check.");
        let arguments = json!({"command": script("MOCK_COMMAND", "echo mock-ok"), "description": "Run command"});
        events.push(json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "r1"}}));
        events.push(json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "delta": thinking}));
        events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "r1", "encrypted_content": format!("ENC{conn}")}}));
        events.extend(call(
            1,
            &format!("call_{conn}_{}", input.len()),
            "Bash",
            &arguments.to_string(),
        ));
    }
    events.push(json!({"type": "response.completed", "response": {"id": id, "status": "completed", "output": [], "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 40}, "output_tokens": 10}}}));
    events
}

fn script(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn message(index: u64, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "response.output_item.added", "output_index": index, "item": {"type": "message", "id": "m", "role": "assistant"}}),
        json!({"type": "response.content_part.added", "output_index": index, "content_index": 0, "part": {"type": "output_text", "text": ""}}),
        json!({"type": "response.output_text.delta", "output_index": index, "content_index": 0, "delta": text}),
        json!({"type": "response.content_part.done", "output_index": index, "content_index": 0}),
        json!({"type": "response.output_item.done", "output_index": index, "item": {"type": "message", "id": "m", "role": "assistant", "content": [{"type": "output_text", "text": text}]}}),
    ]
}

fn call(index: u64, call_id: &str, name: &str, arguments: &str) -> Vec<Value> {
    vec![
        json!({"type": "response.output_item.added", "output_index": index, "item": {"type": "function_call", "id": "f", "call_id": call_id, "name": name}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": index, "delta": arguments}),
        json!({"type": "response.output_item.done", "output_index": index, "item": {"type": "function_call", "id": "f", "call_id": call_id, "name": name, "arguments": arguments}}),
    ]
}
