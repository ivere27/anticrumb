use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{fmt, path::PathBuf};

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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Chat,
    Temporary,
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
    pub keep_session: bool,
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
            keep_session: false,
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
    pub state: String,
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
        matches!(
            self.state.as_str(),
            "closed" | "deleted" | "invalidated" | "outcome_unknown"
        )
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
    pub temporary: bool,
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
    pub parent_id: Option<String>,
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
    pub temporary_bytes: u64,
    pub reclaimable_bytes: u64,
    pub reclaimed_bytes: u64,
    pub eligible_artifacts: u64,
    pub temporary_entries: u64,
    pub orphan_blobs: u64,
    pub expired_cache: u64,
    pub last_cleanup: i64,
    pub pruned: bool,
    pub busy: bool,
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
