//! Wire-format normalization and terminal objects; no browser or tool execution.
use crate::{domain::*, store::Store};
use base64::Engine;
use serde_json::{json, Value};

fn role(value: &str) -> Result<Role> {
    match value {
        "system" => Ok(Role::System),
        "developer" => Ok(Role::Developer),
        "user" => Ok(Role::User),
        "assistant" | "model" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        _ => Err(ServiceError::invalid("unknown message role")),
    }
}
fn string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid(format!("missing string {key}")))
}

pub fn inline_artifact(
    store: &Store,
    account: &str,
    data: &str,
    name: &str,
    mime: Option<&str>,
) -> Result<String> {
    let (mime, encoded) = if let Some(rest) = data.strip_prefix("data:") {
        let (head, body) = rest
            .split_once(",")
            .ok_or_else(|| ServiceError::invalid("invalid data URL"))?;
        let mime = head
            .strip_suffix(";base64")
            .ok_or_else(|| ServiceError::invalid("only base64 data URLs are supported"))?;
        (mime, body)
    } else {
        (mime.unwrap_or("application/octet-stream"), data)
    };
    if encoded.len() > 44 * 1024 * 1024 {
        return Err(ServiceError::invalid("attachment too large"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ServiceError::invalid("invalid base64 attachment"))?;
    Ok(store.artifact(account, name, mime, &bytes, "input")?.id)
}

fn push_inline_artifact(
    store: &Store,
    request: &mut TurnRequest,
    data: &str,
    name: &str,
    mime: Option<&str>,
) -> Result<Block> {
    let artifact_id = inline_artifact(store, &request.account_id, data, name, mime)?;
    request.artifacts.push(artifact_id.clone());
    Ok(Block::Artifact { artifact_id })
}

fn function_tool(tool: &Value, parameters: Option<Value>) -> Result<ToolDefinition> {
    Ok(ToolDefinition {
        kind: ToolKind::Function,
        name: string(tool, "name")?,
        description: tool["description"].as_str().unwrap_or("").into(),
        parameters: parameters.unwrap_or_else(|| json!({"type":"object"})),
    })
}

fn content(value: &Value, request: &mut TurnRequest, store: &Store) -> Result<Vec<Block>> {
    if let Some(text) = value.as_str() {
        return Ok(vec![Block::Text { text: text.into() }]);
    }
    if value.is_null() {
        return Ok(vec![]);
    }
    let parts = value
        .as_array()
        .ok_or_else(|| ServiceError::invalid("content must be string or array"))?;
    let mut blocks = Vec::new();
    for part in parts {
        let block = match part["type"].as_str().unwrap_or("") {
            "text" | "input_text" | "output_text" => Block::Text {
                text: string(part, "text")?,
            },
            "image_url" | "input_image" => {
                let data = part["image_url"]["url"]
                    .as_str()
                    .or_else(|| part["image_url"].as_str())
                    .ok_or_else(|| ServiceError::invalid("image_url must be a data URL"))?;
                if !data.starts_with("data:") {
                    return Err(ServiceError::invalid("remote image URLs are disabled"));
                }
                let name = if data.starts_with("data:image/jpeg") {
                    "image.jpg"
                } else if data.starts_with("data:image/webp") {
                    "image.webp"
                } else {
                    "image.png"
                };
                push_inline_artifact(store, request, data, name, None)?
            }
            "file" | "input_file" => {
                let file = part.get("file").unwrap_or(part);
                push_inline_artifact(
                    store,
                    request,
                    &string(file, "file_data")?,
                    file["filename"].as_str().unwrap_or("document.txt"),
                    None,
                )?
            }
            "image" | "document" => {
                let source = &part["source"];
                if source["type"] != "base64" {
                    return Err(ServiceError::invalid("only base64 sources supported"));
                }
                let mime = source["media_type"].as_str();
                let name = if mime == Some("application/pdf") {
                    "document.pdf"
                } else {
                    "image.png"
                };
                push_inline_artifact(store, request, &string(source, "data")?, name, mime)?
            }
            "tool_use" => Block::ToolCall {
                call: ToolCall {
                    kind: ToolKind::Function,
                    id: string(part, "id")?,
                    name: string(part, "name")?,
                    arguments: part["input"].clone(),
                },
            },
            "tool_result" => Block::ToolResult {
                call_id: string(part, "tool_use_id")?,
                content: part["content"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| part["content"].to_string()),
            },
            other => {
                return Err(ServiceError::new(
                    ErrorCode::UnsupportedCapability,
                    "normalize",
                    format!("unsupported content block: {other}"),
                ))
            }
        };
        blocks.push(block);
    }
    Ok(blocks)
}

fn base(value: &Value, store: &Store) -> Result<TurnRequest> {
    for field in [
        "logprobs",
        "top_logprobs",
        "seed",
        "response_format",
        "temperature",
        "top_p",
        "top_k",
        "stop",
        "frequency_penalty",
        "presence_penalty",
        "logit_bias",
    ] {
        if !value[field].is_null() && value[field] != false {
            return Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "normalize",
                format!("{field} is not supported by Gemini Web"),
            ));
        }
    }
    if value["n"].as_u64().is_some_and(|n| n != 1) {
        return Err(ServiceError::invalid("only n=1 is supported"));
    }
    let mut request = TurnRequest::text("");
    request.messages.clear();
    if let Some(account) = value["account_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    {
        request.account_id = account.into();
    } else {
        request.account_id = store
            .resolve_active_account_id()
            .unwrap_or_else(|_| default_account());
    }
    request.parallel_tool_calls = value["parallel_tool_calls"].as_bool().unwrap_or(true);
    request.output_token_hint = value["max_output_tokens"]
        .as_u64()
        .or_else(|| value["max_completion_tokens"].as_u64())
        .or_else(|| value["max_tokens"].as_u64());
    request.model = value["model"].as_str().map(str::to_string);
    request.thinking = value["reasoning_effort"]
        .as_str()
        .or_else(|| value["reasoning"]["effort"].as_str())
        .map(str::to_string);
    request.tool_choice = ToolChoice::from_value(value["tool_choice"].clone());
    Ok(request)
}

