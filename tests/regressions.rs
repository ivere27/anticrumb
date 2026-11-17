use anticrumb::{domain::*, protocol};
fn parse_openai_tool_calls_from_text(
    text: &str,
    tools: &[ToolDefinition],
) -> (Vec<ToolCall>, String) {
    let mut r = TurnRequest::text("x");
    r.tools = tools.to_vec();
    protocol::decode(text, &r)
        .map(|(text, calls)| (calls, text))
        .unwrap_or((vec![], text.into()))
}
use serde_json::json;

fn specs() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        kind: ToolKind::Function,
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object","required":["path"],"additionalProperties":false,
            "properties":{"path":{"type":"string"}}}),
    }]
}

#[test]
fn tool_examples_and_unfenced_json_are_not_executable() {
    for text in [
        "Example: {\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}",
        "```json\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n```",
        "```rust\n[TOOL_CALL]\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n[/TOOL_CALL]\n```",
    ] {
        let (calls, preserved) = parse_openai_tool_calls_from_text(text, &specs());
        assert!(calls.is_empty());
        assert_eq!(preserved, text);
    }
}

#[test]
fn tool_calls_require_exact_names_complete_json_and_valid_arguments() {
    let valid = "[TOOL_CALL]\n{\"tool\":\"read_file\",\"args\":{\"path\":\"a\"}}\n[/TOOL_CALL]";
    assert_eq!(
        parse_openai_tool_calls_from_text(valid, &specs()).0.len(),
        1
    );
    for invalid in [
        valid.replace("read_file", "READ_FILE"),
        valid.replace("\"path\":\"a\"", "\"path\":42"),
        valid.replace("\"path\":\"a\"", "\"other\":\"a\""),
        valid.trim_end_matches("[/TOOL_CALL]").into(),
    ] {
        assert!(parse_openai_tool_calls_from_text(&invalid, &specs())
            .0
            .is_empty());
    }
}

#[test]
fn external_profile_is_not_managed() {
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("Chrome");
    std::fs::create_dir_all(profile.join("Profile 1")).unwrap();
    std::fs::write(profile.join("Profile 1/Preferences"), "keep").unwrap();
    assert!(anticrumb::browser_host::validate_managed_profile(&profile).is_err());
    assert_eq!(
        std::fs::read_to_string(profile.join("Profile 1/Preferences")).unwrap(),
        "keep"
    );
}
