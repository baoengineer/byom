//! OpenAI Responses stream events -> Anthropic Messages stream events.
//!
//! Output items are emitted strictly in `output_index` order: events for a later item are
//! buffered until earlier items finish, so Anthropic content blocks never interleave.
//! Unknown event and item types are ignored.
use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::request::encode_signature;

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }

    /// Map an OpenAI error code and message onto the Anthropic error Claude Code reacts to.
    pub fn from_openai(code: &str, message: &str, http_status: Option<u16>) -> Self {
        let message = if message.is_empty() { code } else { message };
        if code.contains("usage_limit")
            || code.contains("usage_not_included")
            || code == "rate_limit_exceeded"
            || http_status == Some(429)
        {
            return Self::new(
                429,
                "rate_limit_error",
                format!(
                    "ChatGPT plan limit reached ({code}): {message} Raise this app's limit or check usage at https://chatgpt.com/settings/usage"
                ),
            );
        }
        if code == "context_length_exceeded" || message.contains("context window") {
            // Claude Code compacts on this wording.
            return Self::new(
                400,
                "invalid_request_error",
                format!("prompt is too long: {message}"),
            );
        }
        match http_status {
            Some(401) => Self::new(
                401,
                "authentication_error",
                format!("ChatGPT sign-in was rejected ({code}): {message} Run byom login."),
            ),
            Some(403) => Self::new(403, "permission_error", format!("{code}: {message}")),
            Some(404) => Self::new(404, "not_found_error", format!("{code}: {message}")),
            Some(400 | 422) => {
                Self::new(400, "invalid_request_error", format!("{code}: {message}"))
            }
            Some(503) | Some(529) => {
                Self::new(529, "overloaded_error", format!("{code}: {message}"))
            }
            _ if code == "server_is_overloaded" || code == "slow_down" => {
                Self::new(529, "overloaded_error", format!("{code}: {message}"))
            }
            _ if code.starts_with("invalid")
                || code.contains("not_supported")
                || code.contains("unsupported") =>
            {
                Self::new(400, "invalid_request_error", format!("{code}: {message}"))
            }
            _ => Self::new(500, "api_error", format!("{code}: {message}")),
        }
    }

    pub fn from_event(event: &Value) -> Self {
        let error = [&event["error"], &event["response"]["error"], event]
            .into_iter()
            .find(|e| e.get("code").is_some() || e.get("message").is_some())
            .unwrap_or(&Value::Null);
        Self::from_openai(
            error["code"].as_str().unwrap_or("upstream_error"),
            error["message"].as_str().unwrap_or(""),
            event["status"].as_u64().map(|s| s as u16),
        )
    }

    pub fn event(&self) -> Value {
        json!({"type": "error", "error": {"type": self.kind, "message": self.message}})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub data: Value,
}

impl Event {
    pub fn sse(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.name, self.data)
    }
}

#[derive(Default, PartialEq)]
enum Kind {
    Message,
    Reasoning,
    Function,
    WebSearch,
    #[default]
    Other,
}

#[derive(Default)]
struct Item {
    kind: Kind,
    buffered: Vec<Value>,
    open: Option<usize>,
    streamed: bool,
    summaries: u64,
}

pub struct Translator {
    model: String,
    thinking: bool,
    started: bool,
    finished: bool,
    used_tool: bool,
    items: BTreeMap<u64, Item>,
    cursor: u64,
    /// Final Anthropic content blocks, in order.
    pub content: Vec<Value>,
    tool_json: BTreeMap<usize, String>,
    pub response_id: Option<String>,
    pub stop_reason: Option<&'static str>,
    pub usage: Value,
}

