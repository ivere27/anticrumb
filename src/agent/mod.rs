pub mod approval;
pub mod backend;
pub mod cancel;
pub mod engine;
pub mod events;
pub mod parser;
pub mod process;
pub mod prompts;
pub mod tools;
pub mod transcript;

pub use approval::{
    ApprovalGuard, BatchApprovalPlan, PermissionDecision, ToolApprovalGate, ToolApprovalPolicy,
    ToolArgMatcher, ToolPermissionRule,
};
pub use backend::AgentBackend;
pub use cancel::CancelToken;
pub use engine::CodingAgent;
pub use events::{
    AgentConfig, AgentEvent, AgentRunResult, AgentStatus, AgentStepRecord, EventSink, RunControl,
    SteerInbox, ToolCallRequest, ToolOutput, DEFAULT_MAX_AGENT_STEPS, DEFAULT_SHELL_TIMEOUT_SECS,
};
pub use parser::{parse_response, parse_tool_calls, ParsedResponse};
pub use prompts::{
    init_prompt_files, inspect_status as inspect_prompt_status, load_subagent_instructions,
    LoadedPrompts, PromptContext, PromptStatus,
};
pub use tools::{
    AgentTool, BrowserControlFn, BrowserFetchFn, SpawnAgentFn, SpawnAgentOutcome, ToolContent,
    ToolContext, ToolRegistry,
};

#[cfg(test)]
mod tests;