pub fn chat_request(value: &Value, store: &Store) -> Result<TurnRequest> {
    let mut request = base(value, store)?;
    for message in value["messages"]
        .as_array()
        .ok_or_else(|| ServiceError::invalid("messages must be an array"))?
    {
        let r = role(&string(message, "role")?)?;
        let mut blocks = if matches!(r, Role::Tool) {
            vec![Block::ToolResult {
                call_id: string(message, "tool_call_id")?,
                content: message["content"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| message["content"].to_string()),
            }]
        } else {
            content(&message["content"], &mut request, store)?
        };
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                blocks.push(Block::ToolCall {
                    call: ToolCall {
                        kind: ToolKind::Function,
                        id: string(call, "id")?,
                        name: string(&call["function"], "name")?,
                        arguments: serde_json::from_str(&string(&call["function"], "arguments")?)
                            .map_err(|_| {
                            ServiceError::invalid("invalid tool call arguments")
                        })?,
                    },
                });
            }
        }
        request.messages.push(Message { role: r, blocks });
    }
    if let Some(tools) = value["tools"].as_array() {
        for tool in tools {
            if tool["type"] != "function" {
                return Err(ServiceError::invalid("only function tools are supported"));
            }
            let f = &tool["function"];
            request
                .tools
                .push(function_tool(f, f.get("parameters").cloned())?);
        }
    }
    Ok(request)
}