impl Translator {
    pub fn new(model: &str, thinking: bool) -> Self {
        Self {
            model: model.to_owned(),
            thinking,
            started: false,
            finished: false,
            used_tool: false,
            items: BTreeMap::new(),
            cursor: 0,
            content: Vec::new(),
            tool_json: BTreeMap::new(),
            response_id: None,
            stop_reason: None,
            usage: json!({"input_tokens": 0, "output_tokens": 0}),
        }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// True once anything beyond `message_start` has been produced.
    pub fn has_content(&self) -> bool {
        !self.content.is_empty()
    }

    pub fn handle(&mut self, event: &Value) -> Result<Vec<Event>, ProviderError> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        let ty = event["type"].as_str().unwrap_or("");
        match ty {
            "error" | "response.failed" => return Err(ProviderError::from_event(event)),
            "response.created" | "response.in_progress" => {
                if let Some(id) = event["response"]["id"].as_str() {
                    self.response_id = Some(id.to_owned());
                }
                self.start(&mut out);
            }
            "response.completed" | "response.incomplete" => {
                self.start(&mut out);
                self.complete(event, &mut out);
            }
            _ => {
                let Some(index) = event["output_index"].as_u64() else {
                    return Ok(out);
                };
                self.start(&mut out);
                if index == self.cursor {
                    self.apply(index, event, &mut out);
                    self.advance(&mut out);
                } else if index > self.cursor {
                    self.items
                        .entry(index)
                        .or_default()
                        .buffered
                        .push(event.clone());
                }
            }
        }
        Ok(out)
    }

