use super::pb;
use crate::{
    agent::{AgentRunResult, AgentStatus, AgentStepRecord, ToolCallRequest, ToolOutput},
    domain::{self as d, Result, ServiceError},
    provider::{ProviderCapabilities, ProviderInfo, ProviderManifest},
};
use prost::Message as _;
use serde_json::{json, Value};
use std::path::PathBuf;

fn json_value(raw: &str) -> Result<Value> {
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(raw).map_err(|e| ServiceError::invalid(format!("invalid JSON field: {e}")))
}
fn parse_tool_kind(raw: &str) -> Result<d::ToolKind> {
    match raw {
        "function" => Ok(d::ToolKind::Function),
        "custom" => Ok(d::ToolKind::Custom),
        _ => Err(ServiceError::invalid(format!("invalid tool kind: {raw}"))),
    }
}
fn tool_kind_str(kind: d::ToolKind) -> String {
    match kind {
        d::ToolKind::Function => "function",
        d::ToolKind::Custom => "custom",
    }
    .into()
}
fn parse_role(raw: &str) -> Result<d::Role> {
    match raw {
        "system" => Ok(d::Role::System),
        "developer" => Ok(d::Role::Developer),
        "user" => Ok(d::Role::User),
        "assistant" => Ok(d::Role::Assistant),
        "tool" => Ok(d::Role::Tool),
        _ => Err(ServiceError::invalid(format!("invalid role: {raw}"))),
    }
}
fn role_str(role: d::Role) -> String {
    match role {
        d::Role::System => "system",
        d::Role::Developer => "developer",
        d::Role::User => "user",
        d::Role::Assistant => "assistant",
        d::Role::Tool => "tool",
    }
    .into()
}
fn parse_mode(raw: &str) -> Result<d::SessionMode> {
    match raw {
        "chat" => Ok(d::SessionMode::Chat),
        "temporary" => Ok(d::SessionMode::Temporary),
        _ => Err(ServiceError::invalid(format!(
            "invalid session mode: {raw}"
        ))),
    }
}
fn mode_str(mode: d::SessionMode) -> String {
    match mode {
        d::SessionMode::Chat => "chat",
        d::SessionMode::Temporary => "temporary",
    }
    .into()
}
fn agent_status_str(status: AgentStatus) -> String {
    match status {
        AgentStatus::Completed => "completed",
        AgentStatus::MaxSteps => "max_steps",
        AgentStatus::Cancelled => "cancelled",
    }
    .into()
}
fn parse_agent_status(raw: &str) -> Result<AgentStatus> {
    match raw {
        "completed" => Ok(AgentStatus::Completed),
        "max_steps" => Ok(AgentStatus::MaxSteps),
        "cancelled" => Ok(AgentStatus::Cancelled),
        _ => Err(ServiceError::invalid(format!(
            "invalid agent status: {raw}"
        ))),
    }
}
fn call_from(value: pb::ToolCall) -> Result<d::ToolCall> {
    Ok(d::ToolCall {
        kind: parse_tool_kind(&value.kind)?,
        id: value.id,
        name: value.name,
        arguments: json_value(&value.arguments_json)?,
    })
}
fn call_to(value: d::ToolCall) -> pb::ToolCall {
    pb::ToolCall {
        kind: tool_kind_str(value.kind),
        id: value.id,
        name: value.name,
        arguments_json: value.arguments.to_string(),
    }
}
pub fn request_from(value: pb::TurnRequest) -> Result<d::TurnRequest> {
    let messages = value
        .messages
        .into_iter()
        .map(|m| {
            let blocks = m
                .blocks
                .into_iter()
                .map(|b| {
                    Ok(
                        match b
                            .value
                            .ok_or_else(|| ServiceError::invalid("empty content block"))?
                        {
                            pb::content_block::Value::Text(text) => d::Block::Text { text },
                            pb::content_block::Value::ToolCall(call) => d::Block::ToolCall {
                                call: call_from(call)?,
                            },
                            pb::content_block::Value::ToolResult(r) => d::Block::ToolResult {
                                call_id: r.call_id,
                                content: r.content,
                            },
                            pb::content_block::Value::ArtifactId(artifact_id) => {
                                d::Block::Artifact { artifact_id }
                            }
                        },
                    )
                })
                .collect::<Result<_>>()?;
            Ok(d::Message {
                role: parse_role(&m.role)?,
                blocks,
            })
        })
        .collect::<Result<_>>()?;
    Ok(d::TurnRequest {
        account_id: value.account_id,
        session_id: value.session_id,
        messages,
        tools: value
            .tools
            .into_iter()
            .map(|t| {
                Ok(d::ToolDefinition {
                    kind: parse_tool_kind(&t.kind)?,
                    name: t.name,
                    description: t.description,
                    parameters: json_value(&t.parameters_json)?,
                })
            })
            .collect::<Result<_>>()?,
        tool_choice: d::ToolChoice::from_value(json_value(&value.tool_choice_json)?),
        parallel_tool_calls: value.parallel_tool_calls,
        output_token_hint: value.output_token_hint,
        model: value.model,
        thinking: value.thinking,
        mode: parse_mode(&value.mode)?,
        artifacts: value.artifacts,
        timeout_secs: value.timeout_secs,
        idempotency_key: value.idempotency_key,
        generate_image: value.generate_image,
        keep_session: value.keep_session,
        reuse_scope: value.reuse_scope,
        omit_tool_contract: false,
    })
}
pub fn request_to(value: d::TurnRequest) -> pb::TurnRequest {
    pb::TurnRequest {
        account_id: value.account_id,
        session_id: value.session_id,
        messages: value
            .messages
            .into_iter()
            .map(|m| pb::Message {
                role: role_str(m.role),
                blocks: m
                    .blocks
                    .into_iter()
                    .map(|b| pb::ContentBlock {
                        value: Some(match b {
                            d::Block::Text { text } => pb::content_block::Value::Text(text),
                            d::Block::ToolCall { call } => {
                                pb::content_block::Value::ToolCall(call_to(call))
                            }
                            d::Block::ToolResult { call_id, content } => {
                                pb::content_block::Value::ToolResult(pb::ToolResult {
                                    call_id,
                                    content,
                                })
                            }
                            d::Block::Artifact { artifact_id } => {
                                pb::content_block::Value::ArtifactId(artifact_id)
                            }
                        }),
                    })
                    .collect(),
            })
            .collect(),
        tools: value
            .tools
            .into_iter()
            .map(|t| pb::ToolDefinition {
                kind: tool_kind_str(t.kind),
                name: t.name,
                description: t.description,
                parameters_json: t.parameters.to_string(),
            })
            .collect(),
        tool_choice_json: value.tool_choice.to_string(),
        parallel_tool_calls: value.parallel_tool_calls,
        output_token_hint: value.output_token_hint,
        model: value.model,
        thinking: value.thinking,
        mode: mode_str(value.mode),
        artifacts: value.artifacts,
        timeout_secs: value.timeout_secs,
        idempotency_key: value.idempotency_key,
        generate_image: value.generate_image,
        keep_session: value.keep_session,
        reuse_scope: value.reuse_scope,
    }
}
pub fn result_to(r: d::TurnResult) -> pb::TurnResult {
    pb::TurnResult {
        session_id: r.session_id,
        text: r.text,
        tool_calls: r.tool_calls.into_iter().map(call_to).collect(),
        artifacts: r.artifacts,
        model: r.model,
        elapsed_ms: r.elapsed_ms,
    }
}
pub fn result_from(r: pb::TurnResult) -> Result<d::TurnResult> {
    Ok(d::TurnResult {
        session_id: r.session_id,
        text: r.text,
        tool_calls: r
            .tool_calls
            .into_iter()
            .map(call_from)
            .collect::<Result<_>>()?,
        artifacts: r.artifacts,
        model: r.model,
        elapsed_ms: r.elapsed_ms,
    })
}
pub fn turn_to(r: d::Run) -> pb::Turn {
    pb::Turn {
        id: r.id,
        state: r.state,
        request: Some(request_to(r.request)),
        result: r.result.map(result_to),
        error: r.error.map(error_to),
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}
pub fn turn_from(r: pb::Turn) -> Result<d::Run> {
    let request = request_from(
        r.request
            .ok_or_else(|| ServiceError::invalid("missing turn request"))?,
    )?;
    Ok(d::Run {
        id: r.id,
        provider: d::default_provider(),
        state: r.state,
        request,
        result: r.result.map(result_from).transpose()?,
        error: r.error.map(error_from),
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}
pub fn event_to(e: d::Event) -> pb::Event {
    pb::Event {
        sequence: e.sequence,
        run_id: e.run_id,
        kind: e.kind,
        data_json: e.data.to_string(),
        at: e.at,
    }
}
pub fn event_from(e: pb::Event) -> Result<d::Event> {
    Ok(d::Event {
        sequence: e.sequence,
        run_id: e.run_id,
        kind: e.kind,
        data: json_value(&e.data_json)?,
        at: e.at,
    })
}
fn model_option_to(m: d::ModelOption) -> pb::ModelOption {
    pb::ModelOption {
        mode_id: m.mode_id,
        label: m.label,
        sublabel: m.sublabel,
        selected: m.selected,
        locked: m.locked,
        rate_limit_reset: m.rate_limit_reset,
        index: u32::try_from(m.index).unwrap_or(u32::MAX),
    }
}
fn model_option_from(m: pb::ModelOption) -> d::ModelOption {
    d::ModelOption {
        index: m.index as usize,
        mode_id: m.mode_id,
        label: m.label,
        sublabel: m.sublabel,
        selected: m.selected,
        locked: m.locked,
        rate_limit_reset: m.rate_limit_reset,
    }
}
fn thinking_option_to(t: d::ThinkingOption) -> pb::ThinkingOption {
    pb::ThinkingOption {
        level: t.level,
        label: t.label,
        sublabel: t.sublabel,
        selected: t.selected,
        locked: t.locked,
        rate_limit_reset: t.rate_limit_reset,
        index: u32::try_from(t.index).unwrap_or(u32::MAX),
    }
}
fn thinking_option_from(t: pb::ThinkingOption) -> d::ThinkingOption {
    d::ThinkingOption {
        index: t.index as usize,
        level: t.level,
        label: t.label,
        sublabel: t.sublabel,
        selected: t.selected,
        locked: t.locked,
        rate_limit_reset: t.rate_limit_reset,
    }
}
pub fn catalog_to(account: &str, server: &str, record: d::ModelCatalogRecord) -> pb::ModelCatalog {
    pb::ModelCatalog {
        account_id: account.into(),
        server_id: server.into(),
        checked_at: record.checked_at,
        models: record.models.into_iter().map(model_option_to).collect(),
        thinking: record
            .thinking
            .into_iter()
            .map(thinking_option_to)
            .collect(),
        model_thinking: record
            .model_thinking
            .into_iter()
            .map(|(id, options)| {
                (
                    id,
                    pb::ThinkingOptions {
                        options: options.into_iter().map(thinking_option_to).collect(),
                    },
                )
            })
            .collect(),
        provider: record.provider.unwrap_or_default(),
        adapter: record.adapter,
    }
}
pub fn catalog_from(c: pb::ModelCatalog) -> d::ModelCatalogRecord {
    d::ModelCatalogRecord {
        provider: (!c.provider.is_empty()).then_some(c.provider),
        adapter: c.adapter,
        server_id: Some(c.server_id),
        account_id: Some(c.account_id),
        checked_at: c.checked_at,
        models: c.models.into_iter().map(model_option_from).collect(),
        thinking: c.thinking.into_iter().map(thinking_option_from).collect(),
        model_thinking: c
            .model_thinking
            .into_iter()
            .map(|(id, v)| {
                (
                    id,
                    v.options.into_iter().map(thinking_option_from).collect(),
                )
            })
            .collect(),
        ..Default::default()
    }
}
pub fn error_to(e: ServiceError) -> pb::ErrorDetail {
    use d::ErrorCode::*;
    let (code, grpc_code) = match e.code {
        InvalidRequest => (1, 3),
        UnsupportedCapability => (2, 12),
        AuthRequired => (3, 9),
        VerificationRequired => (4, 9),
        RateLimited => (5, 8),
        UiContractChanged => (6, 9),
        TransportDisconnected => (7, 14),
        SubmissionOutcomeUnknown => (8, 10),
        UploadFailed => (9, 13),
        MediaDownloadFailed => (10, 13),
        ToolProtocolInvalid => (11, 3),
        WorkspaceConflict => (12, 9),
        Cancelled => (13, 1),
        DeadlineExceeded => (14, 4),
        QueueFull => (15, 8),
        NotFound => (16, 5),
        StorageFailure => (17, 13),
        Internal => (18, 13),
    };
    pb::ErrorDetail {
        code,
        message: e.message,
        grpc_code,
        stage: e.stage,
        retry: e.retry,
    }
}
pub fn error_from(e: pb::ErrorDetail) -> ServiceError {
    use d::ErrorCode::*;
    let code = match e.code {
        1 => InvalidRequest,
        2 => UnsupportedCapability,
        3 => AuthRequired,
        4 => VerificationRequired,
        5 => RateLimited,
        6 => UiContractChanged,
        7 => TransportDisconnected,
        8 => SubmissionOutcomeUnknown,
        9 => UploadFailed,
        10 => MediaDownloadFailed,
        11 => ToolProtocolInvalid,
        12 => WorkspaceConflict,
        13 => Cancelled,
        14 => DeadlineExceeded,
        15 => QueueFull,
        16 => NotFound,
        17 => StorageFailure,
        _ => Internal,
    };
    ServiceError {
        code,
        message: e.message,
        stage: e.stage,
        retry: e.retry,
    }
}
pub fn status(e: ServiceError) -> tonic::Status {
    let detail = error_to(e);
    tonic::Status::with_details(
        tonic::Code::from_i32(detail.grpc_code),
        detail.message.clone(),
        detail.encode_to_vec().into(),
    )
}
pub fn from_status(e: tonic::Status) -> ServiceError {
    if let Ok(detail) = pb::ErrorDetail::decode(e.details()) {
        if detail.code != 0 {
            return error_from(detail);
        }
    }
    ServiceError::new(
        match e.code() {
            tonic::Code::Cancelled => d::ErrorCode::Cancelled,
            tonic::Code::DeadlineExceeded => d::ErrorCode::DeadlineExceeded,
            tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
                d::ErrorCode::AuthRequired
            }
            _ => d::ErrorCode::TransportDisconnected,
        },
        "rpc",
        e.message(),
    )
}
pub fn from_module(e: synurang_host::RpcError) -> ServiceError {
    if let Ok(detail) = pb::ErrorDetail::decode(e.details.as_slice()) {
        if detail.code != 0 {
            return error_from(detail);
        }
    }
    ServiceError::new(d::ErrorCode::TransportDisconnected, "ffi", e.message)
}
pub fn module_error(e: ServiceError) -> synurang_call::RpcError {
    let detail = error_to(e);
    synurang_call::RpcError {
        code: detail.grpc_code,
        message: detail.message.clone(),
        details: detail.encode_to_vec(),
    }
}
pub fn account_to(a: d::Account) -> pb::Account {
    pb::Account {
        id: a.id,
        provider: a.provider,
        state: a.state,
        profile: a.profile.to_string_lossy().into_owned(),
        subprofile: a.subprofile,
        port: u32::from(a.port),
        headless: a.headless,
    }
}
pub fn account_from(a: pb::Account) -> Result<d::Account> {
    Ok(d::Account {
        id: a.id,
        provider: if a.provider.is_empty() {
            d::default_provider()
        } else {
            a.provider
        },
        profile: PathBuf::from(a.profile),
        subprofile: a.subprofile,
        port: u16::try_from(a.port).map_err(|_| ServiceError::invalid("invalid account port"))?,
        headless: a.headless,
        state: a.state,
    })
}
pub fn provider_caps_to(c: ProviderCapabilities) -> pb::ProviderCapabilities {
    pb::ProviderCapabilities {
        browser_auth: c.browser_auth,
        temporary_sessions: c.temporary_sessions,
        file_uploads: c.file_uploads,
        image_generation: c.image_generation,
        web_usage: c.web_usage,
        web_history_cleanup: c.web_history_cleanup,
        native_tools: c.native_tools,
    }
}
pub fn provider_caps_from(c: pb::ProviderCapabilities) -> ProviderCapabilities {
    ProviderCapabilities {
        browser_auth: c.browser_auth,
        temporary_sessions: c.temporary_sessions,
        file_uploads: c.file_uploads,
        image_generation: c.image_generation,
        web_usage: c.web_usage,
        web_history_cleanup: c.web_history_cleanup,
        native_tools: c.native_tools,
    }
}
pub fn provider_info_to(info: ProviderInfo) -> pb::ProviderInfo {
    pb::ProviderInfo {
        id: info.id,
        name: info.name,
        version: info.version,
        kind: info.kind,
        entry_url: info.entry_url,
        default_model: info.default_model,
        source: info.source,
        capabilities: Some(provider_caps_to(info.capabilities)),
        inline_prompt_chars: Some(info.inline_prompt_chars),
    }
}
pub fn provider_info_from(info: pb::ProviderInfo) -> ProviderInfo {
    ProviderInfo {
        id: info.id,
        name: info.name,
        version: info.version,
        kind: info.kind,
        entry_url: info.entry_url,
        default_model: info.default_model,
        source: info.source,
        capabilities: info
            .capabilities
            .map(provider_caps_from)
            .unwrap_or_default(),
        inline_prompt_chars: info
            .inline_prompt_chars
            .unwrap_or(crate::provider::DEFAULT_INLINE_PROMPT_CHARS),
    }
}
pub fn register_provider_request_to(m: ProviderManifest) -> pb::RegisterProviderRequest {
    pb::RegisterProviderRequest {
        id: m.id,
        name: m.name,
        version: m.version,
        kind: m.kind,
        entry_url: m.entry_url,
        allowed_origins: m.allowed_origins,
        default_model: m.default_model,
        scripts_dir: m.scripts_dir.map(|p| p.to_string_lossy().into_owned()),
        scripts: m.scripts.into_iter().collect(),
        capabilities: Some(provider_caps_to(m.capabilities)),
        auth_url_substrings: m.auth_url_substrings,
        chat_url_prefixes: m.chat_url_prefixes,
        reusable_path_prefixes: m.reusable_path_prefixes,
        usage_url: m.usage_url,
        observer_binding: m.observer_binding,
        requires_models_control: m.requires_models_control,
        inline_prompt_chars: m.inline_prompt_chars,
    }
}
pub fn provider_manifest_from(r: pb::RegisterProviderRequest) -> ProviderManifest {
    ProviderManifest {
        id: r.id,
        name: r.name,
        version: if r.version.is_empty() {
            "v1".into()
        } else {
            r.version
        },
        kind: if r.kind.is_empty() {
            "web_provider".into()
        } else {
            r.kind
        },
        entry_url: r.entry_url,
        allowed_origins: r.allowed_origins,
        auth_url_substrings: r.auth_url_substrings,
        chat_url_prefixes: r.chat_url_prefixes,
        reusable_path_prefixes: r.reusable_path_prefixes,
        usage_url: r.usage_url,
        observer_binding: r.observer_binding,
        requires_models_control: r.requires_models_control,
        default_model: r.default_model,
        scripts_dir: r.scripts_dir.map(PathBuf::from),
        scripts: r.scripts.into_iter().collect(),
        capabilities: r.capabilities.map(provider_caps_from).unwrap_or_default(),
        inline_prompt_chars: r.inline_prompt_chars,
    }
}
pub fn session_to(s: d::Session) -> pb::Session {
    pb::Session {
        id: s.id,
        account_id: s.account_id,
        mode: mode_str(s.mode),
        state: s.state,
        model: s.model,
        thinking: s.thinking,
        last_used: s.last_used,
        remote_turns: s.remote_turns,
    }
}
pub fn artifact_to(a: d::Artifact) -> pb::Artifact {
    pb::Artifact {
        id: a.id,
        scope: a.scope,
        hash: a.hash,
        mime: a.mime,
        name: a.name,
        size: a.size,
        origin: a.origin,
        created_at: a.created_at,
    }
}
pub fn usage_to(u: d::UsageSummary) -> pb::Usage {
    pb::Usage {
        queued: u.queued,
        running: u.running,
        input_chars: u.input_chars,
        output_chars: u.output_chars,
        completed: u.completed,
        submitted_chars: u.submitted_chars,
        reused_turns: u.reused_turns,
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
    }
}
pub fn prune_to(p: d::PruneSummary) -> pb::PruneResult {
    pb::PruneResult {
        expired_cache: p.expired_cache,
        runs: p.runs,
        artifacts: p.artifacts,
        blob_bytes_reclaimed: p.blob_bytes_reclaimed,
    }
}
pub fn chat_cleanup_to(c: d::ChatCleanupSummary) -> pb::ChatCleanupStatus {
    pb::ChatCleanupStatus {
        pending: c.pending,
        running: c.running,
        failed: c.failed,
        completed: c.completed,
        unmapped: c.unmapped,
        last_error: c.last_error,
    }
}
pub fn web_history_to(w: d::WebHistoryCleanupState) -> pb::WebHistoryCleanupStatus {
    pb::WebHistoryCleanupStatus {
        account_id: w.account_id,
        state: w.state,
        deleted: w.deleted,
        error: w.error,
        started_at: w.started_at,
        updated_at: w.updated_at,
    }
}
pub fn web_history_from(w: &pb::WebHistoryCleanupStatus) -> d::WebHistoryCleanupState {
    d::WebHistoryCleanupState {
        account_id: w.account_id.clone(),
        state: w.state.clone(),
        deleted: w.deleted,
        error: w.error.clone(),
        started_at: w.started_at,
        updated_at: w.updated_at,
    }
}
pub fn web_usage_to(w: d::WebUsageReport) -> pb::WebUsage {
    pb::WebUsage {
        account_id: w.account_id,
        server_id: w.server_id,
        checked_at: w.checked_at,
        plan: w.plan,
        limits: w
            .limits
            .into_iter()
            .map(|l| pb::WebUsageLimit {
                label: l.label,
                used: l.used,
                remaining: l.remaining,
                limit: l.limit,
                reset: l.reset,
                status: l.status,
                detail: l.detail,
                used_percent: l.used_percent,
            })
            .collect(),
        notice: w.notice,
        updated_label: w.updated_label,
        timezone: w.timezone,
    }
}
pub fn login_status_to(l: d::LoginStatus) -> pb::LoginStatus {
    pb::LoginStatus {
        state: l.state,
        error: l.error,
    }
}
pub fn login_status_from(l: pb::LoginStatus) -> d::LoginStatus {
    d::LoginStatus {
        state: l.state,
        error: l.error,
    }
}
pub fn browser_status_to(b: d::BrowserStatusRecord) -> pb::BrowserStatus {
    pb::BrowserStatus {
        account_id: b.account_id,
        account_state: b.account_state,
        connected: b.connected,
        endpoint: b.endpoint,
        browser: b.browser,
        profile: b.profile.to_string_lossy().into_owned(),
        profile_matches: b.profile_matches,
        headless: b.headless,
        owned_sessions: b.owned_sessions,
        last_verified_sign_in: b.last_verified_sign_in,
        checked_at: b.checked_at,
        login: b.login.map(login_status_to),
    }
}
pub fn turn_summary_to(t: d::TurnSummaryRecord) -> pb::TurnSummary {
    pb::TurnSummary {
        id: t.id,
        account_id: t.account_id,
        state: t.state,
        input_chars: t.input_chars,
        output_chars: t.output_chars,
        updated_at: t.updated_at,
    }
}
pub fn knowledge_excerpt_to(e: crate::knowledge::Excerpt) -> pb::KnowledgeExcerpt {
    pb::KnowledgeExcerpt {
        source: e.source,
        path: e.path,
        line: e.line as u64,
        text: e.text,
        hash: e.hash,
        citation: e.citation,
    }
}
const CHAT_ONLY_STEP_FLAG: u32 = 1 << 31;
const PERMS_PRESENT_FLAG: u32 = 1 << 30;
const PERM_READ_FLAG: u32 = 1 << 29;
const PERM_EDIT_FLAG: u32 = 1 << 28;
const PERM_SHELL_FLAG: u32 = 1 << 27;
const PERM_BROWSER_FLAG: u32 = 1 << 26;
const PERM_SUBAGENT_FLAG: u32 = 1 << 25;
const STEP_COUNT_MASK: u32 = (1 << 25) - 1;

pub fn agent_request_to(r: d::AgentRequest) -> pb::AgentRequest {
    let mut max_steps = u32::try_from(r.max_steps).unwrap_or(STEP_COUNT_MASK) & STEP_COUNT_MASK;
    if r.chat_only {
        max_steps |= CHAT_ONLY_STEP_FLAG;
    }
    if let Some(perms) = r.permissions {
        max_steps |= PERMS_PRESENT_FLAG;
        if perms.read {
            max_steps |= PERM_READ_FLAG;
        }
        if perms.edit {
            max_steps |= PERM_EDIT_FLAG;
        }
        if perms.shell {
            max_steps |= PERM_SHELL_FLAG;
        }
        if perms.browser {
            max_steps |= PERM_BROWSER_FLAG;
        }
        if perms.subagent {
            max_steps |= PERM_SUBAGENT_FLAG;
        }
    }
    pb::AgentRequest {
        environment: r.environment.unwrap_or_default().into_iter().collect(),
        task: r.task,
        workspace: r.workspace.to_string_lossy().into_owned(),
        account_id: r.account_id,
        model: r.model,
        thinking: r.thinking,
        temporary: r.temporary,
        no_shell: r.no_shell,
        max_steps,
        knowledge: r.knowledge,
        mcp_config: r.mcp_config.map(|p| p.to_string_lossy().into_owned()),
        artifacts: r.artifacts,
        parent_id: r.parent_id,
    }
}
pub fn agent_request_from(r: pb::AgentRequest) -> d::AgentRequest {
    let chat_only = (r.max_steps & CHAT_ONLY_STEP_FLAG) != 0;
    let permissions = ((r.max_steps & PERMS_PRESENT_FLAG) != 0).then(|| d::ToolPermissions {
        read: (r.max_steps & PERM_READ_FLAG) != 0,
        edit: (r.max_steps & PERM_EDIT_FLAG) != 0,
        shell: (r.max_steps & PERM_SHELL_FLAG) != 0,
        browser: (r.max_steps & PERM_BROWSER_FLAG) != 0,
        subagent: (r.max_steps & PERM_SUBAGENT_FLAG) != 0,
    });
    let max_steps = (r.max_steps & STEP_COUNT_MASK) as usize;
    d::AgentRequest {
        environment: (!r.environment.is_empty()).then(|| r.environment.into_iter().collect()),
        task: r.task,
        workspace: PathBuf::from(r.workspace),
        account_id: r.account_id,
        model: r.model,
        thinking: r.thinking,
        temporary: r.temporary,
        no_shell: r.no_shell,
        max_steps,
        knowledge: r.knowledge,
        mcp_config: r.mcp_config.map(PathBuf::from),
        artifacts: r.artifacts,
        parent_id: r.parent_id,
        chat_only,
        permissions,
    }
}
pub fn agent_to(record: d::AgentRecord) -> pb::Agent {
    let mut request = record.request;
    request.environment = None;
    pb::Agent {
        id: record.id,
        state: record.state,
        request: Some(agent_request_to(request)),
        result: record.result.map(|res| pb::AgentResult {
            status: agent_status_str(res.status),
            final_response: res.final_response,
            files_changed: res.files_changed,
            steps: res
                .steps
                .into_iter()
                .map(|step| pb::AgentStep {
                    step: u32::try_from(step.step).unwrap_or(u32::MAX),
                    model_response: step.model_response,
                    tool_calls: step
                        .tool_calls
                        .into_iter()
                        .map(|tc| pb::AgentToolCall {
                            tool: tc.tool,
                            args_json: tc.args.to_string(),
                        })
                        .collect(),
                    observations: step
                        .observations
                        .into_iter()
                        .map(|obs| pb::AgentObservation {
                            tool: obs.tool,
                            ok: obs.ok,
                            output: obs.output,
                            artifacts: obs.artifacts,
                            elapsed_ms: obs.elapsed_ms,
                        })
                        .collect(),
                    notes: step.notes,
                })
                .collect(),
            task: res.task,
            temp_chat: res.temp_chat,
            elapsed_ms: u64::try_from(res.elapsed_ms).unwrap_or(u64::MAX),
        }),
        error: record.error.map(error_to),
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}
pub fn agent_from(value: pb::Agent) -> Result<d::AgentRecord> {
    let request = agent_request_from(
        value
            .request
            .ok_or_else(|| ServiceError::invalid("missing agent request"))?,
    );
    let result = value
        .result
        .map(|res| {
            let steps = res
                .steps
                .into_iter()
                .map(|step| {
                    let tool_calls = step
                        .tool_calls
                        .into_iter()
                        .map(|tc| {
                            Ok(ToolCallRequest {
                                tool: tc.tool,
                                args: json_value(&tc.args_json)?,
                            })
                        })
                        .collect::<Result<_>>()?;
                    let observations = step
                        .observations
                        .into_iter()
                        .map(|obs| ToolOutput {
                            tool: obs.tool,
                            ok: obs.ok,
                            output: obs.output,
                            artifacts: obs.artifacts,
                            elapsed_ms: obs.elapsed_ms,
                        })
                        .collect();
                    Ok(AgentStepRecord {
                        step: step.step as usize,
                        model_response: step.model_response,
                        tool_calls,
                        observations,
                        notes: step.notes,
                    })
                })
                .collect::<Result<_>>()?;
            Ok(AgentRunResult {
                task: res.task,
                status: parse_agent_status(&res.status)?,
                final_response: res.final_response,
                steps,
                files_changed: res.files_changed,
                temp_chat: res.temp_chat,
                elapsed_ms: u128::from(res.elapsed_ms),
            })
        })
        .transpose()?;
    Ok(d::AgentRecord {
        id: value.id,
        state: value.state,
        request,
        result,
        error: value.error.map(error_from),
        created_at: value.created_at,
        updated_at: value.updated_at,
    })
}
pub fn status_from(status: pb::HostStatus) -> Value {
    let accounts: Vec<_> = status
        .accounts
        .into_iter()
        .map(|a| {
            let c = a.catalog.map(catalog_from).unwrap_or_default();
            json!({
                "id": a.id,
                "provider": a.provider,
                "state": a.state,
                "selected_model": a.selected_model,
                "models": c.models,
                "thinking": c.thinking,
                "model_thinking": c.model_thinking,
                "login": a.login
            })
        })
        .collect();
    let providers: Vec<_> = status
        .providers
        .into_iter()
        .map(provider_info_from)
        .collect();
    let usage = status.usage.unwrap_or_default();
    json!({
        "server_id": status.server_id,
        "runtime": status.runtime,
        "active_agents": status.active_agents,
        "accounts": accounts,
        "queued": usage.queued,
        "running": usage.running,
        "usage": usage,
        "providers": providers,
        "browser_connection": status.browser_connection,
        "chat_cleanup": status.chat_cleanup
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_environment_crosses_local_rpc_but_is_never_persisted_or_returned() {
        let mut request: d::AgentRequest =
            serde_json::from_value(json!({"task":"fixture"})).unwrap();
        request.environment = Some(std::collections::BTreeMap::from([(
            "PROJECT_SECRET".into(),
            "transient-fixture-value".into(),
        )]));
        let wire = agent_request_to(request);
        assert_eq!(
            wire.environment["PROJECT_SECRET"],
            "transient-fixture-value"
        );
        let request = agent_request_from(wire);
        assert!(!serde_json::to_string(&request)
            .unwrap()
            .contains("transient-fixture-value"));
        let response = agent_to(d::AgentRecord {
            id: "fixture".into(),
            state: "running".into(),
            request,
            result: None,
            error: None,
            created_at: 0,
            updated_at: 0,
        });
        assert!(response.request.unwrap().environment.is_empty());
    }

    #[test]
    fn provider_prompt_threshold_survives_wire_roundtrips_and_omitted_defaults() {
        for setting in [None, Some(0), Some(1234), Some(4000)] {
            let mut manifest: ProviderManifest = serde_json::from_value(json!({
                "id": "test-web", "name": "Test Web"
            }))
            .unwrap();
            manifest.inline_prompt_chars = setting;
            let roundtrip = provider_manifest_from(register_provider_request_to(manifest.clone()));
            assert_eq!(roundtrip, manifest);
            assert_eq!(
                roundtrip.resolved_inline_prompt_chars(),
                setting.unwrap_or(crate::provider::DEFAULT_INLINE_PROMPT_CHARS)
            );
            let wire = pb::ProviderInfo {
                inline_prompt_chars: setting,
                ..Default::default()
            };
            let info = provider_info_from(wire);
            assert_eq!(
                info.inline_prompt_chars,
                setting.unwrap_or(crate::provider::DEFAULT_INLINE_PROMPT_CHARS)
            );
            assert_eq!(provider_info_from(provider_info_to(info.clone())), info);
        }
        let mut json =
            serde_json::to_value(provider_info_from(pb::ProviderInfo::default())).unwrap();
        json.as_object_mut().unwrap().remove("inline_prompt_chars");
        assert_eq!(
            serde_json::from_value::<ProviderInfo>(json)
                .unwrap()
                .inline_prompt_chars,
            crate::provider::DEFAULT_INLINE_PROMPT_CHARS
        );
    }

    #[test]
    fn running_agent_has_no_result_and_completed_agent_preserves_tool_observations() {
        let request: d::AgentRequest =
            serde_json::from_value(json!({"task":"inspect", "workspace":"project"})).unwrap();
        let mut running: d::AgentRecord = serde_json::from_value(
            json!({"id":"agent_example","state":"running","request":request,"created_at":1}),
        )
        .unwrap();
        let encoded = agent_to(running.clone());
        assert!(encoded.result.is_none());
        assert!(agent_from(encoded).unwrap().result.is_none());
        running.state = "completed".into();
        running.result = Some(
            serde_json::from_value(json!({"status":"completed","final_response":"done","files_changed":["file.txt"],"steps":[{"step":1,"model_response":"call","tool_calls":[{"tool":"write_file","args":{"path":"file.txt","content":"data"}}],"observations":[{"tool":"write_file","ok":true,"output":"written","elapsed_ms":2}]}]})).unwrap(),
        );
        let roundtrip = agent_from(agent_to(running)).unwrap();
        let result = roundtrip.result.unwrap();
        assert_eq!(result.steps[0].tool_calls[0].args["path"], "file.txt");
        assert_eq!(result.steps[0].observations[0].output, "written");
    }
}
