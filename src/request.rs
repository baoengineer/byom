//! Anthropic Messages request -> OpenAI Responses request.
//!
//! Intake is tolerant: unknown fields and block types are dropped or rendered as text so a
//! newer Claude Code never fails a request on shape alone.
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// Prefix for reasoning carried inside Claude thinking-block signatures.
pub const SIGNATURE_PREFIX: &str = "byoc1.";

pub struct Translated {
    /// Responses body without `input`.
    pub body: Map<String, Value>,
    pub input: Vec<Value>,
    /// Messages after normalization, used to detect continuation of a previous turn.
    pub messages: Vec<Value>,
    pub model: String,
    pub thinking: bool,
    pub web_search: bool,
}

pub struct Options<'a> {
    pub session: Option<&'a str>,
    /// Reasoning levels the model accepts, in increasing order; empty means pass through.
    pub effort_levels: &'a [String],
}

pub fn translate(request: &Value, options: &Options) -> Result<Translated> {
    let model = request["model"]
        .as_str()
        .filter(|m| !m.trim().is_empty())
        .context("model is required")?
        .to_owned();
    let Some(messages) = request["messages"].as_array() else {
        bail!("messages must be an array");
    };
    let messages = merge_roles(messages.iter().map(normalize_message));
    let thinking = matches!(
        request["thinking"]["type"].as_str(),
        Some("enabled" | "adaptive")
    );
    let display = request["thinking"]["display"].as_str() != Some("omitted");

    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    let instructions = system_text(&request["system"]);
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions));
    }

    let mut tools = Vec::new();
    let mut web_search = false;
    for tool in request["tools"].as_array().into_iter().flatten() {
        match tool["type"].as_str() {
            None | Some("custom") => {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                let mut parameters = tool["input_schema"].clone();
                if let Some(schema) = parameters.as_object_mut() {
                    schema.remove("$schema");
                } else {
                    parameters = json!({"type":"object","properties":{}});
                }
                tools.push(json!({
                    "type": "function",
                    "name": name,
                    "description": tool["description"].as_str().unwrap_or(""),
                    "parameters": parameters,
                    "strict": false,
                }));
            }
            Some(kind) if kind.starts_with("web_search") => web_search = true,
            Some(_) => {}
        }
    }
    let mut include = Vec::new();
    if thinking {
        include.push(json!("reasoning.encrypted_content"));
    }
    if web_search {
        body.insert("tools".into(), json!([{"type": "web_search"}]));
        include.push(json!("web_search_call.action.sources"));
    }
    if !include.is_empty() {
        body.insert("include".into(), Value::Array(include));
    }

    match request["tool_choice"]["type"].as_str() {
        Some("any") => {
            body.insert("tool_choice".into(), json!("required"));
        }
        Some("tool") => {
            if let Some(name) = request["tool_choice"]["name"].as_str() {
                body.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "name": name}),
                );
            }
        }
        Some("none") => {
            body.insert("tool_choice".into(), json!("none"));
        }
        _ => {}
    }
    let parallel = request["tool_choice"]["disable_parallel_tool_use"].as_bool() != Some(true);
    body.insert("parallel_tool_calls".into(), json!(parallel));

    let mut reasoning = Map::new();
    if let Some(effort) = request["output_config"]["effort"].as_str() {
        reasoning.insert(
            "effort".into(),
            json!(clamp_effort(effort, options.effort_levels)),
        );
    }
    if thinking && display {
        reasoning.insert("summary".into(), json!("auto"));
    }
    if !reasoning.is_empty() {
        body.insert("reasoning".into(), Value::Object(reasoning));
    }
    let format = &request["output_config"]["format"];
    if format["type"] == "json_schema" {
        body.insert(
            "text".into(),
            json!({"format": {
                "type": "json_schema",
                "name": "output",
                "schema": format["schema"],
                "strict": false,
            }}),
        );
    }
    if let Some(session) = options.session {
        body.insert("prompt_cache_key".into(), json!(session));
    }

    let mut input = Vec::new();
    if !tools.is_empty() {
        input.push(json!({"type": "additional_tools", "role": "developer", "tools": tools}));
    }
    input.extend(translate_messages(&messages, &model, thinking));
    Ok(Translated {
        body,
        input,
        messages,
        model,
        thinking,
        web_search,
    })
}

