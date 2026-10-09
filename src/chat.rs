//! Anthropic Messages <-> OpenAI Chat Completions, for OpenAI-compatible providers.
use serde_json::{Map, Value, json};

use crate::response::{Event, ProviderError};

/// Build a streaming Chat Completions request from an Anthropic Messages request.
pub fn translate_request(request: &Value, model: &str, usage_option: bool) -> Value {
    let mut messages = Vec::new();
    let system = match &request["system"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.starts_with("x-anthropic-billing-header"))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    let normalized = crate::request::merge_roles(
        request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(crate::request::normalize_message),
    );
    for message in &normalized {
        let blocks = message["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match message["role"].as_str() {
            Some("assistant") => assistant(blocks, &mut messages),
            // Not every server accepts system messages after the first; send them as user text.
            Some("system") => {
                let text = texts(blocks);
                if !text.trim().is_empty() {
                    messages.push(json!({"role": "user", "content": text}));
                }
            }
            _ => user(blocks, &mut messages),
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    if usage_option {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(max) = request["max_tokens"].as_u64() {
        body.insert("max_tokens".into(), json!(max));
    }
    let tools: Vec<Value> = request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["type"].is_null() || t["type"] == "custom")
        .filter_map(|t| {
            let mut parameters = t["input_schema"].clone();
            if let Some(schema) = parameters.as_object_mut() {
                schema.remove("$schema");
            } else {
                parameters = json!({"type": "object", "properties": {}});
            }
            Some(json!({"type": "function", "function": {
                "name": t["name"].as_str()?,
                "description": t["description"].as_str().unwrap_or(""),
                "parameters": parameters,
            }}))
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        match request["tool_choice"]["type"].as_str() {
            Some("any") => {
                body.insert("tool_choice".into(), json!("required"));
            }
            Some("tool") => {
                body.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "function": {"name": request["tool_choice"]["name"]}}),
                );
            }
            Some("none") => {
                body.insert("tool_choice".into(), json!("none"));
            }
            _ => {}
        }
        if request["tool_choice"]["disable_parallel_tool_use"] == true {
            body.insert("parallel_tool_calls".into(), json!(false));
        }
    }
    Value::Object(body)
}

fn texts(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn user(blocks: &[Value], messages: &mut Vec<Value>) {
    let mut parts = Vec::new();
    let flush = |parts: &mut Vec<Value>, messages: &mut Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        let parts = std::mem::take(parts);
        let content = if parts.iter().all(|p| p["type"] == "text") {
            json!(
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            )
        } else {
            Value::Array(parts)
        };
        messages.push(json!({"role": "user", "content": content}));
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("tool_result") => {
                flush(&mut parts, messages);
                let mut content = match &block["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(items) => texts(items),
                    _ => String::new(),
                };
                if block["is_error"] == true {
                    content = format!("Tool error: {content}");
                }
                messages.push(json!({"role": "tool", "tool_call_id": block["tool_use_id"], "content": content}));
            }
            Some("text") => {
                if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                    parts.push(json!({"type": "text", "text": text}));
                }
            }
            Some("image") => {
                let source = &block["source"];
                let url = match source["type"].as_str() {
                    Some("base64") => format!(
                        "data:{};base64,{}",
                        source["media_type"].as_str().unwrap_or("image/png"),
                        source["data"].as_str().unwrap_or("")
                    ),
                    Some("url") => source["url"].as_str().unwrap_or("").to_owned(),
                    _ => continue,
                };
                parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            _ => {}
        }
    }
    flush(&mut parts, messages);
}

fn assistant(blocks: &[Value], messages: &mut Vec<Value>) {
    let text = texts(
        &blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .cloned()
            .collect::<Vec<_>>(),
    );
    let calls: Vec<Value> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| {
            json!({"id": b["id"], "type": "function", "function": {
                "name": b["name"],
                "arguments": serde_json::to_string(&b["input"]).unwrap_or_else(|_| "{}".into()),
            }})
        })
        .collect();
    if text.is_empty() && calls.is_empty() {
        return;
    }
    let mut message = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    messages.push(message);
}

#[derive(PartialEq)]
enum Open {
    None,
    Thinking,
    Text,
    Tool,
}

