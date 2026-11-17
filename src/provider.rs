//! Provider abstraction and runtime provider registry.
//!
//! Each provider encapsulates:
//! - Turn execution (`generate`, `close_session`, `delete_conversation`, `recover_media`, `clear_web_history`)
//! - Account lifecycle (`default_login_url`, `is_login_ready`, `discover_models`, `read_usage`)
//! - Declared metadata and capabilities (`ProviderInfo`, `ProviderCapabilities`)
//!
//! New providers can be added in two ways:
//! 1. Implement `Provider` in Rust and register it at startup or runtime via `ProviderRegistry::register`.
//! 2. Supply a declarative `ProviderManifest` (via `<ANTICRUMB_HOME>/providers/<id>/provider.json`
//!    or `ProviderService/RegisterProvider`) backed by `ManifestWebProvider`.

pub(crate) mod bundled;
pub mod gemini;
pub mod manifest;
pub mod registry;

pub use gemini::GeminiWebProvider;
pub use manifest::{ManifestWebProvider, ProviderManifest};
pub use registry::ProviderRegistry;

use crate::{agent::CancelToken, domain::*, service::Emit, store::Store};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PROVIDER_ID: &str = "gemini-web";
pub const DEFAULT_INLINE_PROMPT_CHARS: u32 = 2_000;

pub const fn default_inline_prompt_chars() -> u32 {
    DEFAULT_INLINE_PROMPT_CHARS
}

pub fn validate_provider_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(ServiceError::invalid(
            "provider ID must be 1..=64 chars of [a-z0-9_-] starting with [a-z0-9]",
        ))
    }
}

pub fn suggest_account_id(provider_id: &str, existing_ids: &[&str]) -> String {
    let base = provider_id
        .strip_suffix("-web")
        .filter(|s| !s.is_empty())
        .unwrap_or(provider_id);
    if !existing_ids.contains(&base) {
        return base.to_owned();
    }
    if provider_id != base && !existing_ids.contains(&provider_id) {
        return provider_id.to_owned();
    }
    for index in 2usize.. {
        let candidate = format!("{base}-{index}");
        if !existing_ids.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderCapabilities {
    pub browser_auth: bool,
    pub temporary_sessions: bool,
    pub file_uploads: bool,
    pub image_generation: bool,
    pub web_usage: bool,
    pub web_history_cleanup: bool,
    pub native_tools: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_url: Option<String>,
    pub default_model: String,
    pub source: String,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    /// Maximum characters kept in the browser editor on follow-up agent turns.
    /// Zero attaches every nonempty follow-up; the first prompt is always attached.
    #[serde(default = "default_inline_prompt_chars")]
    pub inline_prompt_chars: u32,
}

impl ProviderInfo {
    pub fn adapter_id(&self) -> String {
        format!("{}/{}", self.id, self.version)
    }
}

pub struct ProviderContext<'a> {
    pub store: Option<&'a Store>,
}

impl<'a> ProviderContext<'a> {
    pub fn with_store(store: &'a Store) -> Self {
        Self { store: Some(store) }
    }

    pub fn stateless() -> Self {
        Self { store: None }
    }
}

pub trait Provider: Send + Sync + 'static {
    fn info(&self) -> ProviderInfo;

    fn adapter_id(&self) -> String {
        self.info().adapter_id()
    }

    fn default_login_url(&self) -> Option<&str> {
        None
    }

    fn is_login_ready(&self, _account: &Account, _cancel: CancelToken) -> Result<bool> {
        Ok(false)
    }

    fn discover_models(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: CancelToken,
    ) -> Result<ModelCatalogRecord>;

    fn generate(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult>;

    fn close_session(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &Session,
    ) -> Result<()>;

    fn delete_conversation(
        &self,
        _ctx: &ProviderContext<'_>,
        _account: &Account,
        _session: &Session,
        _cancel: CancelToken,
    ) -> Result<()> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "chat_cleanup",
            format!(
                "provider '{}' cannot delete remote conversations",
                self.info().id
            ),
        ))
    }

    fn recover_media(
        &self,
        _ctx: &ProviderContext<'_>,
        _account: &Account,
        _session: &Session,
    ) -> Result<Vec<String>> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "download",
            format!(
                "provider '{}' does not support media recovery",
                self.info().id
            ),
        ))
    }

    fn clear_web_history(
        &self,
        _ctx: &ProviderContext<'_>,
        _account: &Account,
        _cancel: CancelToken,
        _progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "web_history",
            format!("provider '{}' cannot clear remote history", self.info().id),
        ))
    }

    fn read_usage(
        &self,
        _ctx: &ProviderContext<'_>,
        _account: &Account,
        _cancel: CancelToken,
    ) -> Result<WebUsageReport> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "usage",
            format!(
                "provider '{}' does not expose account usage",
                self.info().id
            ),
        ))
    }

    fn on_register(&self) -> Result<()> {
        Ok(())
    }

    fn on_unregister(&self) {}
}
