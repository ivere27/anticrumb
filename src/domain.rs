use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{fmt, path::PathBuf};

pub const MAX_ARTIFACT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_ARTIFACT_LABEL: &str = "100 MiB";
pub const EVENT_BROWSER_SESSION: &str = "browser_session";
pub const EVENT_SUBAGENT_BROWSER_SESSION: &str = "subagent_browser_session";
pub const EVENT_BROWSER_TURN: &str = "browser_turn";

pub fn is_valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn is_valid_artifact_name(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0', ':'])
}

pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedCapability,
    AuthRequired,
    VerificationRequired,
    RateLimited,
    UiContractChanged,
    TransportDisconnected,
    SubmissionOutcomeUnknown,
    UploadFailed,
    MediaDownloadFailed,
    ToolProtocolInvalid,
    WorkspaceConflict,
    Cancelled,
    DeadlineExceeded,
    QueueFull,
    NotFound,
    StorageFailure,
    Internal,
    SessionUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ServiceError {
    pub code: ErrorCode,
    #[serde(default)]
    pub stage: String,
    pub message: String,
    #[serde(default)]
    pub retry: String,
}

impl ServiceError {
    pub fn new(code: ErrorCode, stage: &str, message: impl Into<String>) -> Self {
        let retry = match code {
            ErrorCode::AuthRequired | ErrorCode::VerificationRequired => "after_human",
            ErrorCode::RateLimited => "after_delay",
            ErrorCode::UiContractChanged => "after_adapter_update",
            ErrorCode::SubmissionOutcomeUnknown => "after_reconcile",
            _ => "never",
        };
        Self {
            code,
            stage: stage.into(),
            message: message.into(),
            retry: retry.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, "validate", message)
    }
    pub fn session_unavailable(stage: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SessionUnavailable, stage, message)
    }
    pub fn storage(error: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::StorageFailure, "store", error.to_string())
    }
    pub fn backend(message: String, submitted: bool) -> Self {
        let m = message.to_lowercase();
        let (code, retry) = if m.contains("cancelled") {
            (ErrorCode::Cancelled, "never")
        } else if !submitted && m.starts_with("deadline exceeded") {
            // Preparation diagnostics can include verification=false; that
            // observation must not turn an explicit deadline into an auth error.
            (ErrorCode::DeadlineExceeded, "never")
        } else if m.contains("captcha") || m.contains("verification") || m.contains("robot") {
            (ErrorCode::VerificationRequired, "after_human")
        } else if m.contains("sign in") || m.contains("login") {
            (ErrorCode::AuthRequired, "after_human")
        } else if m.contains("rate limit") || m.contains("quota") || m.contains("too many requests")
        {
            (ErrorCode::RateLimited, "after_delay")
        } else if m.contains("unsupported capability") {
            (ErrorCode::UnsupportedCapability, "never")
        } else if m.contains("ui contract changed") {
            (ErrorCode::UiContractChanged, "after_adapter_update")
        } else if submitted {
            (ErrorCode::SubmissionOutcomeUnknown, "after_reconcile")
        } else if m.contains("deadline") || m.contains("timed out") {
            (ErrorCode::DeadlineExceeded, "never")
        } else {
            (ErrorCode::TransportDisconnected, "after_reconcile")
        };
        Self {
            code,
            stage: if submitted { "submit" } else { "prepare" }.into(),
            message,
            retry: retry.into(),
        }
    }
}
pub type Result<T> = std::result::Result<T, ServiceError>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Chat,
    Temporary,
}

impl SessionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Temporary => "temporary",
        }
    }

    pub const fn is_temporary(self) -> bool {
        matches!(self, Self::Temporary)
    }

    pub const fn from_temporary(temporary: bool) -> Self {
        if temporary {
            Self::Temporary
        } else {
            Self::Chat
        }
    }
}

impl fmt::Display for SessionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Ready,
    Running,
    Closed,
    Deleted,
    Invalidated,
    OutcomeUnknown,
    AwaitingHuman,
    CleanupPending,
}