/// Claude Code encodes `{"session_id": ...}` as a JSON string in `metadata.user_id`.
pub fn metadata_session(request: &Value) -> Option<String> {
    let user: Value = serde_json::from_str(request["metadata"]["user_id"].as_str()?).ok()?;
    user["session_id"].as_str().map(str::to_owned)
}

/// Map a Claude effort level onto the nearest level the model supports.
pub fn clamp_effort<'a>(effort: &'a str, levels: &'a [String]) -> &'a str {
    const ORDER: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];
    if levels.is_empty() || levels.iter().any(|l| l == effort) {
        return effort;
    }
    let rank = |e: &str| ORDER.iter().position(|o| *o == e);
    let Some(wanted) = rank(effort) else {
        return levels.last().map(String::as_str).unwrap_or(effort);
    };
    levels
        .iter()
        .filter_map(|l| rank(l).map(|r| (r, l.as_str())))
        .min_by_key(|(r, _)| (r.abs_diff(wanted), usize::MAX - r))
        .map(|(_, l)| l)
        .unwrap_or(effort)
}

fn system_text(system: &Value) -> String {
    match system {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.starts_with("x-anthropic-billing-header"))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Merge consecutive user or assistant messages, as the Messages API treats them as one turn.
pub fn merge_roles(messages: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for message in messages {
        if let Some(last) = merged.last_mut()
            && last["role"] == message["role"]
            && message["role"] != "system"
            && let (Some(into), Some(from)) = (
                last["content"].as_array_mut(),
                message["content"].as_array(),
            )
        {
            into.extend(from.iter().cloned());
            continue;
        }
        merged.push(message);
    }
    merged
}

/// Keep only fields that carry meaning, so cache annotations do not defeat continuation.
pub fn normalize_message(message: &Value) -> Value {
    let role = message["role"].as_str().unwrap_or("user");
    let content = match &message["content"] {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(blocks) => blocks.iter().map(normalize_block).collect(),
        _ => Vec::new(),
    };
    json!({"role": role, "content": content})
}

fn normalize_block(block: &Value) -> Value {
    let mut block = block.clone();
    if let Some(object) = block.as_object_mut() {
        object.remove("cache_control");
        object.remove("citations");
        if let Some(Value::Array(inner)) = object.get_mut("content") {
            for item in inner {
                if let Some(item) = item.as_object_mut() {
                    item.remove("cache_control");
                }
            }
        }
    }
    block
}

/// Translate normalized messages. Each message translates independently, so a suffix of a
/// conversation can be sent as a continuation delta.
pub fn translate_messages(messages: &[Value], model: &str, thinking: bool) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        let blocks = message["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match message["role"].as_str() {
            Some("assistant") => assistant(blocks, model, thinking, &mut input),
            Some("system") => {
                let text = blocks_text(blocks);
                if !text.trim().is_empty() {
                    input.push(json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": text}]}));
                }
            }
            _ => user(blocks, &mut input),
        }
    }
    input
}

fn blocks_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn user(blocks: &[Value], input: &mut Vec<Value>) {
    let mut parts = Vec::new();
    let flush = |parts: &mut Vec<Value>, input: &mut Vec<Value>| {
        if !parts.is_empty() {
            input
                .push(json!({"type": "message", "role": "user", "content": std::mem::take(parts)}));
        }
    };
    for block in blocks {
        if block["type"] == "tool_result" {
            flush(&mut parts, input);
            input.push(tool_result(block));
        } else if let Some(part) = user_part(block) {
            parts.push(part);
        }
    }
    flush(&mut parts, input);
}

