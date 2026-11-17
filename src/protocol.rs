//! Shared prompt/tool codec. Wire protocols convert into domain objects first.
use crate::domain::*;

pub fn validate(request: &TurnRequest) -> Result<()> {
    if request.messages.is_empty() {
        return Err(ServiceError::invalid("messages must not be empty"));
    }
    if request.reuse_scope.as_ref().is_some_and(|s| {
        s.is_empty()
            || s.len() > 128
            || !s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        return Err(ServiceError::invalid("invalid reuse scope"));
    }
    if request.timeout_secs == 0 || request.timeout_secs > 900 {
        return Err(ServiceError::invalid("timeout_secs must be 1..900"));
    }
    if request.tools.len() > 128 || request.artifacts.len() > 10 {
        return Err(ServiceError::invalid("too many tools or artifacts"));
    }
    if !request.tool_choice.is_supported() {
        return Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "validate",
            "unsupported tool_choice",
        ));
    }
    if request.tools.is_empty() && request.tool_choice.requires_declared_tools() {
        return Err(ServiceError::invalid("tool_choice requires declared tools"));
    }
    let mut names = std::collections::HashSet::new();
    for tool in &request.tools {
        if tool.name.is_empty() || tool.name.len() > 128 || !names.insert(tool.name.as_str()) {
            return Err(ServiceError::invalid("invalid or duplicate tool name"));
        }
        // schema::validate also rejects external $ref pointers in the same traversal.
        crate::schema::validate(&tool.parameters)
            .map_err(|e| ServiceError::invalid(format!("invalid tool schema: {e}")))?;
    }
    if serde_json::to_vec(request)
        .map_err(ServiceError::storage)?
        .len()
        > 1024 * 1024
    {
        return Err(ServiceError::invalid(
            "request exceeds 1 MiB; use artifacts for bulk data",
        ));
    }
    if let Some(name) = request.tool_choice.function_name() {
        if !names.contains(name) {
            return Err(ServiceError::invalid(
                "tool_choice names an undeclared tool",
            ));
        }
    }
    if request.output_token_hint == Some(0) {
        return Err(ServiceError::invalid("output token hint must be positive"));
    }
    Ok(())
}

pub fn prompt(request: &TurnRequest) -> Result<String> {
    validate(request)?;
    let mut output = String::new();
    if request.generate_image {
        output.push_str("Generate or edit an image for the following request. Return the generated image, not instructions for drawing it.\n");
    }
    if let Some(limit) = request.output_token_hint {
        output.push_str(&format!("Keep the response within approximately {limit} tokens. This is a response budget hint.\n"));
    }
    if !request.parallel_tool_calls {
        output.push_str("Request at most one client tool per response.\n");
    }
    if !request.tools.is_empty() && !request.omit_tool_contract {
        output.push_str(crate::tool_codec::INSTRUCTIONS);
        output.push_str(&format!(
            "Tool choice constraint: {}\nTool definitions: {}\n",
            request.tool_choice,
            serde_json::to_string(&request.tools).map_err(ServiceError::storage)?
        ));
    }
    for message in &request.messages {
        output.push_str(&format!("\n[{:?}]\n", message.role));
        for block in &message.blocks {
            match block {
                Block::Text { text } => output.push_str(text),
                Block::ToolCall { call } => output.push_str(&format!(
                    "[previous tool call] {}",
                    serde_json::to_string(call).map_err(ServiceError::storage)?
                )),
                Block::ToolResult { call_id, content } => {
                    output.push_str(&format!("[TOOL_RESULT id={call_id}]\n{content}"))
                }
                Block::Artifact { artifact_id } => {
                    output.push_str(&format!("[attachment {artifact_id}]"))
                }
            }
            output.push('\n');
        }
    }
    if output.chars().count() > 100_000 {
        return Err(ServiceError::invalid(
            "context budget exceeded; compact history or read smaller ranges",
        ));
    }
    Ok(output)
}

pub fn decode(text: &str, request: &TurnRequest) -> Result<(String, Vec<ToolCall>)> {
    if request.tools.is_empty() {
        return Ok((text.to_owned(), vec![]));
    }
    let (raw, clean) = crate::tool_codec::parse(text)
        .map_err(|e| ServiceError::new(ErrorCode::ToolProtocolInvalid, "decode", e))?;
    let mut calls = Vec::new();
    for call in raw {
        let spec = request
            .tools
            .iter()
            .find(|s| s.name == call.tool)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::ToolProtocolInvalid,
                    "decode",
                    format!("undeclared tool: {}", call.tool),
                )
            })?;
        let valid =
            crate::schema::is_valid(&spec.parameters, &call.args).map_err(ServiceError::storage)?;
        if !valid {
            return Err(ServiceError::new(
                ErrorCode::ToolProtocolInvalid,
                "decode",
                format!("arguments do not match schema: {}", call.tool),
            ));
        }
        calls.push(ToolCall {
            kind: spec.kind,
            id: id("call"),
            name: call.tool,
            arguments: call.args,
        });
    }
    let invalid = (!request.parallel_tool_calls && calls.len() > 1)
        || match &request.tool_choice {
            ToolChoice::None => !calls.is_empty(),
            ToolChoice::Required => calls.is_empty(),
            ToolChoice::Function { name } => {
                calls.is_empty() || calls.iter().any(|c| c.name != *name)
            }
            ToolChoice::Unspecified | ToolChoice::Auto => false,
            ToolChoice::Unsupported(_) => true,
        };
    if invalid {
        return Err(ServiceError::new(
            ErrorCode::ToolProtocolInvalid,
            "decode",
            "tool_choice constraint was not met",
        ));
    }
    Ok((clean, calls))
}

pub fn openai_result(run_id: &str, result: &TurnResult) -> serde_json::Value {
    let calls:Vec<_>=result.tool_calls.iter().map(|c|serde_json::json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments.to_string()}})).collect();
    let mut message = serde_json::json!({"role":"assistant","content":result.text});
    if !calls.is_empty() {
        message["tool_calls"] = serde_json::json!(calls);
    }
    serde_json::json!({"id":run_id,"object":"chat.completion","created":now_ms()/1000,
        "model":result.model.as_deref().unwrap_or("gemini-web-default"),"choices":[{"index":0,"message":message,
        "finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":null})
}