pub fn responses_request(value: &Value, store: &Store) -> Result<TurnRequest> {
    let mut request = base(value, store)?;
    if let Some(previous) = value["previous_response_id"].as_str() {
        let prior: Run = store.get("local", "run", previous)?;
        if prior.request.account_id != request.account_id || prior.state != "completed" {
            return Err(ServiceError::invalid(
                "previous response is not a completed response of this account",
            ));
        }
        // Reconstruct locally in a new owned tab, so a branch never mutates another client's tab.
        request.messages = prior
            .request
            .messages
            .into_iter()
            .filter(|m| !matches!(m.role, Role::System | Role::Developer))
            .collect();
        request.artifacts = prior.request.artifacts;
        let result = prior
            .result
            .ok_or_else(|| ServiceError::invalid("previous response has no result"))?;
        let mut blocks = vec![Block::Text { text: result.text }];
        blocks.extend(
            result
                .tool_calls
                .into_iter()
                .map(|call| Block::ToolCall { call }),
        );
        request.messages.push(Message {
            role: Role::Assistant,
            blocks,
        });
    }
    if !value["text"]["format"].is_null() && value["text"]["format"]["type"] != "text" {
        return Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "normalize",
            "structured output is not supported by Gemini Web",
        ));
    }
    if let Some(instructions) = value["instructions"].as_str() {
        request.messages.push(Message {
            role: Role::Developer,
            blocks: vec![Block::Text {
                text: instructions.into(),
            }],
        });
    }
    if let Some(text) = value["input"].as_str() {
        request.messages.push(Message::user(text));
    } else {
        for item in value["input"]
            .as_array()
            .ok_or_else(|| ServiceError::invalid("input must be string or array"))?
        {
            let message = match item["type"].as_str().unwrap_or("message") {
                "message" => Message {
                    role: role(&string(item, "role")?)?,
                    blocks: content(&item["content"], &mut request, store)?,
                },
                "custom_tool_call" => Message {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolCall {
                        call: ToolCall {
                            kind: ToolKind::Custom,
                            id: string(item, "call_id")?,
                            name: string(item, "name")?,
                            arguments: json!({"input":string(item,"input")?}),
                        },
                    }],
                },
                "function_call" => Message {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolCall {
                        call: ToolCall {
                            kind: ToolKind::Function,
                            id: string(item, "call_id")?,
                            name: string(item, "name")?,
                            arguments: serde_json::from_str(&string(item, "arguments")?)
                                .map_err(|_| ServiceError::invalid("invalid arguments"))?,
                        },
                    }],
                },
                "function_call_output" | "custom_tool_call_output" => Message {
                    role: Role::Tool,
                    blocks: vec![Block::ToolResult {
                        call_id: string(item, "call_id")?,
                        content: item["output"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| item["output"].to_string()),
                    }],
                },
                other => {
                    return Err(ServiceError::new(
                        ErrorCode::UnsupportedCapability,
                        "normalize",
                        format!("unsupported Responses input: {other}"),
                    ))
                }
            };
            request.messages.push(message);
        }
    }
    if let Some(tools) = value["tools"].as_array() {
        for tool in tools {
            if tool["type"] == "custom" {
                let description=format!("{}\nCustom text tool: put the raw input string in args.input. Format specification (best-effort; client validates): {}",tool["description"].as_str().unwrap_or(""),tool.get("format").cloned().unwrap_or(Value::Null));
                request.tools.push(ToolDefinition{kind:ToolKind::Custom,name:string(tool,"name")?,description,parameters:json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false})});
                continue;
            }
            if tool["type"] != "function" {
                return Err(ServiceError::new(
                    ErrorCode::UnsupportedCapability,
                    "normalize",
                    "unsupported Responses tool type",
                ));
            }
            request
                .tools
                .push(function_tool(tool, tool.get("parameters").cloned())?);
        }
    }
    if value["tool_choice"]["type"] == "function" || value["tool_choice"]["type"] == "custom" {
        request.tool_choice = value["tool_choice"]["name"]
            .as_str()
            .map(|name| ToolChoice::Function {
                name: name.to_owned(),
            })
            .unwrap_or_else(|| ToolChoice::Unsupported(value["tool_choice"].clone()));
    }
    Ok(request)
}

