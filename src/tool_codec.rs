//! Explicit envelopes survive Markdown renderers that discard unknown code languages.
//! Markers inside ordinary code fences are examples, never executable requests.
pub const INSTRUCTIONS: &str = "To request CLIENT tools, emit plain text markers on their own lines, with JSON between them (enclosing the JSON in a ```json code block). Do not put the [TOOL_CALL] markers themselves in a code block. Never invent tool results. Stop and wait after requests. Example format:\n[TOOL_CALL]\n```json\n{\"tool\":\"exact_name\",\"args\":{}}\n```\n[/TOOL_CALL]\n";
use crate::agent::ToolCallRequest;
use serde_json::Value;

pub(crate) fn strip_code_fence(payload: &str) -> &str {
    let trimmed = payload.trim();
    let marker_char = if trimmed.starts_with("```") {
        '`'
    } else if trimmed.starts_with("~~~") {
        '~'
    } else {
        return trimmed;
    };

    let fence_len = trimmed.chars().take_while(|&c| c == marker_char).count();
    let Some((_first_line, tail)) = trimmed.split_once('\n') else {
        return trimmed;
    };

    let tail = tail.trim_end();
    let Some((body, last_line)) = tail.rsplit_once('\n') else {
        return trimmed;
    };

    let closing_line = last_line.trim();
    if closing_line.len() >= fence_len && closing_line.chars().all(|c| c == marker_char) {
        body.trim()
    } else {
        trimmed
    }
}

pub(crate) fn toggle_fence(trimmed: &str, fence: &mut Option<char>) -> bool {
    if !(trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
        return false;
    }
    let marker = trimmed.chars().next();
    if *fence == marker {
        *fence = None;
    } else if fence.is_none() {
        *fence = marker;
    }
    true
}

/// Display partial model prose without leaking unfinished tool envelopes into the UI.
/// Ordinary code fences remain visible. Parsing for execution stays strict and separate.
pub fn visible_prose(text: &str) -> String {
    let mut output = String::new();
    let mut in_tool = false;
    let mut fence = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if in_tool {
            if trimmed == "[/TOOL_CALL]" {
                in_tool = false;
            }
            continue;
        }
        if fence.is_none() && trimmed == "[TOOL_CALL]" {
            in_tool = true;
            continue;
        }
        if fence.is_none()
            && !line.ends_with('\n')
            && ("[TOOL_CALL]".starts_with(trimmed) || "[/TOOL_CALL]".starts_with(trimmed))
        {
            break;
        }
        toggle_fence(trimmed, &mut fence);
        output.push_str(line);
    }
    output
}
fn calls(value: Value) -> Result<Vec<ToolCallRequest>, String> {
    if let Some(values) = value
        .as_array()
        .or_else(|| value.get("tool_calls").and_then(Value::as_array))
    {
        if values.is_empty() || values.len() > 128 {
            return Err("tool_calls must contain 1..128 calls".into());
        }
        let mut result = Vec::new();
        for value in values {
            result.extend(calls(value.clone())?);
        }
        return Ok(result);
    }
    let tool = value["tool"]
        .as_str()
        .ok_or("tool envelope needs exact tool and args fields")?;
    let args = value
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if tool.is_empty() || !args.is_object() {
        return Err("tool name must be nonempty and args must be an object".into());
    }
    Ok(vec![ToolCallRequest {
        tool: tool.into(),
        args,
    }])
}
pub fn parse(text: &str) -> Result<(Vec<ToolCallRequest>, String), String> {
    let mut output = String::new();
    let mut body = None::<String>;
    let mut fence = None::<char>;
    let mut result = Vec::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(payload) = body.as_mut() {
            if trimmed == "[/TOOL_CALL]" {
                let payload = body.take().unwrap();
                // A renderer or model may wrap only the JSON in a code fence; the explicit outer markers remain authoritative.
                let payload = strip_code_fence(&payload);
                result.extend(calls(
                    serde_json::from_str(payload).map_err(|e| format!("invalid tool JSON: {e}"))?,
                )?);
                if result.len() > 128 {
                    return Err("maximum 128 tool calls per response".into());
                }
            } else if trimmed == "[TOOL_CALL]" {
                return Err("nested tool envelope".into());
            } else {
                payload.push_str(line);
            }
        } else if toggle_fence(trimmed, &mut fence) {
            output.push_str(line);
        } else if fence.is_none() && trimmed == "[TOOL_CALL]" {
            body = Some(String::new());
        } else if fence.is_none() && trimmed == "[/TOOL_CALL]" {
            return Err("unexpected tool closing marker".into());
        } else {
            output.push_str(line);
        }
    }
    if body.is_some() {
        return Err("tool envelope is not closed".into());
    }
    Ok((result, output.trim().into()))
}

#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn streaming_prose_hides_partial_tool_calls_and_keeps_code_examples() {
        assert_eq!(visible_prose("Working\n[TOOL_CA"), "Working\n");
        assert_eq!(
            visible_prose("Working\n[TOOL_CALL]\n{\"tool\":\"write_file\""),
            "Working\n"
        );
        assert_eq!(
            visible_prose("Working\n[TOOL_CALL]\n{}\n[/TOOL_CALL]\nDone"),
            "Working\nDone"
        );
        let example = "```text\n[TOOL_CALL]\n{}\n[/TOOL_CALL]\n```\n";
        assert_eq!(visible_prose(example), example);
    }

    #[test]
    fn tool_calls_with_fenced_and_raw_json_are_parsed() {
        let raw = "[TOOL_CALL]\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n[/TOOL_CALL]";
        let (calls, prose) = parse(raw).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read_file");
        assert_eq!(calls[0].args["path"], "a");
        assert!(prose.is_empty());

        let fenced = "[TOOL_CALL]\n```json\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n```\n[/TOOL_CALL]";
        let (calls, _) = parse(fenced).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read_file");
        assert!(visible_prose(fenced).is_empty());

        let four_ticks = "[TOOL_CALL]\n````json\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n````\n[/TOOL_CALL]";
        let (calls, _) = parse(four_ticks).unwrap();
        assert_eq!(calls.len(), 1);

        let tildes = "[TOOL_CALL]\n~~~json\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n~~~\n[/TOOL_CALL]";
        let (calls, _) = parse(tildes).unwrap();
        assert_eq!(calls.len(), 1);

        let batch_fenced = "[TOOL_CALL]\n```json\n{\"tool_calls\": [{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}, {\"tool\":\"run_shell\",\"args\":{\"command\":\"ls\"}}]}\n```\n[/TOOL_CALL]";
        let (calls, _) = parse(batch_fenced).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool, "read_file");
        assert_eq!(calls[1].tool, "run_shell");
    }
}