/// A tool call assembled from stream chunks.
struct ToolCall {
    index: Option<u64>,
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Chat Completions stream chunks -> Anthropic events.
pub struct ChatTranslator {
    model: String,
    thinking: bool,
    started: bool,
    finished: bool,
    open: Open,
    index: usize,
    stop: Option<&'static str>,
    used_tool: bool,
    pub content: Vec<Value>,
    tool_json: String,
    /// Tool calls are buffered until the provider finishes, as their chunks may interleave.
    tools: Vec<ToolCall>,
    pub usage: Value,
}

impl ChatTranslator {
    pub fn new(model: &str, thinking: bool) -> Self {
        Self {
            model: model.to_owned(),
            thinking,
            started: false,
            finished: false,
            open: Open::None,
            index: 0,
            stop: None,
            used_tool: false,
            content: Vec::new(),
            tool_json: String::new(),
            tools: Vec::new(),
            usage: json!({"input_tokens": 0, "output_tokens": 0}),
        }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn has_content(&self) -> bool {
        !self.content.is_empty()
    }

    fn start(&mut self, out: &mut Vec<Event>) {
        if !self.started {
            self.started = true;
            out.push(Event {
                name: "message_start",
                data: json!({"type": "message_start", "message": {
                    "id": "msg_byom", "type": "message", "role": "assistant", "model": self.model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }}),
            });
        }
    }

    fn close(&mut self, out: &mut Vec<Event>) {
        if self.open == Open::None {
            return;
        }
        if self.open == Open::Tool {
            let json = std::mem::take(&mut self.tool_json);
            self.content[self.index - 1]["input"] =
                serde_json::from_str(if json.trim().is_empty() { "{}" } else { &json })
                    .unwrap_or(json!({}));
        }
        out.push(Event {
            name: "content_block_stop",
            data: json!({"type": "content_block_stop", "index": self.index - 1}),
        });
        self.open = Open::None;
    }

    fn open_block(&mut self, kind: Open, block: Value, out: &mut Vec<Event>) {
        self.close(out);
        self.content.push(block.clone());
        out.push(Event {
            name: "content_block_start",
            data: json!({"type": "content_block_start", "index": self.index, "content_block": block}),
        });
        self.index += 1;
        self.open = kind;
    }

    fn delta(&mut self, delta: Value, out: &mut Vec<Event>) {
        let block = &mut self.content[self.index - 1];
        let append = |field: &str, block: &mut Value, text: &Value| {
            let joined = format!(
                "{}{}",
                block[field].as_str().unwrap_or(""),
                text.as_str().unwrap_or("")
            );
            block[field] = Value::String(joined);
        };
        match delta["type"].as_str() {
            Some("text_delta") => append("text", block, &delta["text"]),
            Some("thinking_delta") => append("thinking", block, &delta["thinking"]),
            Some("input_json_delta") => self
                .tool_json
                .push_str(delta["partial_json"].as_str().unwrap_or("")),
            _ => {}
        }
        out.push(Event {
            name: "content_block_delta",
            data: json!({"type": "content_block_delta", "index": self.index - 1, "delta": delta}),
        });
    }

    pub fn handle(&mut self, chunk: &Value) -> Result<Vec<Event>, ProviderError> {
        let mut out = Vec::new();
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(ProviderError::from_openai(
                error["code"]
                    .as_str()
                    .or(error["type"].as_str())
                    .unwrap_or("upstream_error"),
                error["message"].as_str().unwrap_or(""),
                error["status"].as_u64().map(|s| s as u16),
            ));
        }
        self.start(&mut out);
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            let input = usage["prompt_tokens"].as_u64().unwrap_or(0);
            let cached = usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            self.usage = json!({
                "input_tokens": input.saturating_sub(cached),
                "cache_read_input_tokens": cached,
                "cache_creation_input_tokens": 0,
                "output_tokens": usage["completion_tokens"].as_u64().unwrap_or(0),
            });
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            let reasoning = delta["reasoning_content"]
                .as_str()
                .or(delta["reasoning"].as_str())
                .unwrap_or("");
            if !reasoning.is_empty() && self.thinking {
                if self.open != Open::Thinking {
                    self.open_block(
                        Open::Thinking,
                        json!({"type": "thinking", "thinking": "", "signature": ""}),
                        &mut out,
                    );
                }
                self.delta(
                    json!({"type": "thinking_delta", "thinking": reasoning}),
                    &mut out,
                );
            }
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                if self.open != Open::Text {
                    self.open_block(Open::Text, json!({"type": "text", "text": ""}), &mut out);
                }
                self.delta(json!({"type": "text_delta", "text": text}), &mut out);
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                self.used_tool = true;
                self.tool_delta(call);
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.close(&mut out);
                self.flush_tools(&mut out);
                self.stop = Some(match reason {
                    "length" => "max_tokens",
                    "content_filter" => "refusal",
                    "tool_calls" | "function_call" => "tool_use",
                    _ if self.used_tool => "tool_use",
                    _ => "end_turn",
                });
            }
        }
        Ok(out)
    }

    /// Add a tool call chunk to its call: matched by index, else by ID, else the latest call.
    fn tool_delta(&mut self, call: &Value) {
        let index = call["index"].as_u64();
        let id = call["id"].as_str().filter(|id| !id.is_empty());
        let found = match (id, index) {
            (Some(id), _) => self
                .tools
                .iter()
                .rposition(|t| t.id.as_deref() == Some(id))
                .or_else(|| {
                    self.tools
                        .iter()
                        .rposition(|t| index.is_some() && t.index == index && t.id.is_none())
                }),
            (None, Some(_)) => self.tools.iter().rposition(|t| t.index == index),
            (None, None) => self.tools.len().checked_sub(1),
        };
        let position = found.unwrap_or_else(|| {
            self.tools.push(ToolCall {
                index,
                id: None,
                name: String::new(),
                arguments: String::new(),
            });
            self.tools.len() - 1
        });
        let tool = &mut self.tools[position];
        if tool.id.is_none() {
            tool.id = id.map(str::to_owned);
        }
        if let Some(name) = call["function"]["name"].as_str()
            && tool.name.is_empty()
        {
            tool.name = name.to_owned();
        }
        tool.arguments
            .push_str(call["function"]["arguments"].as_str().unwrap_or(""));
    }

    /// Emit the buffered tool calls as tool_use blocks.
    fn flush_tools(&mut self, out: &mut Vec<Event>) {
        for tool in std::mem::take(&mut self.tools) {
            let id = tool
                .id
                .unwrap_or_else(|| format!("toolu_{:024x}", rand::random::<u128>() >> 32));
            let block = json!({"type": "tool_use", "id": id, "name": tool.name, "input": {}});
            self.open_block(Open::Tool, block, out);
            if !tool.arguments.is_empty() {
                self.delta(
                    json!({"type": "input_json_delta", "partial_json": tool.arguments}),
                    out,
                );
            }
            self.close(out);
        }
    }

    /// End of stream: close blocks and emit the final message events. A stream that ends
    /// without `[DONE]` (`done`) or a finish reason was cut off.
    pub fn finish(&mut self, done: bool) -> Result<Vec<Event>, ProviderError> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        if !done && self.stop.is_none() {
            return Err(ProviderError::new(
                502,
                "api_error",
                "The provider's stream ended before the response completed",
            ));
        }
        self.start(&mut out);
        self.close(&mut out);
        self.flush_tools(&mut out);
        let stop = self.stop.unwrap_or(if self.used_tool {
            "tool_use"
        } else {
            "end_turn"
        });
        self.stop = Some(stop);
        out.push(Event {
            name: "message_delta",
            data: json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": self.usage}),
        });
        out.push(Event {
            name: "message_stop",
            data: json!({"type": "message_stop"}),
        });
        self.finished = true;
        Ok(out)
    }

    pub fn message(&self) -> Value {
        json!({
            "id": "msg_byom", "type": "message", "role": "assistant", "model": self.model,
            "content": self.content, "stop_reason": self.stop, "stop_sequence": null, "usage": self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_tools_results_and_images() {
        let request = json!({
            "model": "groq/llama", "max_tokens": 100, "system": [{"type": "text", "text": "Be brief."}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {"$schema": "x", "type": "object"}}],
            "tool_choice": {"type": "any"},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}]},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "x", "signature": "s"}, {"type": "text", "text": "ok"}, {"type": "tool_use", "id": "c1", "name": "Read", "input": {"p": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1", "content": "data", "is_error": true}]},
                {"role": "system", "content": "reminder"},
            ],
        });
        let body = translate_request(&request, "llama", true);
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(
            m[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA"
        );
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"p\":1}");
        assert_eq!(m[2]["content"], "ok");
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "c1", "content": "Tool error: data"})
        );
        assert_eq!(m[4]["role"], "user");
        assert_eq!(body["tools"][0]["function"]["name"], "Read");
        assert!(
            body["tools"][0]["function"]["parameters"]
                .get("$schema")
                .is_none()
        );
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stream_maps_text_reasoning_tools_and_usage() {
        let mut t = ChatTranslator::new("m", true);
        let mut events = Vec::new();
        for chunk in [
            json!({"choices": [{"delta": {"reasoning_content": "plan"}}]}),
            json!({"choices": [{"delta": {"content": "Hi"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Read", "arguments": "{\"a\""}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 4}}}),
        ] {
            events.extend(t.handle(&chunk).ok().unwrap());
        }
        events.extend(t.finish(true).ok().unwrap());
        let message = t.message();
        assert_eq!(message["content"][0]["thinking"], "plan");
        assert_eq!(message["content"][1]["text"], "Hi");
        assert_eq!(message["content"][2]["input"]["a"], 1);
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["usage"]["input_tokens"], 6);
        assert_eq!(events.last().unwrap().name, "message_stop");
        let starts = events
            .iter()
            .filter(|e| e.name == "content_block_start")
            .count();
        let stops = events
            .iter()
            .filter(|e| e.name == "content_block_stop")
            .count();
        assert_eq!((starts, stops), (3, 3));
    }

    #[test]
    fn interleaved_parallel_tool_calls_stay_separate() {
        let mut t = ChatTranslator::new("m", false);
        let mut events = Vec::new();
        for chunk in [
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {"name": "Read", "arguments": "{\"p\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 1, "function": {"name": "Grep", "arguments": "{\"q\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 1, "function": {"arguments": "2}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"id": "c", "function": {"name": "Ls", "arguments": "{}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"id": "d", "function": {"name": "Ls", "arguments": "{\"r\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": "3}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ] {
            events.extend(t.handle(&chunk).ok().unwrap());
        }
        events.extend(t.finish(true).ok().unwrap());
        let content = t.message()["content"].as_array().unwrap().clone();
        let summary: Vec<_> = content
            .iter()
            .map(|b| (b["name"].as_str().unwrap(), b["input"].clone()))
            .collect();
        assert_eq!(
            summary,
            [
                ("Read", json!({"p": 1})),
                ("Grep", json!({"q": 2})),
                ("Ls", json!({})),
                ("Ls", json!({"r": 3})),
            ]
        );
        let ids: Vec<_> = content.iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert_eq!((ids[0], ids[2], ids[3]), ("a", "c", "d"));
        assert!(ids[1].starts_with("toolu_"));
        let starts: Vec<_> = events
            .iter()
            .filter(|e| e.name == "content_block_start")
            .map(|e| e.data["index"].as_u64().unwrap())
            .collect();
        assert_eq!(starts, [0, 1, 2, 3]);
        let mut other = ChatTranslator::new("m", false);
        other
            .handle(&json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "Read"}}]}, "finish_reason": "tool_calls"}]}))
            .ok()
            .unwrap();
        assert_ne!(other.message()["content"][0]["id"], content[1]["id"]);
    }

    #[test]
    fn stream_cut_off_before_finish_is_an_error() {
        let mut t = ChatTranslator::new("m", false);
        t.handle(&json!({"choices": [{"delta": {"content": "Hi"}}]}))
            .ok()
            .unwrap();
        assert_eq!(t.finish(false).err().unwrap().status, 502);
        assert!(!t.finished());
        let events = t.finish(true).ok().unwrap();
        assert_eq!(events.last().unwrap().name, "message_stop");
        let mut t = ChatTranslator::new("m", false);
        t.handle(&json!({"choices": [{"delta": {"content": "Hi"}, "finish_reason": "stop"}]}))
            .ok()
            .unwrap();
        assert!(t.finish(false).is_ok());
    }

    #[test]
    fn stream_errors_map_to_claude_errors() {
        let mut t = ChatTranslator::new("m", false);
        let error = t
            .handle(&json!({"error": {"code": "rate_limit_exceeded", "message": "slow down"}}))
            .err()
            .unwrap();
        assert_eq!(error.kind, "rate_limit_error");
    }
}