pub fn responses_result(id: &str, result: &TurnResult) -> Value {
    let mut output = Vec::new();
    if !result.text.is_empty() {
        output.push(json!({"id":format!("msg_{id}"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":result.text,"annotations":[]}]}));
    }
    for call in &result.tool_calls {
        if call.kind == ToolKind::Custom {
            output.push(json!({"id":format!("ct_{}",call.id),"type":"custom_tool_call","status":"completed","call_id":call.id,"name":call.name,"input":call.arguments["input"]}));
            continue;
        }
        output.push(json!({"id":format!("fc_{}",call.id),"type":"function_call","status":"completed","call_id":call.id,"name":call.name,"arguments":call.arguments.to_string()}));
    }
    json!({"id":id,"object":"response","created_at":now_ms()/1000,"status":"completed","error":null,"incomplete_details":null,"output":output,"model":result.model.as_deref().unwrap_or("gemini-web-default"),"usage":null})
}

pub fn messages_request(value: &Value, store: &Store) -> Result<TurnRequest> {
    let mut request = base(value, store)?;
    if !value["system"].is_null() {
        let blocks = content(&value["system"], &mut request, store)?;
        request.messages.push(Message {
            role: Role::System,
            blocks,
        });
    }
    for item in value["messages"]
        .as_array()
        .ok_or_else(|| ServiceError::invalid("messages must be an array"))?
    {
        let blocks = content(&item["content"], &mut request, store)?;
        request.messages.push(Message {
            role: role(&string(item, "role")?)?,
            blocks,
        });
    }
    if let Some(tools) = value["tools"].as_array() {
        for tool in tools {
            request
                .tools
                .push(function_tool(tool, Some(tool["input_schema"].clone()))?);
        }
    }
    request.tool_choice = match value["tool_choice"]["type"].as_str() {
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => value["tool_choice"]["name"]
            .as_str()
            .map(|name| ToolChoice::Function {
                name: name.to_owned(),
            })
            .unwrap_or_else(|| ToolChoice::Unsupported(value["tool_choice"].clone())),
        _ => ToolChoice::Auto,
    };
    Ok(request)
}
pub fn messages_result(id: &str, result: &TurnResult, input_estimate: usize) -> Value {
    let mut content = Vec::new();
    if !result.text.is_empty() {
        content.push(json!({"type":"text","text":result.text}));
    }
    for c in &result.tool_calls {
        content.push(json!({"type":"tool_use","id":c.id,"name":c.name,"input":c.arguments}));
    }
    // The Messages contract requires numbers. Explicitly marked estimates, never used as real usage.
    json!({"id":id,"type":"message","role":"assistant","model":result.model.as_deref().unwrap_or("gemini-web-default"),"content":content,
        "stop_reason":if result.tool_calls.is_empty(){"end_turn"}else{"tool_use"},"stop_sequence":null,"usage":{"input_tokens":input_estimate,"output_tokens":(result.text.len()+serde_json::to_vec(&result.tool_calls).unwrap_or_default().len()).div_ceil(3)}})
}

pub fn gemini_request(value: &Value, model: &str, store: &Store) -> Result<TurnRequest> {
    let config = &value["generationConfig"];
    let mut normalized = json!({"model":model,"account_id":value["account_id"]});
    for (from, to) in [
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("topK", "top_k"),
        ("stopSequences", "stop"),
        ("maxOutputTokens", "max_output_tokens"),
        ("seed", "seed"),
    ] {
        if let Some(v) = config.get(from) {
            normalized[to] = v.clone();
        }
    }
    for field in [
        "responseMimeType",
        "responseSchema",
        "responseJsonSchema",
        "thinkingConfig",
    ] {
        if !config[field].is_null() {
            return Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "normalize",
                format!("generationConfig.{field} is unsupported"),
            ));
        }
    }
    let mut request = base(&normalized, store)?;
    for item in value.get("systemInstruction").into_iter().chain(
        value["contents"]
            .as_array()
            .ok_or_else(|| ServiceError::invalid("contents must be an array"))?
            .iter(),
    ) {
        let mut blocks = Vec::new();
        for part in item["parts"]
            .as_array()
            .ok_or_else(|| ServiceError::invalid("parts must be an array"))?
        {
            if let Some(text) = part["text"].as_str() {
                blocks.push(Block::Text { text: text.into() });
            } else if !part["functionCall"].is_null() {
                let c = &part["functionCall"];
                blocks.push(Block::ToolCall {
                    call: ToolCall {
                        kind: ToolKind::Function,
                        id: c["id"]
                            .as_str()
                            .or_else(|| c["name"].as_str())
                            .unwrap_or("gemini-call")
                            .into(),
                        name: string(c, "name")?,
                        arguments: c["args"].clone(),
                    },
                });
            } else if !part["functionResponse"].is_null() {
                let r = &part["functionResponse"];
                blocks.push(Block::ToolResult {
                    call_id: r["id"]
                        .as_str()
                        .or_else(|| r["name"].as_str())
                        .unwrap_or("gemini-call")
                        .into(),
                    content: r["response"].to_string(),
                });
            } else if !part["inlineData"].is_null() {
                let data = &part["inlineData"];
                blocks.push(push_inline_artifact(
                    store,
                    &mut request,
                    &string(data, "data")?,
                    "image.png",
                    data["mimeType"].as_str(),
                )?);
            } else {
                return Err(ServiceError::new(
                    ErrorCode::UnsupportedCapability,
                    "normalize",
                    "unsupported Gemini part",
                ));
            }
        }
        request.messages.push(Message {
            role: if item["role"].is_null() {
                Role::System
            } else {
                role(&string(item, "role")?)?
            },
            blocks,
        });
    }
    if let Some(groups) = value["tools"].as_array() {
        for group in groups {
            for tool in group["functionDeclarations"]
                .as_array()
                .ok_or_else(|| ServiceError::invalid("only functionDeclarations supported"))?
            {
                let params = normalize_gemini_schema(
                    tool.get("parametersJsonSchema")
                        .or_else(|| tool.get("parameters"))
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object"})),
                );
                request.tools.push(function_tool(tool, Some(params))?);
            }
        }
    }
    request.tool_choice = match value["toolConfig"]["functionCallingConfig"]["mode"].as_str() {
        Some("NONE") => ToolChoice::None,
        Some("ANY") => ToolChoice::Required,
        _ => ToolChoice::Auto,
    };
    if let Some(names) =
        value["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"].as_array()
    {
        request.tools.retain(|t| names.iter().any(|n| n == &t.name));
    }
    Ok(request)
}
fn normalize_gemini_schema(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            if let Some(Value::String(kind)) = map.get_mut("type") {
                *kind = kind.to_lowercase();
            }
            map.remove("propertyOrdering");
            for v in map.values_mut() {
                *v = normalize_gemini_schema(v.take());
            }
        }
        Value::Array(values) => {
            for v in values {
                *v = normalize_gemini_schema(v.take());
            }
        }
        _ => {}
    }
    value
}
pub fn gemini_result(result: &TurnResult) -> Value {
    let mut parts = Vec::new();
    if !result.text.is_empty() {
        parts.push(json!({"text":result.text}));
    }
    for c in &result.tool_calls {
        parts.push(json!({"functionCall":{"id":c.id,"name":c.name,"args":c.arguments}}));
    }
    json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"modelVersion":result.model})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    Chat,
    Responses,
    Messages,
    Gemini,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