impl SessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Closed => "closed",
            Self::Deleted => "deleted",
            Self::Invalidated => "invalidated",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::AwaitingHuman => "awaiting_human",
            Self::CleanupPending => "cleanup_pending",
        }
    }

    pub const fn is_inactive(self) -> bool {
        matches!(
            self,
            Self::Closed | Self::Deleted | Self::Invalidated | Self::OutcomeUnknown
        )
    }

    pub const fn is_terminated(self) -> bool {
        matches!(self, Self::Closed | Self::Deleted | Self::Invalidated)
    }

    pub const fn is_unavailable_for_temporary_resume(self) -> bool {
        matches!(
            self,
            Self::Closed | Self::Invalidated | Self::CleanupPending
        )
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRetention {
    #[default]
    Unspecified,
    Ephemeral,
    Retain,
}

impl SessionRetention {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Ephemeral => "ephemeral",
            Self::Retain => "retain",
        }
    }

    pub const fn is_unspecified(&self) -> bool {
        matches!(self, Self::Unspecified)
    }

    pub const fn should_retain_turn(self) -> bool {
        matches!(self, Self::Retain)
    }

    pub const fn should_retain_agent(self, has_workspace_lease: bool) -> bool {
        has_workspace_lease && !matches!(self, Self::Ephemeral)
    }

    pub const fn for_turn(retain: bool) -> Self {
        if retain {
            Self::Retain
        } else {
            Self::Ephemeral
        }
    }
}

impl fmt::Display for SessionRetention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    #[default]
    Function,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(default)]
    pub kind: ToolKind,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub kind: ToolKind,
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    ToolCall { call: ToolCall },
    ToolResult { call_id: String, content: String },
    Artifact { artifact_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            blocks: vec![Block::Text { text: text.into() }],
        }
    }
}

fn default_account() -> String {
    "default".into()
}
fn default_timeout() -> u64 {
    120
}

pub fn max_turn_timeout_secs(idle_timeout_secs: u64) -> u64 {
    let base = idle_timeout_secs.max(1);
    if base >= 30 {
        base.saturating_mul(15).max(1800)
    } else {
        base.saturating_mul(5)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Unspecified,
    Auto,
    None,
    Required,
    Function {
        name: String,
    },
    Unsupported(Value),
}

impl ToolChoice {
    pub fn from_value(value: Value) -> Self {
        match &value {
            Value::Null => Self::Unspecified,
            Value::String(s) => match s.as_str() {
                "auto" => Self::Auto,
                "none" => Self::None,
                "required" => Self::Required,
                _ => Self::Unsupported(value),
            },
            Value::Object(map) => {
                if let Some(name) = map
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                {
                    Self::Function {
                        name: name.to_owned(),
                    }
                } else {
                    Self::Unsupported(value)
                }
            }
            _ => Self::Unsupported(value),
        }
    }

    pub fn to_wire_value(&self) -> Value {
        match self {
            Self::Unspecified => Value::Null,
            Self::Auto => Value::String("auto".into()),
            Self::None => Value::String("none".into()),
            Self::Required => Value::String("required".into()),
            Self::Function { name } => serde_json::json!({"function": {"name": name}}),
            Self::Unsupported(value) => value.clone(),
        }
    }

    pub fn function_name(&self) -> Option<&str> {
        match self {
            Self::Function { name } => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }

    pub fn requires_declared_tools(&self) -> bool {
        matches!(self, Self::Required | Self::Function { .. })
    }
}

impl Serialize for ToolChoice {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_wire_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::from_value)
    }
}

impl fmt::Display for ToolChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_wire_value())
    }
}

impl PartialEq<&str> for ToolChoice {
    fn eq(&self, other: &&str) -> bool {
        matches!(
            (self, *other),
            (Self::Auto, "auto") | (Self::None, "none") | (Self::Required, "required")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default = "default_account")]
    pub account_id: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default = "default_parallel")]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub output_token_hint: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub mode: SessionMode,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub generate_image: bool,
    #[serde(default)]
    pub retention: SessionRetention,
    #[serde(default)]
    pub reuse_scope: Option<String>,
    #[serde(skip)]
    pub omit_tool_contract: bool,
}

