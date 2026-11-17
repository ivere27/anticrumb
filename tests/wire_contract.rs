use anticrumb::{domain::*, protocol, store::Store, wire};
use serde_json::json;
#[test]
fn custom_tool_arguments_and_outputs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let request=wire::responses_request(&json!({"input":"change file","tools":[{"type":"custom","name":"apply_patch","format":{"type":"text"}}]}),&store).unwrap();
    let (text,calls)=protocol::decode("[TOOL_CALL]\n{\"tool\":\"apply_patch\",\"args\":{\"input\":\"*** Begin Patch\\n*** End Patch\"}}\n[/TOOL_CALL]",&request).unwrap();
    assert_eq!(calls[0].kind, ToolKind::Custom);
    let result = TurnResult {
        session_id: "s".into(),
        text,
        tool_calls: calls,
        artifacts: vec![],
        model: None,
        elapsed_ms: 0,
    };
    let response = wire::responses_result("r", &result);
    assert_eq!(response["output"][0]["type"], "custom_tool_call");
    let request=wire::responses_request(&json!({"input":[response["output"][0].clone(),{"type":"custom_tool_call_output","call_id":response["output"][0]["call_id"],"output":"done"}]}),&store).unwrap();
    assert_eq!(request.messages.len(), 2);
}
#[test]
fn gemini_schema_and_messages_estimates_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let request=wire::gemini_request(&json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"OBJECT","properties":{"path":{"type":"STRING"}},"required":["path"]}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY"}}}),"gemini-web-default",&store).unwrap();
    protocol::validate(&request).unwrap();
    assert_eq!(request.tool_choice, "required");
    assert_eq!(
        request.tools[0].parameters["properties"]["path"]["type"],
        "string"
    );
    let result = TurnResult {
        session_id: "s".into(),
        text: "hello".into(),
        tool_calls: vec![],
        artifacts: vec![],
        model: None,
        elapsed_ms: 0,
    };
    let message = wire::messages_result("r", &result, 10);
    assert_eq!(message["usage"]["input_tokens"], 10);
    assert!(message["usage"]["output_tokens"].as_u64().unwrap() > 0);
}
#[test]
fn malformed_or_partial_batches_never_execute_partially() {
    let mut request = TurnRequest::text("x");
    request.tools = vec![ToolDefinition {
        kind: ToolKind::Function,
        name: "read".into(),
        description: "".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }];
    for text in ["[TOOL_CALL]\n{\"tool_calls\":[{\"tool\":\"read\",\"args\":{\"path\":\"a\"}},{\"tool\":\"unknown\",\"args\":{}}]}\n[/TOOL_CALL]","[TOOL_CALL]\n{\"tool\":\"read\",\"args\":{}}\n[/TOOL_CALL]"]{assert!(protocol::decode(text,&request).is_err());}
}

#[test]
fn previous_response_replays_in_a_new_session_and_respects_account_scope() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut first = TurnRequest::text("first");
    first.messages.insert(
        0,
        Message {
            role: Role::Developer,
            blocks: vec![Block::Text {
                text: "old instructions".into(),
            }],
        },
    );
    let run = Run {
        id: "run_previous".into(),
        provider: default_provider(),
        state: "completed".into(),
        request: first,
        result: Some(TurnResult {
            session_id: "old-tab".into(),
            text: "answer".into(),
            tool_calls: vec![],
            artifacts: vec![],
            model: None,
            elapsed_ms: 1,
        }),
        error: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    store.put("local", "run", &run.id, &run).unwrap();
    let request = wire::responses_request(
        &json!({"previous_response_id":run.id,"input":"next","instructions":"new instructions"}),
        &store,
    )
    .unwrap();
    assert!(request.session_id.is_none());
    let prompt = protocol::prompt(&request).unwrap();
    assert!(prompt.contains("answer"));
    assert!(prompt.contains("new instructions"));
    assert!(!prompt.contains("old instructions"));
    assert!(wire::responses_request(
        &json!({"account_id":"other","previous_response_id":run.id,"input":"next"}),
        &store
    )
    .is_err());
    assert!(wire::chat_request(&json!({"temperature":0.1,"messages":[]}), &store).is_err());
}