fn user_part(block: &Value) -> Option<Value> {
    match block["type"].as_str()? {
        "text" => {
            let text = block["text"].as_str()?;
            (!text.is_empty()).then(|| json!({"type": "input_text", "text": text}))
        }
        "image" => image(&block["source"]),
        "document" => document(block),
        "search_result" => Some(json!({"type": "input_text", "text": format!(
            "Search result: {}\n{}\n{}",
            block["title"].as_str().unwrap_or(""),
            block["source"].as_str().unwrap_or(""),
            blocks_text(block["content"].as_array().map(Vec::as_slice).unwrap_or(&[])),
        )})),
        _ => None,
    }
}

fn image(source: &Value) -> Option<Value> {
    let url = match source["type"].as_str()? {
        "base64" => format!(
            "data:{};base64,{}",
            source["media_type"].as_str()?,
            source["data"].as_str()?
        ),
        "url" => source["url"].as_str()?.to_owned(),
        _ => return None,
    };
    Some(json!({"type": "input_image", "image_url": url}))
}

fn document(block: &Value) -> Option<Value> {
    let source = &block["source"];
    let title = block["title"].as_str().unwrap_or("document");
    match source["type"].as_str()? {
        "base64" => {
            let media = source["media_type"].as_str().unwrap_or("application/pdf");
            Some(json!({
                "type": "input_file",
                "filename": if media == "application/pdf" { format!("{title}.pdf") } else { title.to_owned() },
                "file_data": format!("data:{media};base64,{}", source["data"].as_str()?),
            }))
        }
        "text" => Some(
            json!({"type": "input_text", "text": format!("{title}:\n{}", source["data"].as_str()?)}),
        ),
        "url" => Some(json!({"type": "input_file", "file_url": source["url"].as_str()?})),
        "content" => {
            Some(json!({"type": "input_text", "text": blocks_text(source["content"].as_array()?)}))
        }
        _ => None,
    }
}