fn default_parallel() -> bool {
    true
}

impl TurnRequest {
    pub fn text(prompt: impl Into<String>) -> Self {
        Self {
            session_id: None,
            account_id: default_account(),
            messages: vec![Message::user(prompt)],
            tools: vec![],
            tool_choice: ToolChoice::Unspecified,
            parallel_tool_calls: true,
            output_token_hint: None,
            model: None,
            thinking: None,
            mode: SessionMode::Chat,
            artifacts: vec![],
            timeout_secs: 120,
            idempotency_key: None,
            generate_image: false,
            retention: SessionRetention::Ephemeral,
            reuse_scope: None,
            omit_tool_contract: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub session_id: String,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub artifacts: Vec<String>,
    pub model: Option<String>,
    pub elapsed_ms: u64,
}

pub fn default_provider() -> String {
    "gemini-web".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub profile: PathBuf,
    pub subprofile: Option<String>,
    pub port: u16,
    pub headless: bool,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub provider: String,
    pub account_id: String,
    pub mode: SessionMode,
    pub target_id: Option<String>,
    pub remote_url: Option<String>,
    #[serde(default)]
    pub remote_session_id: Option<String>,
    pub history: Vec<Message>,
    #[serde(default)]
    pub state: SessionState,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub tool_digest: String,
    #[serde(default)]
    pub remote_turns: u64,
    #[serde(default = "now_ms")]
    pub last_used: i64,
    #[serde(default)]
    pub reuse_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub provider: String,
    pub state: String,
    pub request: TurnRequest,
    pub result: Option<TurnResult>,
    pub error: Option<ServiceError>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Session {
    pub fn is_inactive(&self) -> bool {
        self.state.is_inactive()
    }

    pub fn can_reopen_closed(&self) -> bool {
        self.state == SessionState::Closed
            && self.mode == SessionMode::Chat
            && (self.remote_url.is_some() || self.remote_session_id.is_some())
    }

    pub fn is_continuable(&self) -> bool {
        self.state == SessionState::Ready || self.can_reopen_closed()
    }

    pub fn has_remote_handle(&self) -> bool {
        match self.mode {
            SessionMode::Temporary => self.target_id.is_some() || self.remote_session_id.is_some(),
            SessionMode::Chat => {
                self.target_id.is_some()
                    || self.remote_url.is_some()
                    || self.remote_session_id.is_some()
            }
        }
    }
}

impl Run {
    pub fn is_active(&self) -> bool {
        matches!(
            self.state.as_str(),
            "queued" | "preparing" | "submitted" | "running"
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub run_id: String,
    pub kind: String,
    pub data: Value,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub scope: String,
    pub hash: String,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub origin: String,
    #[serde(default = "now_ms")]
    pub created_at: i64,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPermissions {
    #[serde(default = "default_true")]
    pub read: bool,
    /// Tree-sitter code navigation (`code_outline`, `find_symbol`, `find_references`); also requires `read`.
    #[serde(default = "default_true")]
    pub ast: bool,
    #[serde(default)]
    pub edit: bool,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub browser: bool,
    #[serde(default)]
    pub subagent: bool,
}

impl Default for ToolPermissions {
    fn default() -> Self {
        Self::read_only()
    }
}

impl ToolPermissions {
    pub const fn read_only() -> Self {
        Self {
            read: true,
            ast: true,
            edit: false,
            shell: false,
            browser: false,
            subagent: false,
        }
    }

    pub const fn all() -> Self {
        Self {
            read: true,
            ast: true,
            edit: true,
            shell: true,
            browser: true,
            subagent: true,
        }
    }

    pub fn summary(&self) -> String {
        let mut enabled = Vec::new();
        if self.read {
            enabled.push("read");
        }
        if self.ast {
            enabled.push("ast");
        }
        if self.edit {
            enabled.push("edit");
        }
        if self.shell {
            enabled.push("shell");
        }
        if self.browser {
            enabled.push("browser");
        }
        if self.subagent {
            enabled.push("subagent");
        }
        if enabled.is_empty() {
            "none".into()
        } else if enabled.len() == 6 {
            "all (read, ast, edit, shell, browser, subagent)".into()
        } else {
            enabled.join(", ")
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalMode {
    /// Legacy or unspecified turns; treated conservatively when resuming interactively.
    #[default]
    Unspecified,
    /// Prompt the user once per tool batch (`Y/N`) before executing tools.
    Ask,
    /// Auto-execute tools without prompting (`--yolo`).
    Auto,
}

impl ToolApprovalMode {
    pub const fn is_unspecified(&self) -> bool {
        matches!(self, Self::Unspecified)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    #[serde(skip)]
    pub environment: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub workspace: PathBuf,
    #[serde(default = "default_account")]
    pub account_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub session_mode: SessionMode,
    #[serde(default)]
    pub no_shell: bool,
    #[serde(default)]
    pub max_steps: usize,
    #[serde(default)]
    pub knowledge: Option<String>,
    #[serde(default)]
    pub mcp_config: Option<PathBuf>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub chat_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ToolPermissions>,
    #[serde(default, skip_serializing_if = "SessionRetention::is_unspecified")]
    pub session_retention: SessionRetention,
    #[serde(default, skip_serializing_if = "ToolApprovalMode::is_unspecified")]
    pub tool_approval: ToolApprovalMode,
}

impl AgentRequest {
    pub const fn is_temporary(&self) -> bool {
        self.session_mode.is_temporary()
    }
}

fn deserialize_optional_agent_error<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<ServiceError>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawError {
        Structured(ServiceError),
        Message(String),
    }

    Option::<RawError>::deserialize(deserializer).map(|opt| {
        opt.map(|raw| match raw {
            RawError::Structured(err) => err,
            RawError::Message(msg) => ServiceError::new(ErrorCode::Internal, "agent", msg),
        })
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub state: String,
    pub request: AgentRequest,
    #[serde(default)]
    pub result: Option<crate::agent::AgentRunResult>,
    #[serde(default, deserialize_with = "deserialize_optional_agent_error")]
    pub error: Option<ServiceError>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

impl AgentRecord {
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "running" | "queued")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartedAgent {
    pub id: String,
    pub conversation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub latest_agent_id: String,
    pub workspace: PathBuf,
    pub account_id: String,
    pub state: String,
    #[serde(default)]
    pub session_mode: SessionMode,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub turn_count: u32,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub chat_only: bool,
}

impl ConversationSummary {
    pub const fn is_temporary(&self) -> bool {
        self.session_mode.is_temporary()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub summary: ConversationSummary,
    pub turns: Vec<AgentRecord>,
}

impl ConversationRecord {
    /// Builds a conversation record from its turns, sorting them chronologically
    /// and deriving the summary from a single source of truth.
    pub fn from_turns(id: impl Into<String>, mut turns: Vec<AgentRecord>) -> Option<Self> {
        turns.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let first = turns.first()?;
        let latest = turns.last()?;
        let created_at = turns.iter().map(|t| t.created_at).min().unwrap_or(0);
        let updated_at = turns
            .iter()
            .map(|t| t.updated_at.max(t.created_at))
            .max()
            .unwrap_or(0);
        let session_mode = if turns.iter().any(|t| t.request.is_temporary()) {
            SessionMode::Temporary
        } else {
            SessionMode::Chat
        };
        let account_id = if latest.request.account_id.is_empty() {
            "default".into()
        } else {
            latest.request.account_id.clone()
        };
        let summary = ConversationSummary {
            id: id.into(),
            latest_agent_id: latest.id.clone(),
            workspace: latest.request.workspace.clone(),
            account_id,
            state: latest.state.clone(),
            session_mode,
            title: first.request.task.clone(),
            turn_count: u32::try_from(turns.len()).unwrap_or(u32::MAX),
            created_at,
            updated_at,
            model: latest.request.model.clone(),
            thinking: latest.request.thinking.clone(),
            chat_only: latest.request.chat_only,
        };
        Some(Self { summary, turns })
    }

    pub fn latest_turn(&self) -> Option<&AgentRecord> {
        self.turns.last()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LoginStatus {
    pub state: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelOption {
    pub index: usize,
    pub mode_id: String,
    pub label: String,
    pub sublabel: String,
    pub selected: bool,
    pub locked: bool,
    pub rate_limit_reset: Option<String>,
}

impl ModelOption {
    pub fn display_name(&self) -> &str {
        if self.sublabel.is_empty() {
            &self.label
        } else {
            &self.sublabel
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ThinkingOption {
    pub index: usize,
    pub level: String,
    pub label: String,
    pub sublabel: String,
    pub selected: bool,
    pub locked: bool,
    pub rate_limit_reset: Option<String>,
}

impl ThinkingOption {
    pub fn display_name(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelCatalogRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    pub checked_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uploads: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notebooks: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_tools: Option<bool>,
    pub models: Vec<ModelOption>,
    pub thinking: Vec<ThinkingOption>,
    pub model_thinking: std::collections::BTreeMap<String, Vec<ThinkingOption>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebUsageLimit {
    pub label: String,
    pub used: Option<String>,
    pub remaining: Option<String>,
    pub limit: Option<String>,
    pub reset: Option<String>,
    pub status: String,
    pub detail: String,
    pub used_percent: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebUsageReport {
    pub account_id: String,
    pub server_id: String,
    pub checked_at: i64,
    pub plan: Option<String>,
    pub limits: Vec<WebUsageLimit>,
    pub notice: String,
    pub updated_label: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WebHistoryCleanupState {
    pub account_id: String,
    pub state: String,
    pub deleted: u32,
    pub error: String,
    pub started_at: i64,
    pub updated_at: i64,
}

impl WebHistoryCleanupState {
    pub fn idle(account: &str) -> Self {
        Self {
            account_id: account.into(),
            state: "idle".into(),
            deleted: 0,
            error: String::new(),
            started_at: 0,
            updated_at: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "pending" | "running")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ChatCleanupSummary {
    pub pending: u32,
    pub running: u32,
    pub failed: u32,
    pub completed: u32,
    pub unmapped: u32,
    pub last_error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BrowserStatusRecord {
    pub account_id: String,
    pub account_state: String,
    pub connected: bool,
    pub endpoint: String,
    pub browser: Option<String>,
    pub profile: PathBuf,
    pub profile_matches: Option<bool>,
    pub headless: Option<bool>,
    pub owned_sessions: u64,
    pub last_verified_sign_in: Option<bool>,
    pub checked_at: Option<i64>,
    pub login: Option<LoginStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UsageSummary {
    pub queued: u64,
    pub running: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub completed: u64,
    pub submitted_chars: u64,
    pub reused_turns: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ScopedUsageSummary {
    pub provider: String,
    pub account_id: Option<String>,
    pub usage: UsageSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TurnSummaryRecord {
    pub id: String,
    pub provider: String,
    pub account_id: String,
    pub model: Option<String>,
    pub state: String,
    pub input_chars: u64,
    pub output_chars: u64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PruneSummary {
    pub expired_cache: u64,
    pub runs: u64,
    pub artifacts: u64,
    pub blob_bytes_reclaimed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CacheStats {
    pub retention_days: u32,
    pub blob_bytes: u64,
    /// Total bytes across temporary staging directories (`uploads/`, `media/`, `blobs/.tmp`),
    /// including active staging; reduced by reclaimed directories when `pruned` is true.
    pub temporary_bytes: u64,
    pub reclaimable_bytes: u64,
    pub reclaimed_bytes: u64,
    pub eligible_artifacts: u64,
    /// Count of reclaimable temporary staging directories eligible for cleanup.
    pub temporary_entries: u64,
    pub orphan_blobs: u64,
    pub expired_cache: u64,
    pub last_cleanup: i64,
    pub pruned: bool,
    pub busy: bool,
    pub pruned_events: u64,
    pub compacted_turns: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RemoteTurnRef {
    pub server_id: String,
    pub idempotency_key: String,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WebHistoryScope {
    pub started_at: i64,
    pub local_cutoff: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RuntimeSelection {
    pub model: Option<String>,
    pub thinking: Option<String>,
}
