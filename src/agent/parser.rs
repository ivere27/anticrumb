use super::events::ToolCallRequest;
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedResponse {
    pub calls: Vec<ToolCallRequest>,
    pub malformed: Vec<String>,
}
pub fn parse_response(response: &str) -> ParsedResponse {
    match crate::tool_codec::parse(response) {
        Ok((calls, _)) => ParsedResponse {
            calls,
            malformed: vec![],
        },
        Err(error) => ParsedResponse {
            calls: vec![],
            malformed: vec![error],
        },
    }
}
pub fn parse_tool_calls(response: &str) -> Vec<ToolCallRequest> {
    parse_response(response).calls
}
