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
pub fn session_mode_from(raw: i32) -> Result<d::SessionMode> {
    match pb::SessionMode::try_from(raw) {
        Ok(pb::SessionMode::Unspecified | pb::SessionMode::Chat) => Ok(d::SessionMode::Chat),
        Ok(pb::SessionMode::Temporary) => Ok(d::SessionMode::Temporary),
        Err(_) => Err(ServiceError::invalid(format!(
            "invalid session mode: {raw}"
        ))),
    }
}
pub fn session_mode_filter(raw: i32) -> Result<Option<d::SessionMode>> {
    match pb::SessionMode::try_from(raw) {
        Ok(pb::SessionMode::Unspecified) => Ok(None),
        Ok(pb::SessionMode::Chat) => Ok(Some(d::SessionMode::Chat)),
        Ok(pb::SessionMode::Temporary) => Ok(Some(d::SessionMode::Temporary)),
        Err(_) => Err(ServiceError::invalid(format!(
            "invalid session mode: {raw}"
        ))),
    }
}
pub fn session_mode_to(mode: d::SessionMode) -> i32 {
    match mode {
        d::SessionMode::Chat => pb::SessionMode::Chat as i32,
        d::SessionMode::Temporary => pb::SessionMode::Temporary as i32,
    }
}
pub fn session_state_from(raw: i32) -> Result<d::SessionState> {
    match pb::SessionState::try_from(raw) {
        Ok(pb::SessionState::Ready) => Ok(d::SessionState::Ready),
        Ok(pb::SessionState::Running) => Ok(d::SessionState::Running),
        Ok(pb::SessionState::Closed) => Ok(d::SessionState::Closed),
        Ok(pb::SessionState::Deleted) => Ok(d::SessionState::Deleted),
        Ok(pb::SessionState::Invalidated) => Ok(d::SessionState::Invalidated),
        Ok(pb::SessionState::OutcomeUnknown) => Ok(d::SessionState::OutcomeUnknown),
        Ok(pb::SessionState::AwaitingHuman) => Ok(d::SessionState::AwaitingHuman),
        Ok(pb::SessionState::CleanupPending) => Ok(d::SessionState::CleanupPending),
        Ok(pb::SessionState::Unspecified) | Err(_) => Err(ServiceError::invalid(format!(
            "invalid session state: {raw}"
        ))),
    }
}
pub fn session_state_filter(raw: i32) -> Result<Option<d::SessionState>> {
    match pb::SessionState::try_from(raw) {
        Ok(pb::SessionState::Unspecified) => Ok(None),
        _ => session_state_from(raw).map(Some),
    }
}
pub fn session_state_to(state: d::SessionState) -> i32 {
    match state {
        d::SessionState::Ready => pb::SessionState::Ready as i32,
        d::SessionState::Running => pb::SessionState::Running as i32,
        d::SessionState::Closed => pb::SessionState::Closed as i32,
        d::SessionState::Deleted => pb::SessionState::Deleted as i32,
        d::SessionState::Invalidated => pb::SessionState::Invalidated as i32,
        d::SessionState::OutcomeUnknown => pb::SessionState::OutcomeUnknown as i32,
        d::SessionState::AwaitingHuman => pb::SessionState::AwaitingHuman as i32,
        d::SessionState::CleanupPending => pb::SessionState::CleanupPending as i32,
    }
}
pub fn session_retention_from(raw: i32) -> Result<d::SessionRetention> {
    match pb::SessionRetention::try_from(raw) {
        Ok(pb::SessionRetention::Unspecified) => Ok(d::SessionRetention::Unspecified),
        Ok(pb::SessionRetention::Ephemeral) => Ok(d::SessionRetention::Ephemeral),
        Ok(pb::SessionRetention::Retain) => Ok(d::SessionRetention::Retain),
        Err(_) => Err(ServiceError::invalid(format!(
            "invalid session retention: {raw}"
        ))),
    }
}
pub fn session_retention_to(retention: d::SessionRetention) -> i32 {
    match retention {
        d::SessionRetention::Unspecified => pb::SessionRetention::Unspecified as i32,
        d::SessionRetention::Ephemeral => pb::SessionRetention::Ephemeral as i32,
        d::SessionRetention::Retain => pb::SessionRetention::Retain as i32,
    }
}
pub fn tool_permissions_to(p: d::ToolPermissions) -> pb::ToolPermissions {
    pb::ToolPermissions {
        read: p.read,
        edit: p.edit,
        shell: p.shell,
        browser: p.browser,
        subagent: p.subagent,
        ast: p.ast,
    }
}
pub fn tool_permissions_from(p: pb::ToolPermissions) -> d::ToolPermissions {
    d::ToolPermissions {
        read: p.read,
        edit: p.edit,
        shell: p.shell,
        browser: p.browser,
        subagent: p.subagent,
        ast: p.ast,
    }
}
pub fn tool_approval_mode_from(raw: i32) -> Result<d::ToolApprovalMode> {
    match pb::ToolApprovalMode::try_from(raw) {
        Ok(pb::ToolApprovalMode::Unspecified) => Ok(d::ToolApprovalMode::Unspecified),
        Ok(pb::ToolApprovalMode::Ask) => Ok(d::ToolApprovalMode::Ask),
        Ok(pb::ToolApprovalMode::Auto) => Ok(d::ToolApprovalMode::Auto),
        Err(_) => Err(ServiceError::invalid(format!(
            "invalid tool approval mode: {raw}"
        ))),
    }
}
pub fn tool_approval_mode_to(mode: d::ToolApprovalMode) -> i32 {
    match mode {
        d::ToolApprovalMode::Unspecified => pb::ToolApprovalMode::Unspecified as i32,
        d::ToolApprovalMode::Ask => pb::ToolApprovalMode::Ask as i32,
        d::ToolApprovalMode::Auto => pb::ToolApprovalMode::Auto as i32,
    }
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
        mode: session_mode_from(value.mode)?,
        artifacts: value.artifacts,
        timeout_secs: value.timeout_secs,
        idempotency_key: value.idempotency_key,
        generate_image: value.generate_image,
        retention: session_retention_from(value.retention)?,
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
        mode: session_mode_to(value.mode),
        artifacts: value.artifacts,
        timeout_secs: value.timeout_secs,
        idempotency_key: value.idempotency_key,
        generate_image: value.generate_image,
        retention: session_retention_to(value.retention),
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
        InvalidRequest => (pb::ErrorCode::InvalidRequest, tonic::Code::InvalidArgument),
        UnsupportedCapability => (
            pb::ErrorCode::UnsupportedCapability,
            tonic::Code::Unimplemented,
        ),
        AuthRequired => (pb::ErrorCode::AuthRequired, tonic::Code::FailedPrecondition),
        VerificationRequired => (
            pb::ErrorCode::VerificationRequired,
            tonic::Code::FailedPrecondition,
        ),
        RateLimited => (pb::ErrorCode::RateLimited, tonic::Code::ResourceExhausted),
        UiContractChanged => (
            pb::ErrorCode::UiContractChanged,
            tonic::Code::FailedPrecondition,
        ),
        TransportDisconnected => (
            pb::ErrorCode::TransportDisconnected,
            tonic::Code::Unavailable,
        ),
        SubmissionOutcomeUnknown => (
            pb::ErrorCode::SubmissionOutcomeUnknown,
            tonic::Code::Aborted,
        ),
        UploadFailed => (pb::ErrorCode::UploadFailed, tonic::Code::Internal),
        MediaDownloadFailed => (pb::ErrorCode::MediaDownloadFailed, tonic::Code::Internal),
        ToolProtocolInvalid => (
            pb::ErrorCode::ToolProtocolInvalid,
            tonic::Code::InvalidArgument,
        ),
        WorkspaceConflict => (
            pb::ErrorCode::WorkspaceConflict,
            tonic::Code::FailedPrecondition,
        ),
        Cancelled => (pb::ErrorCode::Cancelled, tonic::Code::Cancelled),
        DeadlineExceeded => (
            pb::ErrorCode::DeadlineExceeded,
            tonic::Code::DeadlineExceeded,
        ),
        QueueFull => (pb::ErrorCode::QueueFull, tonic::Code::ResourceExhausted),
        NotFound => (pb::ErrorCode::NotFound, tonic::Code::NotFound),
        StorageFailure => (pb::ErrorCode::StorageFailure, tonic::Code::Internal),
        Internal => (pb::ErrorCode::Internal, tonic::Code::Internal),
        SessionUnavailable => (
            pb::ErrorCode::SessionUnavailable,
            tonic::Code::FailedPrecondition,
        ),
    };
    pb::ErrorDetail {
        code: code as i32,
        message: e.message,
        grpc_code: grpc_code as i32,
        stage: e.stage,
        retry: e.retry,
    }
}
pub fn error_from(e: pb::ErrorDetail) -> ServiceError {
    use d::ErrorCode::*;
    let code = match pb::ErrorCode::try_from(e.code) {
        Ok(pb::ErrorCode::InvalidRequest) => InvalidRequest,
        Ok(pb::ErrorCode::UnsupportedCapability) => UnsupportedCapability,
        Ok(pb::ErrorCode::AuthRequired) => AuthRequired,
        Ok(pb::ErrorCode::VerificationRequired) => VerificationRequired,
        Ok(pb::ErrorCode::RateLimited) => RateLimited,
        Ok(pb::ErrorCode::UiContractChanged) => UiContractChanged,
        Ok(pb::ErrorCode::TransportDisconnected) => TransportDisconnected,
        Ok(pb::ErrorCode::SubmissionOutcomeUnknown) => SubmissionOutcomeUnknown,
        Ok(pb::ErrorCode::UploadFailed) => UploadFailed,
        Ok(pb::ErrorCode::MediaDownloadFailed) => MediaDownloadFailed,
        Ok(pb::ErrorCode::ToolProtocolInvalid) => ToolProtocolInvalid,
        Ok(pb::ErrorCode::WorkspaceConflict) => WorkspaceConflict,
        Ok(pb::ErrorCode::Cancelled) => Cancelled,
        Ok(pb::ErrorCode::DeadlineExceeded) => DeadlineExceeded,
        Ok(pb::ErrorCode::QueueFull) => QueueFull,
        Ok(pb::ErrorCode::NotFound) => NotFound,
        Ok(pb::ErrorCode::StorageFailure) => StorageFailure,
        Ok(pb::ErrorCode::SessionUnavailable) => SessionUnavailable,
        Ok(pb::ErrorCode::Internal | pb::ErrorCode::Unspecified) | Err(_) => Internal,
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
pub fn browser_profile_to(b: d::BrowserProfile) -> pb::BrowserProfile {
    pb::BrowserProfile {
        provider_id: b.provider_id,
        profile: b.profile.to_string_lossy().into_owned(),
        subprofile: b.subprofile,
        port: u32::from(b.port),
        headless: b.headless,
    }
}
pub fn browser_profile_from(b: pb::BrowserProfile) -> Result<d::BrowserProfile> {
    Ok(d::BrowserProfile {
        provider_id: b.provider_id,
        profile: PathBuf::from(b.profile),
        subprofile: b.subprofile,
        port: u16::try_from(b.port).map_err(|_| ServiceError::invalid("invalid browser port"))?,
        headless: b.headless,
    })
}
pub fn account_to(a: d::Account) -> pb::Account {
    pb::Account {
        id: a.id,
        provider: a.provider,
        state: a.state,
        identity: a.identity,
        label: a.label,
        slot: a.slot,
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
        state: a.state,
        identity: a.identity.filter(|s| !s.trim().is_empty()),
        label: a.label.filter(|s| !s.trim().is_empty()),
        slot: a.slot.filter(|s| !s.trim().is_empty()),
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
        multi_account: c.multi_account,
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
        multi_account: c.multi_account,
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
        entry_url_template: m.entry_url_template,
        usage_url_template: m.usage_url_template,
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
        entry_url_template: r.entry_url_template,
        usage_url_template: r.usage_url_template,
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
    let continuable = s.is_continuable();
    pb::Session {
        id: s.id,
        account_id: s.account_id,
        mode: session_mode_to(s.mode),
        state: session_state_to(s.state),
        model: s.model,
        thinking: s.thinking,
        last_used: s.last_used,
        remote_turns: s.remote_turns,
        provider: s.provider,
        reuse_scope: s.reuse_scope,
        continuable,
    }
}
pub fn session_json(s: pb::Session) -> Value {
    let mode = match pb::SessionMode::try_from(s.mode) {
        Ok(pb::SessionMode::Chat) => "chat",
        Ok(pb::SessionMode::Temporary) => "temporary",
        _ => "unspecified",
    };
    let state = session_state_from(s.state).map_or("unspecified", d::SessionState::as_str);
    json!({
        "id": s.id,
        "account_id": s.account_id,
        "provider": s.provider,
        "mode": mode,
        "state": state,
        "model": s.model,
        "thinking": s.thinking,
        "last_used": s.last_used,
        "remote_turns": s.remote_turns,
        "reuse_scope": s.reuse_scope,
        "continuable": s.continuable,
    })
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
        provider_id: b.provider_id,
        connected: b.connected,
        endpoint: b.endpoint,
        browser: b.browser,
        profile: b.profile.to_string_lossy().into_owned(),
        profile_matches: b.profile_matches,
        headless: b.headless,
        owned_sessions: b.owned_sessions,
        account_count: b.account_count,
        ready_accounts: b.ready_accounts,
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
pub fn agent_request_to(r: d::AgentRequest) -> pb::AgentRequest {
    pb::AgentRequest {
        environment: r.environment.unwrap_or_default().into_iter().collect(),
        task: r.task,
        workspace: r.workspace.to_string_lossy().into_owned(),
        account_id: r.account_id,
        model: r.model,
        thinking: r.thinking,
        session_mode: session_mode_to(r.session_mode),
        no_shell: r.no_shell,
        max_steps: u32::try_from(r.max_steps).unwrap_or(u32::MAX),
        knowledge: r.knowledge,
        mcp_config: r.mcp_config.map(|p| p.to_string_lossy().into_owned()),
        artifacts: r.artifacts,
        session_retention: session_retention_to(r.session_retention),
        chat_only: r.chat_only,
        permissions: r.permissions.map(tool_permissions_to),
        conversation_id: r.conversation_id,
        tool_approval: tool_approval_mode_to(r.tool_approval),
    }
}
pub fn agent_request_from(r: pb::AgentRequest) -> Result<d::AgentRequest> {
    let account_id = r.account_id.trim().to_owned();
    let conversation_id = r
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok(d::AgentRequest {
        environment: (!r.environment.is_empty()).then(|| r.environment.into_iter().collect()),
        task: r.task,
        workspace: PathBuf::from(r.workspace),
        account_id,
        model: r.model,
        thinking: r.thinking,
        session_mode: session_mode_from(r.session_mode)?,
        no_shell: r.no_shell,
        max_steps: r.max_steps as usize,
        knowledge: r.knowledge,
        mcp_config: r.mcp_config.map(PathBuf::from),
        artifacts: r.artifacts,
        conversation_id,
        chat_only: r.chat_only,
        permissions: r.permissions.map(tool_permissions_from),
        session_retention: session_retention_from(r.session_retention)?,
        tool_approval: tool_approval_mode_from(r.tool_approval)?,
    })
}
pub fn agent_to(record: d::AgentRecord) -> pb::Agent {
    let mut request = record.request;
    request.environment = None;
    pb::Agent {
        id: record.id,
        conversation_id: record.conversation_id,
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
            inspector_pruned: res.inspector_pruned,
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
    )?;
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
                inspector_pruned: res.inspector_pruned,
            })
        })
        .transpose()?;
    Ok(d::AgentRecord {
        id: value.id,
        conversation_id: value.conversation_id,
        state: value.state,
        request,
        result,
        error: value.error.map(error_from),
        created_at: value.created_at,
        updated_at: value.updated_at,
    })
}
pub fn conversation_summary_to(s: d::ConversationSummary) -> pb::ConversationSummary {
    pb::ConversationSummary {
        id: s.id,
        latest_agent_id: s.latest_agent_id,
        workspace: s.workspace.to_string_lossy().into_owned(),
        account_id: s.account_id,
        state: s.state,
        session_mode: session_mode_to(s.session_mode),
        title: s.title,
        turn_count: s.turn_count,
        created_at: s.created_at,
        updated_at: s.updated_at,
        model: s.model,
        thinking: s.thinking,
        chat_only: s.chat_only,
    }
}
pub fn conversation_summary_from(s: pb::ConversationSummary) -> Result<d::ConversationSummary> {
    Ok(d::ConversationSummary {
        id: s.id,
        latest_agent_id: s.latest_agent_id,
        workspace: PathBuf::from(s.workspace),
        account_id: s.account_id,
        state: s.state,
        session_mode: session_mode_from(s.session_mode)?,
        title: s.title,
        turn_count: s.turn_count,
        created_at: s.created_at,
        updated_at: s.updated_at,
        model: s.model,
        thinking: s.thinking,
        chat_only: s.chat_only,
    })
}
pub fn conversation_to(c: d::ConversationRecord) -> pb::Conversation {
    pb::Conversation {
        summary: Some(conversation_summary_to(c.summary)),
        turns: c.turns.into_iter().map(agent_to).collect(),
    }
}
pub fn conversation_from(c: pb::Conversation) -> Result<d::ConversationRecord> {
    let summary = conversation_summary_from(
        c.summary
            .ok_or_else(|| ServiceError::invalid("missing conversation summary"))?,
    )?;
    let turns = c.turns.into_iter().map(agent_from).collect::<Result<_>>()?;
    Ok(d::ConversationRecord { summary, turns })
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
                "identity": a.identity,
                "label": a.label,
                "slot": a.slot,
                "active": a.active,
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
    let workspace_alerts: Vec<_> = status
        .workspace_alerts
        .into_iter()
        .map(workspace_alert_from)
        .collect();
    json!({
        "server_id": status.server_id,
        "runtime": status.runtime,
        "active_agents": status.active_agents,
        "browsers": status.browsers,
        "accounts": accounts,
        "active_account_id": status.active_account_id,
        "queued": usage.queued,
        "running": usage.running,
        "usage": usage,
        "providers": providers,
        "browser_connection": status.browser_connection,
        "chat_cleanup": status.chat_cleanup,
        "workspace_alerts": workspace_alerts
    })
}

