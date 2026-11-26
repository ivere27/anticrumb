use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::approval::{ToolApprovalGate, ToolApprovalPolicy};
use super::cancel::CancelToken;

pub const DEFAULT_MAX_AGENT_STEPS: usize = usize::MAX;
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallRequest {
    pub tool: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolOutput {
    pub tool: String,
    pub ok: bool,
    pub output: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The model produced a final answer without further tool calls.
    #[default]
    Completed,
    /// The step budget ran out before the model finished.
    MaxSteps,
    /// The run was cancelled by the user / supervisor.
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentStepRecord {
    pub step: usize,
    pub model_response: String,
    pub tool_calls: Vec<ToolCallRequest>,
    pub observations: Vec<ToolOutput>,
    /// Harness-side remarks for this step (malformed tool blocks, repeated calls, steering ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentRunResult {
    pub task: String,
    pub status: AgentStatus,
    pub final_response: String,
    pub steps: Vec<AgentStepRecord>,
    /// Workspace-relative paths created/modified through `write_file` / `edit_file`.
    pub files_changed: Vec<String>,
    pub temp_chat: bool,
    pub elapsed_ms: u128,
    /// True once heavy step payloads have been compacted by SQLite retention.
    #[serde(default)]
    pub inspector_pruned: bool,
}

/// Progress notifications emitted while an agent runs. Consumers (CLI renderer, job log,
/// NDJSON output) subscribe through [`EventSink`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Durable ownership links used when deleting this saved agent conversation.
    BrowserTurn {
        run_id: String,
    },
    BrowserSession {
        session_id: String,
    },
    BrowserStage {
        stage: String,
        detail: String,
    },
    RequestPayload {
        request: Value,
        prompt: String,
    },
    ModelInput {
        chars: usize,
    },
    ModelInfo {
        model: String,
    },
    Started {
        task: String,
        workspace: String,
        max_steps: usize,
        temp_chat: bool,
    },
    StepStarted {
        step: usize,
    },
    /// Incremental model output (only emitted by backends that stream).
    ModelChunk {
        step: usize,
        text: String,
    },
    ModelResponse {
        step: usize,
        text: String,
    },
    ToolApprovalRequested {
        id: String,
        step: usize,
        calls: Vec<ToolCallRequest>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        call_indices: Vec<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    ToolApprovalResolved {
        id: String,
        step: usize,
        approved: bool,
    },
    ToolStarted {
        step: usize,
        index: usize,
        tool: String,
        args: Value,
    },
    /// Live incremental stdout/stderr chunk from a running tool (RAM-only, never persisted to SQLite).
    ToolOutputChunk {
        step: usize,
        index: usize,
        tool: String,
        stream: OutputStream,
        text: String,
    },
    ToolFinished {
        step: usize,
        index: usize,
        tool: String,
        ok: bool,
        output: String,
        elapsed_ms: u64,
    },
    /// A worker sub-agent was spawned via `spawn_agent`.
    SubagentStarted {
        id: String,
        label: String,
        task: String,
    },
    /// Live step or tool update emitted by an active worker sub-agent.
    SubagentProgress {
        id: String,
        label: String,
        phase: String,
        detail: String,
    },
    /// A worker sub-agent completed or failed.
    SubagentFinished {
        id: String,
        label: String,
        ok: bool,
        elapsed_ms: u64,
    },
    /// A user message was injected into a running agent.
    Steered {
        message: String,
    },
    Retrying {
        step: usize,
        attempt: u32,
        error: String,
    },
    Warning {
        message: String,
    },
    Finished {
        result: AgentRunResult,
    },
    Failed {
        error: String,
    },
}

impl AgentEvent {
    /// High-frequency or large transient events delivered exclusively via the in-memory
    /// `LiveEventLog` + `WatchAgentEvents` stream (0 SQLite INSERTs).
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            Self::ToolOutputChunk { .. }
                | Self::ModelChunk { .. }
                | Self::BrowserStage { .. }
                | Self::SubagentProgress { .. }
                | Self::RequestPayload { .. }
                | Self::ModelInput { .. }
        )
    }
}

type SinkFn = dyn Fn(AgentEvent) + Send + Sync;

/// Thread-safe event callback. The default sink discards everything.
#[derive(Clone, Default)]
pub struct EventSink(Option<Arc<SinkFn>>);

impl EventSink {
    pub fn new(f: impl Fn(AgentEvent) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    pub fn emit(&self, ev: AgentEvent) {
        if let Some(f) = &self.0 {
            f(ev);
        }
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "EventSink(active)"
        } else {
            "EventSink(none)"
        })
    }
}

/// Mailbox used to inject user messages into an agent that is already running.
#[derive(Debug, Clone, Default)]
pub struct SteerInbox(Arc<Mutex<VecDeque<Steering>>>);

#[derive(Debug, Clone)]
pub struct Steering {
    pub message: String,
    pub artifacts: Vec<String>,
}

impl SteerInbox {
    pub fn push(&self, msg: impl Into<String>) {
        self.push_with_artifacts(msg.into(), Vec::new());
    }

    pub fn push_with_artifacts(&self, message: String, artifacts: Vec<String>) {
        if let Ok(mut q) = self.0.lock() {
            q.push_back(Steering { message, artifacts });
        }
    }

    pub fn drain(&self) -> Vec<Steering> {
        self.0
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().map(|q| q.is_empty()).unwrap_or(true)
    }
}

/// Everything a supervisor needs to observe and steer one agent run.
#[derive(Debug, Clone, Default)]
pub struct RunControl {
    pub cancel: CancelToken,
    pub inbox: SteerInbox,
    pub approvals: ToolApprovalGate,
    pub events: EventSink,
}

impl RunControl {
    pub fn with_events(events: EventSink) -> Self {
        Self {
            events,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub environment: Option<std::collections::BTreeMap<String, String>>,
    pub workspace_root: std::path::PathBuf,
    pub max_steps: usize,
    pub temp_chat: bool,
    pub allow_shell: bool,
    pub shell_timeout: Duration,
    /// Upper bound of tool calls executed from a single model reply (the rest is reported back).
    pub max_tool_calls_per_step: usize,
    /// Extra attempts for a failed model step (CDP hiccups, tab crashes, timeouts).
    pub step_retries: u32,
    /// Prompt budget (chars) for stateless temp-chat transcript replay only.
    /// Persistent sessions send deltas and never roll over based on this budget.
    pub context_budget_chars: usize,
    /// Optional override directory for `system.md` / `tools.toml` / `tools/*.md` / `subagent.md`.
    /// When `None`, reads `<ANTICRUMB_HOME>/prompts` (`~/.anticrumb/prompts`) at run start.
    pub prompts_dir: Option<std::path::PathBuf>,
    /// Whether to inject global and workspace `AGENTS.md` instruction files into the initial prompt.
    pub load_instructions: bool,
    /// Authorization policy applied to each step's tool batch before execution.
    pub approval_policy: ToolApprovalPolicy,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            environment: None,
            workspace_root: std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
            max_steps: DEFAULT_MAX_AGENT_STEPS,
            temp_chat: false,
            allow_shell: true,
            shell_timeout: Duration::from_secs(DEFAULT_SHELL_TIMEOUT_SECS),
            max_tool_calls_per_step: 8,
            step_retries: 2,
            context_budget_chars: 60_000,
            prompts_dir: None,
            load_instructions: true,
            approval_policy: ToolApprovalPolicy::auto(),
        }
    }
}