fn tool_result(block: &Value) -> Value {
    let call_id = block["tool_use_id"].as_str().unwrap_or("");
    let error = block["is_error"].as_bool() == Some(true);
    let output = match &block["content"] {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(items) => {
            let parts: Vec<Value> = items.iter().filter_map(user_part).collect();
            if parts.iter().all(|p| p["type"] == "input_text") {
                Value::String(
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            } else {
                Value::Array(parts)
            }
        }
        _ => Value::String(String::new()),
    };
    let output = match (error, output) {
        (true, Value::String(text)) => Value::String(format!("Tool error: {text}")),
        (_, output) => output,
    };
    json!({"type": "function_call_output", "call_id": call_id, "output": output})
}

fn assistant(blocks: &[Value], model: &str, thinking: bool, input: &mut Vec<Value>) {
    let mut text = String::new();
    let flush = |text: &mut String, input: &mut Vec<Value>| {
        if !text.is_empty() {
            input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": std::mem::take(text)}]}));
        }
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(t) = block["text"].as_str() {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                flush(&mut text, input);
                let arguments =
                    serde_json::to_string(&block["input"]).unwrap_or_else(|_| "{}".into());
                input.push(json!({
                    "type": "function_call",
                    "call_id": block["id"],
                    "name": block["name"],
                    "arguments": arguments,
                }));
            }
            Some("thinking") | Some("redacted_thinking") if thinking => {
                let carried = block["signature"].as_str().or(block["data"].as_str());
                if let Some(encrypted) = carried.and_then(|s| decode_signature(s, model)) {
                    flush(&mut text, input);
                    let summary: Vec<Value> = block["thinking"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(|t| json!({"type": "summary_text", "text": t}))
                        .into_iter()
                        .collect();
                    input.push(json!({"type": "reasoning", "summary": summary, "encrypted_content": encrypted}));
                }
            }
            _ => {}
        }
    }
    flush(&mut text, input);
}

pub fn encode_signature(model: &str, encrypted: &str) -> String {
    format!("{SIGNATURE_PREFIX}{model}.{encrypted}")
}

/// Reasoning is replayed only to the model that produced it.
fn decode_signature<'a>(signature: &'a str, model: &str) -> Option<&'a str> {
    signature
        .strip_prefix(SIGNATURE_PREFIX)?
        .strip_prefix(model)?
        .strip_prefix('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options<'static> {
        Options {
            session: Some("s1"),
            effort_levels: &[],
        }
    }

    #[test]
    fn captured_claude_request_translates() {
        let request: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/claude-2.1.289-request.json"
        ))
        .unwrap();
        let t = translate(&request, &opts()).unwrap();
        assert_eq!(t.body["store"], false);
        assert!(
            !t.body["instructions"]
                .as_str()
                .unwrap()
                .contains("billing-header")
        );
        assert_eq!(t.input[0]["type"], "additional_tools");
        assert_eq!(t.body["prompt_cache_key"], "s1");
    }

    #[test]
    fn tolerates_unknown_fields_and_blocks() {
        let request = json!({
            "model": "m", "max_tokens": 10, "temperature": 0.2, "future_field": {"x": 1},
            "messages": [
                {"role": "user", "content": [{"type": "hologram"}, {"type": "text", "text": "hi"}]},
                {"role": "system", "content": [{"type": "text", "text": "effort changed"}]},
            ],
        });
        let t = translate(&request, &opts()).unwrap();
        assert_eq!(t.input.len(), 2);
        assert_eq!(t.input[1]["role"], "developer");
        assert!(t.body.get("temperature").is_none());
    }

    #[test]
    fn tools_images_and_results() {
        let request = json!({
            "model": "m",
            "tools": [{"name": "Read", "input_schema": {"$schema": "x", "type": "object"}}, {"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "messages": [
                {"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}, {"type": "text", "text": "look"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}, {"type": "tool_use", "id": "c1", "name": "Read", "input": {"p": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1", "is_error": true, "content": [{"type": "text", "text": "nope"}]}, {"type": "text", "text": "again"}]},
            ],
        });
        let t = translate(&request, &opts()).unwrap();
        assert_eq!(t.body["tool_choice"], "required");
        assert_eq!(t.body["parallel_tool_calls"], false);
        assert_eq!(t.body["tools"][0]["type"], "web_search");
        assert!(
            t.input[0]["tools"][0]["parameters"]
                .get("$schema")
                .is_none()
        );
        assert_eq!(
            t.input[1]["content"][0]["image_url"],
            "data:image/png;base64,AAA"
        );
        assert_eq!(t.input[3]["arguments"], "{\"p\":1}");
        assert_eq!(t.input[4]["output"], "Tool error: nope");
        assert_eq!(t.input[5]["content"][0]["text"], "again");
    }

    #[test]
    fn reasoning_round_trips_to_same_model_only() {
        let sig = encode_signature("m", "ENC");
        let request = |model: &str| {
            json!({"model": model, "thinking": {"type": "adaptive"}, "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "plan", "signature": sig}, {"type": "text", "text": "a"}]},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "x", "signature": "claude-native"}]},
            ]})
        };
        let t = translate(&request("m"), &opts()).unwrap();
        assert_eq!(t.input[1]["type"], "reasoning");
        assert_eq!(t.input[1]["encrypted_content"], "ENC");
        assert_eq!(t.input.len(), 3);
        let t = translate(&request("other"), &opts()).unwrap();
        assert!(t.input.iter().all(|i| i["type"] != "reasoning"));
    }

    #[test]
    fn effort_clamps_to_supported_levels() {
        let levels: Vec<String> = ["low", "medium", "high", "xhigh"].map(String::from).into();
        assert_eq!(clamp_effort("max", &levels), "xhigh");
        assert_eq!(clamp_effort("high", &levels), "high");
        assert_eq!(clamp_effort("max", &[]), "max");
    }

    #[test]
    fn consecutive_same_role_messages_merge() {
        let request = json!({"model": "m", "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": [{"type": "text", "text": "a"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "c", "name": "T", "input": {}}]},
            {"role": "system", "content": "s1"},
            {"role": "system", "content": "s2"},
        ]});
        let t = translate(&request, &opts()).unwrap();
        assert_eq!(t.messages.len(), 4);
        assert_eq!(t.messages[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn normalization_ignores_cache_annotations() {
        let a = normalize_message(
            &json!({"role": "user", "content": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]}),
        );
        let b = normalize_message(&json!({"role": "user", "content": "x"}));
        assert_eq!(a, b);
    }
}