pub struct SseStream {
    wire: WireFormat,
    id: String,
    sequence: u64,
    emitted: String,
    opened: bool,
}

impl SseStream {
    pub fn new(wire: WireFormat, id: String) -> Self {
        Self {
            wire,
            id,
            sequence: 0,
            emitted: String::new(),
            opened: false,
        }
    }

    fn frame(&mut self, event: Option<&str>, mut value: Value) -> SseFrame {
        if event.is_some_and(|e| e.starts_with("response.")) {
            value["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
        }
        SseFrame {
            event: event.map(str::to_owned),
            data: value.to_string(),
        }
    }

    fn push_open_message(&mut self, out: &mut Vec<SseFrame>) {
        out.push(self.frame(
            Some("response.output_item.added"),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": format!("msg_{}", self.id),
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
        ));
        out.push(self.frame(
            Some("response.content_part.added"),
            json!({
                "type": "response.content_part.added",
                "item_id": format!("msg_{}", self.id),
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }),
        ));
        self.opened = true;
    }

    pub fn prelude(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if self.wire == WireFormat::Responses {
            for kind in ["response.created", "response.in_progress"] {
                let value = json!({
                    "type": kind,
                    "response": {
                        "id": &self.id,
                        "object": "response",
                        "status": "in_progress",
                        "output": []
                    }
                });
                frames.push(self.frame(Some(kind), value));
            }
        }
        frames
    }

    pub fn push_text_delta(&mut self, text: &str, has_tools: bool) -> Vec<SseFrame> {
        if has_tools || !matches!(self.wire, WireFormat::Chat | WireFormat::Responses) {
            return Vec::new();
        }
        let mut frames = Vec::new();
        if self.wire == WireFormat::Chat {
            frames.push(self.frame(
                None,
                json!({
                    "id": &self.id,
                    "object": "chat.completion.chunk",
                    "created": now_ms() / 1000,
                    "model": "gemini-web-default",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
                }),
            ));
        } else {
            if !self.opened {
                self.push_open_message(&mut frames);
            }
            frames.push(self.frame(
                Some("response.output_text.delta"),
                json!({
                    "type": "response.output_text.delta",
                    "item_id": format!("msg_{}", self.id),
                    "output_index": 0,
                    "content_index": 0,
                    "delta": text
                }),
            ));
        }
        self.emitted.push_str(text);
        frames
    }

    pub fn fail(&mut self, error: &ServiceError) -> SseFrame {
        let kind = if self.wire == WireFormat::Responses {
            "response.failed"
        } else {
            "error"
        };
        let value = if self.wire == WireFormat::Responses {
            json!({"type": kind, "response": {"id": &self.id, "status": "failed", "error": error}})
        } else {
            json!({"error": error})
        };
        self.frame(Some(kind), value)
    }

    pub fn finish(&mut self, r: &TurnResult, input_estimate: usize) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if !r.text.starts_with(&self.emitted) {
            frames.push(self.frame(
                Some("error"),
                json!({"error": {"message": "stream prefix changed"}}),
            ));
            return frames;
        }
        match self.wire {
            WireFormat::Chat => {
                let mut delta =
                    json!({"role": "assistant", "content": &r.text[self.emitted.len()..]});
                if !r.tool_calls.is_empty() {
                    delta["tool_calls"] = json!(r
                        .tool_calls
                        .iter()
                        .enumerate()
                        .map(|(i, c)| json!({
                            "index": i,
                            "id": c.id,
                            "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments.to_string()}
                        }))
                        .collect::<Vec<_>>());
                }
                frames.push(self.frame(
                    None,
                    json!({
                        "id": &self.id,
                        "object": "chat.completion.chunk",
                        "created": now_ms() / 1000,
                        "model": r.model,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                    }),
                ));
                frames.push(self.frame(
                    None,
                    json!({
                        "id": &self.id,
                        "object": "chat.completion.chunk",
                        "created": now_ms() / 1000,
                        "model": r.model,
                        "choices": [{"index": 0, "delta": {}, "finish_reason": if r.tool_calls.is_empty() { "stop" } else { "tool_calls" }}]
                    }),
                ));
                frames.push(SseFrame {
                    event: None,
                    data: "[DONE]".into(),
                });
            }
            WireFormat::Responses => {
                let response = responses_result(&self.id, r);
                let items = response["output"].as_array().unwrap();
                for (i, item) in items.iter().enumerate() {
                    if item["type"] == "message" {
                        if !self.opened {
                            self.push_open_message(&mut frames);
                        }
                        if r.text.len() > self.emitted.len() {
                            frames.push(self.frame(
                                Some("response.output_text.delta"),
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": item["id"],
                                    "output_index": i,
                                    "content_index": 0,
                                    "delta": &r.text[self.emitted.len()..]
                                }),
                            ));
                        }
                        frames.push(self.frame(
                            Some("response.output_text.done"),
                            json!({
                                "type": "response.output_text.done",
                                "item_id": item["id"],
                                "output_index": i,
                                "content_index": 0,
                                "text": r.text
                            }),
                        ));
                        frames.push(self.frame(
                            Some("response.content_part.done"),
                            json!({
                                "type": "response.content_part.done",
                                "item_id": item["id"],
                                "output_index": i,
                                "content_index": 0,
                                "part": item["content"][0]
                            }),
                        ));
                    } else {
                        let custom = item["type"] == "custom_tool_call";
                        let field = if custom { "input" } else { "arguments" };
                        let event = if custom {
                            "response.custom_tool_call_input"
                        } else {
                            "response.function_call_arguments"
                        };
                        let mut added = item.clone();
                        added["status"] = json!("in_progress");
                        added[field] = json!("");
                        frames.push(self.frame(
                            Some("response.output_item.added"),
                            json!({
                                "type": "response.output_item.added",
                                "output_index": i,
                                "item": added
                            }),
                        ));
                        frames.push(self.frame(
                            Some(&format!("{event}.delta")),
                            json!({
                                "type": format!("{event}.delta"),
                                "item_id": item["id"],
                                "output_index": i,
                                "delta": item[field]
                            }),
                        ));
                        let mut done = json!({
                            "type": format!("{event}.done"),
                            "item_id": item["id"],
                            "output_index": i
                        });
                        done[field] = item[field].clone();
                        frames.push(self.frame(Some(&format!("{event}.done")), done));
                    }
                    frames.push(self.frame(
                        Some("response.output_item.done"),
                        json!({
                            "type": "response.output_item.done",
                            "output_index": i,
                            "item": item
                        }),
                    ));
                }
                frames.push(self.frame(
                    Some("response.completed"),
                    json!({"type": "response.completed", "response": response}),
                ));
            }
            WireFormat::Messages => {
                let message = messages_result(&self.id, r, input_estimate);
                let mut start = message.clone();
                start["content"] = json!([]);
                start["stop_reason"] = Value::Null;
                frames.push(self.frame(
                    Some("message_start"),
                    json!({"type": "message_start", "message": start}),
                ));
                for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
                    let mut initial = block.clone();
                    let delta = if block["type"] == "text" {
                        initial["text"] = json!("");
                        json!({"type": "text_delta", "text": block["text"]})
                    } else {
                        initial["input"] = json!({});
                        json!({"type": "input_json_delta", "partial_json": block["input"].to_string()})
                    };
                    frames.push(self.frame(
                        Some("content_block_start"),
                        json!({"type": "content_block_start", "index": index, "content_block": initial}),
                    ));
                    frames.push(self.frame(
                        Some("content_block_delta"),
                        json!({"type": "content_block_delta", "index": index, "delta": delta}),
                    ));
                    frames.push(self.frame(
                        Some("content_block_stop"),
                        json!({"type": "content_block_stop", "index": index}),
                    ));
                }
                frames.push(self.frame(
                    Some("message_delta"),
                    json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": message["stop_reason"], "stop_sequence": null},
                        "usage": message["usage"]
                    }),
                ));
                frames.push(self.frame(Some("message_stop"), json!({"type": "message_stop"})));
            }
            WireFormat::Gemini => {
                frames.push(self.frame(None, gemini_result(r)));
            }
        }
        frames
    }
}