pub(crate) fn workspace_alert_to(
    alert: crate::agent_service::WorkspaceAlertRecord,
) -> pb::WorkspaceAlert {
    let kind = match alert.kind {
        crate::agent_service::WorkspaceAlertKind::Approval => pb::WorkspaceAlertKind::Approval,
        crate::agent_service::WorkspaceAlertKind::Attention => pb::WorkspaceAlertKind::Attention,
        crate::agent_service::WorkspaceAlertKind::Failed => pb::WorkspaceAlertKind::Failed,
        crate::agent_service::WorkspaceAlertKind::Completed => pb::WorkspaceAlertKind::Completed,
    };
    pb::WorkspaceAlert {
        workspace: alert.workspace,
        agent_id: alert.agent_id,
        kind: kind as i32,
        count: alert.count,
        updated_at: alert.updated_at,
    }
}

fn workspace_alert_from(alert: pb::WorkspaceAlert) -> Value {
    let kind = match pb::WorkspaceAlertKind::try_from(alert.kind) {
        Ok(pb::WorkspaceAlertKind::Approval) => "approval",
        Ok(pb::WorkspaceAlertKind::Attention) => "attention",
        Ok(pb::WorkspaceAlertKind::Failed) => "failed",
        Ok(pb::WorkspaceAlertKind::Completed) => "completed",
        _ => "unspecified",
    };
    json!({
        "workspace": alert.workspace,
        "agent_id": alert.agent_id,
        "kind": kind,
        "count": alert.count,
        "updated_at": alert.updated_at
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
        let request = agent_request_from(wire).unwrap();
        assert!(!serde_json::to_string(&request)
            .unwrap()
            .contains("transient-fixture-value"));
        let response = agent_to(d::AgentRecord {
            id: "fixture".into(),
            conversation_id: "conv-fixture".into(),
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

    #[test]
    fn session_enums_retention_and_error_codes_roundtrip_cleanly() {
        for mode in [d::SessionMode::Chat, d::SessionMode::Temporary] {
            let wire = session_mode_to(mode);
            assert_eq!(session_mode_from(wire).unwrap(), mode);
            assert_eq!(session_mode_filter(wire).unwrap(), Some(mode));
        }
        assert_eq!(
            session_mode_from(pb::SessionMode::Unspecified as i32).unwrap(),
            d::SessionMode::Chat
        );
        assert_eq!(
            session_mode_filter(pb::SessionMode::Unspecified as i32).unwrap(),
            None
        );
        assert!(session_mode_from(999).is_err());

        for state in [
            d::SessionState::Ready,
            d::SessionState::Running,
            d::SessionState::Closed,
            d::SessionState::Deleted,
            d::SessionState::Invalidated,
            d::SessionState::OutcomeUnknown,
            d::SessionState::AwaitingHuman,
            d::SessionState::CleanupPending,
        ] {
            let wire = session_state_to(state);
            assert_eq!(session_state_from(wire).unwrap(), state);
            assert_eq!(session_state_filter(wire).unwrap(), Some(state));
        }
        assert!(session_state_from(pb::SessionState::Unspecified as i32).is_err());
        assert_eq!(
            session_state_filter(pb::SessionState::Unspecified as i32).unwrap(),
            None
        );

        for retention in [
            d::SessionRetention::Unspecified,
            d::SessionRetention::Ephemeral,
            d::SessionRetention::Retain,
        ] {
            let wire = session_retention_to(retention);
            assert_eq!(session_retention_from(wire).unwrap(), retention);
        }
        assert!(session_retention_from(999).is_err());

        let mut req = d::TurnRequest::text("hi");
        req.mode = d::SessionMode::Temporary;
        req.retention = d::SessionRetention::Retain;
        let wire_req = request_to(req);
        assert_eq!(wire_req.mode, pb::SessionMode::Temporary as i32);
        assert_eq!(wire_req.retention, pb::SessionRetention::Retain as i32);
        let back_req = request_from(wire_req).unwrap();
        assert_eq!(back_req.mode, d::SessionMode::Temporary);
        assert_eq!(back_req.retention, d::SessionRetention::Retain);

        for mode in [
            d::ToolApprovalMode::Unspecified,
            d::ToolApprovalMode::Ask,
            d::ToolApprovalMode::Auto,
        ] {
            let wire = tool_approval_mode_to(mode);
            assert_eq!(tool_approval_mode_from(wire).unwrap(), mode);
        }
        assert!(tool_approval_mode_from(999).is_err());

        let agent_req = d::AgentRequest {
            environment: None,
            task: "typed agent".into(),
            workspace: PathBuf::from("/tmp/ws"),
            account_id: "default".into(),
            model: None,
            thinking: None,
            session_mode: d::SessionMode::Temporary,
            no_shell: false,
            max_steps: 42,
            knowledge: None,
            mcp_config: None,
            artifacts: vec![],
            conversation_id: Some("conv-typed".into()),
            chat_only: true,
            permissions: Some(d::ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: true,
                subagent: false,
                ast: true,
            }),
            session_retention: d::SessionRetention::Ephemeral,
            tool_approval: d::ToolApprovalMode::Ask,
        };
        let wire_agent = agent_request_to(agent_req.clone());
        assert_eq!(wire_agent.session_mode, pb::SessionMode::Temporary as i32);
        assert_eq!(
            wire_agent.session_retention,
            pb::SessionRetention::Ephemeral as i32
        );
        assert_eq!(wire_agent.tool_approval, pb::ToolApprovalMode::Ask as i32);
        assert_eq!(wire_agent.max_steps, 42);
        assert!(wire_agent.chat_only);
        let back_agent = agent_request_from(wire_agent).unwrap();
        assert_eq!(back_agent.session_mode, d::SessionMode::Temporary);
        assert_eq!(back_agent.session_retention, d::SessionRetention::Ephemeral);
        assert_eq!(back_agent.tool_approval, d::ToolApprovalMode::Ask);
        assert_eq!(back_agent.max_steps, 42);
        assert!(back_agent.chat_only);
        assert_eq!(back_agent.permissions, agent_req.permissions);

        let err = ServiceError::session_unavailable("resume", "tab gone");
        let wire_err = error_to(err);
        assert_eq!(wire_err.code, pb::ErrorCode::SessionUnavailable as i32);
        assert_eq!(wire_err.grpc_code, tonic::Code::FailedPrecondition as i32);
        let back_err = error_from(wire_err);
        assert_eq!(back_err.code, d::ErrorCode::SessionUnavailable);
        assert_eq!(back_err.stage, "resume");

        let wire_session = session_to(d::Session {
            id: "sess_1".into(),
            provider: d::default_provider(),
            account_id: "default".into(),
            mode: d::SessionMode::Temporary,
            target_id: Some("tab-1".into()),
            remote_url: None,
            remote_session_id: None,
            history: vec![],
            state: d::SessionState::Ready,
            model: Some("pro".into()),
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 2,
            last_used: 42,
            reuse_scope: None,
        });
        assert_eq!(wire_session.mode, pb::SessionMode::Temporary as i32);
        assert_eq!(wire_session.state, pb::SessionState::Ready as i32);
        assert!(wire_session.continuable);
        let rendered = session_json(wire_session);
        assert_eq!(rendered["mode"], "temporary");
        assert_eq!(rendered["state"], "ready");
        assert_eq!(rendered["continuable"], true);
    }
}