    fn start(&mut self, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(Event {
            name: "message_start",
            data: json!({"type": "message_start", "message": {
                "id": self.response_id.clone().unwrap_or_else(|| "msg_byom".into()),
                "type": "message", "role": "assistant", "model": self.model, "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }}),
        });
    }

    /// Replay buffered events for each item that reaches the cursor.
    fn advance(&mut self, out: &mut Vec<Event>) {
        loop {
            let index = self.cursor;
            let buffered = self
                .items
                .get_mut(&index)
                .map(|item| std::mem::take(&mut item.buffered))
                .unwrap_or_default();
            if buffered.is_empty() {
                return;
            }
            for event in buffered {
                if index >= self.cursor {
                    self.apply(index, &event, out);
                }
            }
        }
    }

    fn open(&mut self, index: u64, block: Value, out: &mut Vec<Event>) -> usize {
        let block_index = self.content.len();
        self.content.push(block.clone());
        out.push(Event {
            name: "content_block_start",
            data: json!({"type": "content_block_start", "index": block_index, "content_block": block}),
        });
        if let Some(item) = self.items.get_mut(&index) {
            item.open = Some(block_index);
        }
        block_index
    }

    fn delta(&mut self, block: usize, delta: Value, out: &mut Vec<Event>) {
        match delta["type"].as_str() {
            Some("text_delta") => append(&mut self.content[block]["text"], &delta["text"]),
            Some("thinking_delta") => {
                append(&mut self.content[block]["thinking"], &delta["thinking"])
            }
            Some("input_json_delta") => self
                .tool_json
                .entry(block)
                .or_default()
                .push_str(delta["partial_json"].as_str().unwrap_or("")),
            Some("signature_delta") => {
                self.content[block]["signature"] = delta["signature"].clone()
            }
            _ => {}
        }
        out.push(Event {
            name: "content_block_delta",
            data: json!({"type": "content_block_delta", "index": block, "delta": delta}),
        });
    }

    fn close(&mut self, index: u64, out: &mut Vec<Event>) {
        let Some(block) = self.items.get_mut(&index).and_then(|i| i.open.take()) else {
            return;
        };
        if let Some(json) = self.tool_json.remove(&block) {
            self.content[block]["input"] =
                serde_json::from_str(if json.trim().is_empty() { "{}" } else { &json })
                    .unwrap_or(json!({}));
        }
        out.push(Event {
            name: "content_block_stop",
            data: json!({"type": "content_block_stop", "index": block}),
        });
    }

    fn apply(&mut self, index: u64, event: &Value, out: &mut Vec<Event>) {
        let ty = event["type"].as_str().unwrap_or("");
        match ty {
            "response.output_item.added" => {
                let item = &event["item"];
                let kind = match item["type"].as_str() {
                    Some("message") => Kind::Message,
                    Some("reasoning") => Kind::Reasoning,
                    Some("function_call") => Kind::Function,
                    Some("web_search_call") => Kind::WebSearch,
                    _ => Kind::Other,
                };
                let entry = self.items.entry(index).or_default();
                entry.kind = kind;
                if entry.kind == Kind::Function {
                    self.used_tool = true;
                    let block = json!({"type": "tool_use", "id": item["call_id"], "name": item["name"], "input": {}});
                    self.open(index, block, out);
                }
            }
            "response.content_part.added" => {
                self.close(index, out);
                if matches!(
                    event["part"]["type"].as_str(),
                    Some("output_text" | "refusal")
                ) {
                    self.open(index, json!({"type": "text", "text": ""}), out);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let block = match self.items.get(&index).and_then(|i| i.open) {
                    Some(block) => block,
                    None => self.open(index, json!({"type": "text", "text": ""}), out),
                };
                if let Some(item) = self.items.get_mut(&index) {
                    item.streamed = true;
                }
                self.delta(
                    block,
                    json!({"type": "text_delta", "text": event["delta"]}),
                    out,
                );
            }
            "response.content_part.done" => self.close(index, out),
            "response.reasoning_summary_text.delta" if self.thinking => {
                let (open, summaries, summary_index) = {
                    let item = self.items.entry(index).or_default();
                    (
                        item.open,
                        item.summaries,
                        event["summary_index"].as_u64().unwrap_or(0),
                    )
                };
                let block = match open {
                    Some(block) => block,
                    None => self.open(
                        index,
                        json!({"type": "thinking", "thinking": "", "signature": ""}),
                        out,
                    ),
                };
                let mut text = event["delta"].as_str().unwrap_or("").to_owned();
                if summary_index > summaries {
                    text.insert_str(0, "\n\n");
                }
                if let Some(item) = self.items.get_mut(&index) {
                    item.summaries = summary_index;
                    item.streamed = true;
                }
                self.delta(
                    block,
                    json!({"type": "thinking_delta", "thinking": text}),
                    out,
                );
            }
            "response.function_call_arguments.delta" => {
                if let Some(block) = self.items.get(&index).and_then(|i| i.open) {
                    if let Some(item) = self.items.get_mut(&index) {
                        item.streamed = true;
                    }
                    self.delta(
                        block,
                        json!({"type": "input_json_delta", "partial_json": event["delta"]}),
                        out,
                    );
                }
            }
            "response.output_item.done" => {
                self.item_done(index, &event["item"], out);
                if let Some(item) = self.items.get_mut(&index) {
                    item.kind = Kind::Other;
                    item.streamed = true;
                }
                self.cursor = index + 1;
            }
            _ => {}
        }
    }

    fn item_done(&mut self, index: u64, item: &Value, out: &mut Vec<Event>) {
        let (kind, open, streamed) = match self.items.get(&index) {
            Some(i) => (
                match i.kind {
                    Kind::Message => 1,
                    Kind::Reasoning => 2,
                    Kind::Function => 3,
                    Kind::WebSearch => 4,
                    Kind::Other => 0,
                },
                i.open,
                i.streamed,
            ),
            None => (0, None, false),
        };
        match kind {
            1 => {
                if !streamed {
                    for part in item["content"].as_array().into_iter().flatten() {
                        let text = part["text"]
                            .as_str()
                            .or(part["refusal"].as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            let block = self.open(index, json!({"type": "text", "text": ""}), out);
                            self.delta(block, json!({"type": "text_delta", "text": text}), out);
                            self.close(index, out);
                        }
                    }
                }
                self.close(index, out);
            }
            2 => {
                if !self.thinking {
                    return;
                }
                let signature = item["encrypted_content"]
                    .as_str()
                    .map(|e| encode_signature(&self.model, e));
                match (open, signature) {
                    (Some(block), Some(signature)) => {
                        self.delta(
                            block,
                            json!({"type": "signature_delta", "signature": signature}),
                            out,
                        );
                        self.close(index, out);
                    }
                    (Some(_), None) => self.close(index, out),
                    (None, Some(signature)) => {
                        self.open(
                            index,
                            json!({"type": "redacted_thinking", "data": signature}),
                            out,
                        );
                        self.close(index, out);
                    }
                    (None, None) => {}
                }
            }
            3 => {
                if !streamed && let Some(block) = open {
                    let arguments = item["arguments"].as_str().unwrap_or("{}").to_owned();
                    self.delta(
                        block,
                        json!({"type": "input_json_delta", "partial_json": arguments}),
                        out,
                    );
                }
                self.close(index, out);
            }
            4 => {
                let id = format!("srvtoolu_{}", item["id"].as_str().unwrap_or("search"));
                let action = &item["action"];
                let query = action["query"]
                    .as_str()
                    .or_else(|| action["queries"][0].as_str())
                    .unwrap_or("");
                self.open(index, json!({"type": "server_tool_use", "id": id, "name": "web_search", "input": {"query": query}}), out);
                self.close(index, out);
                let results: Vec<Value> = action["sources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["url"].as_str())
                    .map(|url| json!({"type": "web_search_result", "url": url, "title": url, "encrypted_content": "", "page_age": null}))
                    .collect();
                self.open(index, json!({"type": "web_search_tool_result", "tool_use_id": id, "content": results}), out);
                self.close(index, out);
            }
            _ => {}
        }
    }

    fn complete(&mut self, event: &Value, out: &mut Vec<Event>) {
        // Flush items whose events were buffered behind an item that never finished.
        let pending: Vec<u64> = self
            .items
            .keys()
            .copied()
            .filter(|i| *i >= self.cursor)
            .collect();
        for index in pending {
            let buffered = self
                .items
                .get_mut(&index)
                .map(|i| std::mem::take(&mut i.buffered))
                .unwrap_or_default();
            for e in buffered {
                self.apply(index, &e, out);
            }
            self.close(index, out);
        }
        let response = &event["response"];
        let incomplete = event["type"] == "response.incomplete";
        let reason = response["incomplete_details"]["reason"].as_str();
        let stop = if incomplete && reason == Some("content_filter") {
            "refusal"
        } else if incomplete {
            "max_tokens"
        } else if self.used_tool {
            "tool_use"
        } else {
            "end_turn"
        };
        let usage = &response["usage"];
        let input = usage["input_tokens"].as_u64().unwrap_or(0);
        let cached = usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        self.usage = json!({
            "input_tokens": input.saturating_sub(cached),
            "cache_read_input_tokens": cached,
            "cache_creation_input_tokens": 0,
            "output_tokens": usage["output_tokens"].as_u64().unwrap_or(0),
        });
        self.stop_reason = Some(stop);
        out.push(Event {
            name: "message_delta",
            data: json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": self.usage}),
        });
        out.push(Event {
            name: "message_stop",
            data: json!({"type": "message_stop"}),
        });
        self.finished = true;
    }

    /// The complete assistant message, for non-streaming responses.
    pub fn message(&self) -> Value {
        json!({
            "id": self.response_id.clone().unwrap_or_else(|| "msg_byom".into()),
            "type": "message", "role": "assistant", "model": self.model,
            "content": self.content, "stop_reason": self.stop_reason, "stop_sequence": null,
            "usage": self.usage,
        })
    }
}

fn append(target: &mut Value, delta: &Value) {
    let joined = format!(
        "{}{}",
        target.as_str().unwrap_or(""),
        delta.as_str().unwrap_or("")
    );
    *target = Value::String(joined);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &mut Translator, events: &[Value]) -> Vec<Event> {
        events.iter().flat_map(|e| t.handle(e).unwrap()).collect()
    }

    fn names(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e.data["delta"]["type"].as_str() {
                Some(d) => format!("{}:{d}", e.name),
                None => match e.data["content_block"]["type"].as_str() {
                    Some(b) => format!("{}:{b}", e.name),
                    None => e.name.to_owned(),
                },
            })
            .collect()
    }

    #[test]
    fn unstored_text_stream_with_empty_completed_output() {
        let mut t = Translator::new("m", false);
        let out = run(
            &mut t,
            &[
                json!({"type":"response.created","response":{"id":"r1"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"i"}}),
                json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text"}}),
                json!({"type":"response.output_text.delta","output_index":0,"delta":"Hi"}),
                json!({"type":"response.content_part.done","output_index":0}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"Hi"}]}}),
                json!({"type":"response.completed","response":{"output":[],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":2}}}),
            ],
        );
        assert_eq!(
            names(&out),
            [
                "message_start",
                "content_block_start:text",
                "content_block_delta:text_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(t.usage["input_tokens"], 6);
        assert_eq!(t.usage["cache_read_input_tokens"], 4);
        assert_eq!(t.message()["content"][0]["text"], "Hi");
        assert_eq!(t.message()["stop_reason"], "end_turn");
    }

    #[test]
    fn reasoning_becomes_signed_thinking_then_tool_use() {
        let mut t = Translator::new("m", true);
        let out = run(
            &mut t,
            &[
                json!({"type":"response.created","response":{"id":"r1"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}),
                json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"Plan"}),
                json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"More"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"ENC"}}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"Read"}}),
                json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"a\":"}),
                json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"1}"}),
                json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","arguments":"{\"a\":1}"}}),
                json!({"type":"response.completed","response":{"usage":{}}}),
            ],
        );
        assert!(names(&out).contains(&"content_block_delta:signature_delta".to_owned()));
        let message = t.message();
        assert_eq!(message["content"][0]["thinking"], "Plan\n\nMore");
        assert_eq!(message["content"][0]["signature"], "byoc1.m.ENC");
        assert_eq!(message["content"][1]["input"]["a"], 1);
        assert_eq!(message["stop_reason"], "tool_use");
    }

    #[test]
    fn reasoning_without_summary_is_redacted_and_hidden_when_thinking_off() {
        let events = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"ENC"}}),
            json!({"type":"response.completed","response":{}}),
        ];
        let mut t = Translator::new("m", true);
        run(&mut t, &events);
        assert_eq!(t.content[0]["type"], "redacted_thinking");
        let mut t = Translator::new("m", false);
        run(&mut t, &events);
        assert!(t.content.is_empty());
    }

    #[test]
    fn interleaved_items_are_serialized() {
        let mut t = Translator::new("m", false);
        let out = run(
            &mut t,
            &[
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"a","name":"A"}}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"b","name":"B"}}),
                json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","arguments":"{}"}}),
                json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","arguments":"{}"}}),
                json!({"type":"response.completed","response":{}}),
            ],
        );
        let mut open: Option<u64> = None;
        for e in &out {
            match e.name {
                "content_block_start" => {
                    assert!(open.is_none(), "blocks interleaved");
                    open = e.data["index"].as_u64();
                }
                "content_block_delta" => assert_eq!(open, e.data["index"].as_u64()),
                "content_block_stop" => open = None,
                _ => {}
            }
        }
        assert_eq!(t.content[0]["id"], "a");
        assert_eq!(t.content[1]["id"], "b");
    }

    #[test]
    fn web_search_maps_to_server_tool_blocks() {
        let mut t = Translator::new("m", false);
        run(
            &mut t,
            &[
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws1"}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws1","action":{"query":"rust","sources":[{"type":"url","url":"https://r.example"}]}}}),
                json!({"type":"response.completed","response":{}}),
            ],
        );
        assert_eq!(t.content[0]["type"], "server_tool_use");
        assert_eq!(t.content[1]["content"][0]["url"], "https://r.example");
        assert_eq!(t.message()["stop_reason"], "end_turn");
    }

    #[test]
    fn errors_map_to_claude_semantics() {
        let mut t = Translator::new("m", false);
        let e = t
            .handle(&json!({"type":"error","error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}))
            .unwrap_err();
        assert_eq!((e.status, e.kind), (429, "rate_limit_error"));
        let e = ProviderError::from_openai("context_length_exceeded", "too big", Some(400));
        assert!(e.message.starts_with("prompt is too long"));
    }

    #[test]
    fn unknown_events_are_ignored() {
        let mut t = Translator::new("m", false);
        assert!(
            t.handle(&json!({"type":"codex.response.metadata"}))
                .unwrap()
                .is_empty()
        );
        assert!(
            t.handle(&json!({"type":"responsesapi.websocket_timing","output_index":null}))
                .unwrap()
                .is_empty()
        );
    }
}
