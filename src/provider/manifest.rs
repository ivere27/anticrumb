use super::{
    gemini::{model_discovery_error, unique_model_aliases},
    validate_provider_id, Provider, ProviderCapabilities, ProviderContext, ProviderInfo,
};
#[path = "history.rs"]
mod history;
use crate::{
    account::{MODEL_DISCOVERY_TIMEOUT, WEB_USAGE_TIMEOUT},
    agent::CancelToken,
    browser_host as browser,
    cdp::CdpClient,
    domain::*,
    gemini::{
        load_human_pace, probe_matches_model_aliases, probe_matches_thinking, AskResult,
        AttachmentProgress, ChatSessionMode, StreamUpdate,
    },
    service::{Emit, TempDirGuard},
    store::Store,
    tab_pool,
    url_lite::Url,
    web_chat::{self, WebChatConfig, WebChatTab, DEFAULT_OBSERVER_BINDING},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn default_version() -> String {
    "v1".into()
}

fn default_kind() -> String {
    "web_provider".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderManifest {
    pub id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub entry_url: Option<String>,
    pub allowed_origins: Vec<String>,
    pub auth_url_substrings: Vec<String>,
    pub chat_url_prefixes: Vec<String>,
    pub reusable_path_prefixes: Vec<String>,
    pub usage_url: Option<String>,
    pub observer_binding: Option<String>,
    pub requires_models_control: bool,
    pub default_model: Option<String>,
    pub scripts_dir: Option<PathBuf>,
    pub scripts: BTreeMap<String, String>,
    pub capabilities: ProviderCapabilities,
    /// Omitted values use the shared default; zero attaches every nonempty follow-up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_prompt_chars: Option<u32>,
}

fn validate_http_url(raw: &str, label: &str) -> Result<()> {
    let parsed = Url::parse(raw)
        .map_err(|_| ServiceError::invalid(format!("invalid provider {label}: {raw}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(ServiceError::invalid(format!(
            "provider {label} must be an http(s) URL"
        )));
    }
    Ok(())
}

fn validate_binding_name(raw: &str) -> Result<()> {
    let mut chars = raw.bytes();
    let valid = !raw.is_empty()
        && raw.len() <= 64
        && chars
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && chars.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(ServiceError::invalid(
            "provider observer_binding must be a non-empty ASCII identifier up to 64 chars",
        ))
    }
}

impl ProviderManifest {
    /// Anchor explicit relative script paths to the manifest, not the caller's cwd.
    /// Leave omitted paths unset so each caller retains its discovery defaults.
    pub(crate) fn read_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(ServiceError::storage)?;
        let mut manifest: Self = serde_json::from_str(&raw).map_err(ServiceError::storage)?;
        if let Some(scripts_dir) = &mut manifest.scripts_dir {
            if scripts_dir.is_relative() {
                let provider_dir = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .canonicalize()
                    .map_err(ServiceError::storage)?;
                *scripts_dir = provider_dir.join(&*scripts_dir);
            }
        }
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        validate_provider_id(&self.id)?;
        if self.name.trim().is_empty() {
            return Err(ServiceError::invalid("provider name must not be empty"));
        }
        if self.version.trim().is_empty() {
            return Err(ServiceError::invalid("provider version must not be empty"));
        }
        if self.kind.trim().is_empty() {
            return Err(ServiceError::invalid("provider kind must not be empty"));
        }
        if let Some(url) = &self.entry_url {
            validate_http_url(url, "entry_url")?;
        }
        for origin in &self.allowed_origins {
            validate_http_url(origin, "allowed_origin")?;
        }
        if let Some(url) = &self.usage_url {
            validate_http_url(url, "usage_url")?;
        }
        for prefix in &self.chat_url_prefixes {
            validate_http_url(prefix, "chat_url_prefix")?;
        }
        for needle in &self.auth_url_substrings {
            if needle.trim().is_empty() || needle.chars().any(char::is_control) {
                return Err(ServiceError::invalid(
                    "provider auth_url_substrings entries must be non-empty without control characters",
                ));
            }
        }
        for path_prefix in &self.reusable_path_prefixes {
            if !path_prefix.starts_with('/') || path_prefix.chars().any(char::is_control) {
                return Err(ServiceError::invalid(
                    "provider reusable_path_prefixes entries must start with '/'",
                ));
            }
        }
        if let Some(binding) = &self.observer_binding {
            validate_binding_name(binding)?;
        }
        for (name, body) in &self.scripts {
            if name.is_empty()
                || matches!(name.as_str(), "." | "..")
                || name.contains(['/', '\\', '\0'])
            {
                return Err(ServiceError::invalid(format!(
                    "invalid provider script name: {name}"
                )));
            }
            if body.len() > 128 * 1024 {
                return Err(ServiceError::invalid(format!(
                    "provider script {name} exceeds 128 KiB"
                )));
            }
        }
        Ok(())
    }

    pub fn resolved_default_model(&self) -> String {
        self.default_model
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{}-default", self.id))
    }

    pub fn resolved_inline_prompt_chars(&self) -> u32 {
        self.inline_prompt_chars
            .unwrap_or(super::DEFAULT_INLINE_PROMPT_CHARS)
    }
}

pub struct ManifestWebProvider {
    manifest: ProviderManifest,
    builtin: bool,
}

impl ManifestWebProvider {
    pub fn new(manifest: ProviderManifest) -> Result<Self> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            builtin: false,
        })
    }

    pub(crate) fn bundled(manifest: ProviderManifest) -> Result<Self> {
        let mut provider = Self::new(manifest)?;
        provider.builtin = true;
        Ok(provider)
    }

    fn resolve_script(
        manifest: &ProviderManifest,
        name: &str,
    ) -> std::result::Result<Cow<'static, str>, String> {
        if let Some(dir) = &manifest.scripts_dir {
            if dir.join(name).is_file() {
                return crate::adapter::load_for(&manifest.id, name, Some(dir));
            }
        }
        if let Some(inline) = manifest.scripts.get(name) {
            return Ok(Cow::Owned(inline.clone()));
        }
        crate::adapter::script_for(&manifest.id, name)
    }

    fn script(&self, name: &str) -> Result<Cow<'static, str>> {
        Self::resolve_script(&self.manifest, name).map_err(ServiceError::invalid)
    }

    fn web_chat_config(&self) -> Result<Arc<WebChatConfig>> {
        let entry_url = self.manifest.entry_url.clone().ok_or_else(|| {
            ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "provider",
                format!(
                    "provider '{}' has no entry_url configured",
                    self.manifest.id
                ),
            )
        })?;
        let manifest = self.manifest.clone();
        Ok(Arc::new(WebChatConfig {
            provider_id: self.manifest.id.clone(),
            provider_name: self.manifest.name.clone(),
            entry_url,
            allowed_origins: self.manifest.allowed_origins.clone(),
            auth_url_substrings: self.manifest.auth_url_substrings.clone(),
            chat_url_prefixes: self.manifest.chat_url_prefixes.clone(),
            reusable_path_prefixes: self.manifest.reusable_path_prefixes.clone(),
            observer_binding: self
                .manifest
                .observer_binding
                .clone()
                .unwrap_or_else(|| DEFAULT_OBSERVER_BINDING.into()),
            requires_models_control: self.manifest.requires_models_control,
            usage_url: self.manifest.usage_url.clone(),
            resolve_script: Arc::new(move |name| Self::resolve_script(&manifest, name)),
        }))
    }

    fn default_catalog(&self, signed_in: bool) -> ModelCatalogRecord {
        ModelCatalogRecord {
            provider: Some(self.manifest.id.clone()),
            adapter: Some(self.adapter_id()),
            checked_at: Some(now_ms()),
            signed_in: Some(signed_in),
            models: vec![ModelOption {
                index: 0,
                mode_id: self.manifest.resolved_default_model(),
                label: self.manifest.name.clone(),
                sublabel: format!("{} ({})", self.manifest.name, self.manifest.version),
                selected: true,
                locked: false,
                rate_limit_reset: None,
            }],
            ..Default::default()
        }
    }

    fn require_web_turn_scripts(&self) -> Result<()> {
        for script in ["driver.js", "observer.js", "extract-response.js"] {
            if self.script(script).is_err() {
                return Err(ServiceError::new(
                    ErrorCode::UnsupportedCapability,
                    "provider",
                    format!(
                        "provider '{}' requires '{script}' in scripts, scripts_dir, or <ANTICRUMB_HOME>/providers/{}",
                        self.manifest.id, self.manifest.id
                    ),
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn acquire_tab(
        &self,
        store: &Store,
        config: &Arc<WebChatConfig>,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: &CancelToken,
        deadline: Instant,
        emit: Emit<'_>,
    ) -> Result<(WebChatTab, bool, bool)> {
        let opts = browser::LaunchOptions::headless_account(account, &config.entry_url);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let other_owned: HashSet<String> = store
            .list::<Session>("local", "session")
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.id != session.id && !s.is_inactive())
            .filter_map(|s| s.target_id)
            .collect();
        let owned = session
            .target_id
            .as_ref()
            .filter(|id| !other_owned.contains(*id))
            .and_then(|id| {
                browser::list_targets(host.port)
                    .ok()?
                    .into_iter()
                    .find(|t| &t.id == id)
            });
        let continuing = session.remote_turns > 0 || session.target_id.is_some();
        let mut same_warm_tab = false;
        let (target, mode, reused) = if let Some(target) = owned {
            emit(
                "browser_stage",
                json!({"stage": "checking_tab", "detail": format!("verifying owned tab {}", target.id)}),
            )?;
            let mut navigated = false;
            if let Some(expected) = &session.remote_url {
                let expected_url = Url::parse(expected).map_err(ServiceError::storage)?;
                let current_url = Url::parse(&target.url).map_err(ServiceError::storage)?;
                if expected_url.host_str() != current_url.host_str()
                    || expected_url.path() != current_url.path()
                {
                    if session.mode == SessionMode::Chat {
                        emit(
                            "browser_stage",
                            json!({"stage": "navigating_tab", "detail": format!("restoring saved {} conversation URL", self.manifest.name)}),
                        )?;
                        crate::gemini::navigate_tab_commit(
                            host.port, &target, expected, cancel, deadline,
                        )
                        .map_err(|e| ServiceError::backend(e, false))?;
                        navigated = true;
                    } else {
                        return Err(ServiceError::invalid(
                            "owned tab navigated to another conversation; start a new session",
                        ));
                    }
                }
            }
            same_warm_tab = !navigated;
            (target, ChatSessionMode::Continue, false)
        } else if continuing {
            if session.mode == SessionMode::Chat {
                if let Some(remote_url) = session.remote_url.clone() {
                    emit(
                        "browser_stage",
                        json!({"stage": "navigating_tab", "detail": format!("acquiring tab & opening saved {} chat", self.manifest.name)}),
                    )?;
                    let (target, _) = web_chat::with_tab_pool_matcher(config, |matcher| {
                        tab_pool::acquire_session_tab_for(
                            store,
                            &account.id,
                            host.port,
                            &session.id,
                            &SessionMode::Chat,
                            matcher,
                        )
                    })?;
                    crate::gemini::navigate_tab_commit(
                        host.port,
                        &target,
                        &remote_url,
                        cancel,
                        deadline,
                    )
                    .map_err(|e| ServiceError::backend(e, false))?;
                    (target, ChatSessionMode::Continue, false)
                } else {
                    return Err(ServiceError::new(
                        ErrorCode::SubmissionOutcomeUnknown,
                        "resume",
                        "owned browser tab is gone; create a new session from a checkpoint",
                    ));
                }
            } else {
                return Err(ServiceError::new(
                    ErrorCode::SubmissionOutcomeUnknown,
                    "resume",
                    "owned browser tab is gone; create a new session from a checkpoint",
                ));
            }
        } else {
            emit(
                "browser_stage",
                json!({"stage": "acquiring_tab", "detail": format!("opening {:?} {} tab", request.mode, self.manifest.name)}),
            )?;
            let mode = if request.mode == SessionMode::Temporary {
                ChatSessionMode::TempChat
            } else {
                ChatSessionMode::NewChat
            };
            let (target, reused) = web_chat::with_tab_pool_matcher(config, |matcher| {
                tab_pool::acquire_session_tab_for(
                    store,
                    &account.id,
                    host.port,
                    &session.id,
                    &request.mode,
                    matcher,
                )
            })?;
            (target, mode, reused)
        };
        session.target_id = Some(target.id.clone());
        store.put("local", "session", &session.id, session)?;
        let mut tab = if reused {
            WebChatTab::attach_reused(
                host.port,
                target,
                mode,
                config.clone(),
                cancel.clone(),
                deadline,
                request.generate_image,
            )
        } else {
            WebChatTab::attach(
                host.port,
                target,
                mode,
                config.clone(),
                cancel.clone(),
                deadline,
                request.generate_image,
            )
        }
        .map_err(|e| ServiceError::backend(e, false))?;
        tab.set_control(cancel.clone(), deadline);
        let is_continue = mode == ChatSessionMode::Continue;
        Ok((tab, is_continue, is_continue && same_warm_tab))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_tab(
        &self,
        store: &Store,
        tab: &mut WebChatTab,
        account: &Account,
        session: &Session,
        request: &TurnRequest,
        same_warm_tab: bool,
        emit: Emit<'_>,
    ) -> Result<()> {
        let initial_probe = tab.initial_probe().clone();
        let default_model = self.manifest.resolved_default_model();
        let mut requested_model = request.model.as_deref().filter(|m| {
            *m != default_model && (!same_warm_tab || session.model.as_deref() != Some(*m))
        });
        let mut requested_thinking = request
            .thinking
            .as_deref()
            .filter(|t| !same_warm_tab || session.thinking.as_deref() != Some(*t));
        if let Some(model_query) = requested_model {
            let catalog = store.model_catalog(&account.id).ok();
            let aliases = unique_model_aliases(catalog.as_ref(), model_query);
            if probe_matches_model_aliases(&initial_probe, &aliases) {
                requested_model = None;
            }
        }
        if let Some(thinking_query) = requested_thinking {
            if probe_matches_thinking(&initial_probe, thinking_query) {
                requested_thinking = None;
            }
        }
        let changed_selection = requested_model.is_some() || requested_thinking.is_some();
        if changed_selection {
            emit(
                "browser_stage",
                json!({
                    "stage": "selecting_model",
                    "detail": format!(
                        "model={} thinking={}",
                        requested_model.unwrap_or("keep"),
                        requested_thinking.unwrap_or("keep")
                    )
                }),
            )?;
        }
        tab.select_model_and_thinking(requested_model, requested_thinking)
            .map_err(|e| ServiceError::backend(e, false))?;
        let model = if changed_selection {
            tab.current_model()
                .map_err(|e| ServiceError::backend(e, false))?
        } else {
            initial_probe["model"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| tab.current_model().ok().flatten())
        };
        store.put(
            &account.id,
            "runtime",
            "selection",
            &json!({"model": model, "at": now_ms()}),
        )?;
        emit("model_info", json!({"model": model}))?;
        let mut files = Vec::new();
        let upload_root = store.root().join("uploads").join(id("turn"));
        let _cleanup = TempDirGuard::new(upload_root.clone())?;
        for (idx, artifact_id) in request.artifacts.iter().enumerate() {
            let target = store.export_artifact_under(
                &account.id,
                artifact_id,
                &upload_root,
                &format!("{idx}-"),
            )?;
            files.push(target);
        }
        if !files.is_empty() {
            if !self.manifest.capabilities.file_uploads {
                return Err(ServiceError::new(
                    ErrorCode::UnsupportedCapability,
                    "upload",
                    format!(
                        "provider '{}' does not support file uploads",
                        self.manifest.id
                    ),
                ));
            }
            emit("uploading", json!({"count": files.len()}))?;
            tab.attach_files(&files, None::<&mut fn(&AttachmentProgress)>)
                .map_err(|e| ServiceError::new(ErrorCode::UploadFailed, "upload", e))?;
        }
        Ok(())
    }

    fn stream_turn(
        &self,
        tab: &mut WebChatTab,
        config: &WebChatConfig,
        account: &Account,
        session: &mut Session,
        prompt: &str,
        emit: Emit<'_>,
    ) -> Result<AskResult> {
        let mut emit_error = None;
        let provider_name = &self.manifest.name;
        let result = {
            let mut on_stream = |update: StreamUpdate<'_>| {
                if emit_error.is_some() {
                    return;
                }
                let res = match update {
                    StreamUpdate::Delta(text) => emit("text_delta", json!({"text": text})),
                    StreamUpdate::Thinking {
                        summary,
                        elapsed_secs,
                    } => {
                        let clean_summary = summary.trim();
                        let detail = if clean_summary.is_empty()
                            || clean_summary.eq_ignore_ascii_case("Thinking...")
                        {
                            format!("{provider_name} thinking ({elapsed_secs}s)")
                        } else {
                            format!("{provider_name} thinking ({elapsed_secs}s): {clean_summary}")
                        };
                        emit(
                            "browser_stage",
                            json!({
                                "stage": "thinking",
                                "detail": detail,
                                "summary": clean_summary,
                                "elapsed_secs": elapsed_secs,
                            }),
                        )
                    }
                    StreamUpdate::Answering { chars } => emit(
                        "browser_stage",
                        json!({
                            "stage": "generating",
                            "detail": format!("{provider_name} responding ({chars}c)"),
                            "chars": chars,
                        }),
                    ),
                };
                if let Err(e) = res {
                    emit_error = Some(e);
                }
            };
            tab.ask_with_stream(prompt, Some(&mut on_stream))
        };
        let result = match result {
            Ok(result) => result,
            Err(e) => {
                let err = ServiceError::backend(e, true);
                session.state = "outcome_unknown".into();
                if matches!(
                    err.code,
                    ErrorCode::Cancelled
                        | ErrorCode::DeadlineExceeded
                        | ErrorCode::SubmissionOutcomeUnknown
                ) {
                    if let Some(target) = &session.target_id {
                        let _ = WebChatTab::stop_owned(account.port, target, config);
                    }
                }
                return Err(err);
            }
        };
        if let Some(e) = emit_error {
            return Err(e);
        }
        Ok(result)
    }

    fn capture_media(store: &Store, tab: &mut WebChatTab, account: &str) -> Result<Vec<String>> {
        let root = store.root().join("media").join(id("download"));
        let _cleanup = TempDirGuard::new(root.clone())?;
        let paths = tab
            .download_generated_media(&root)
            .map_err(|e| ServiceError::new(ErrorCode::MediaDownloadFailed, "download", e))?;
        let mut artifacts = Vec::new();
        for path in paths {
            let bytes = std::fs::read(&path).map_err(ServiceError::storage)?;
            let mime = if bytes.starts_with(b"\x89PNG") {
                "image/png"
            } else if bytes.starts_with(&[0xff, 0xd8]) {
                "image/jpeg"
            } else {
                "image/webp"
            };
            artifacts.push(
                store
                    .artifact(account, &path, mime, &bytes, "generated")?
                    .id,
            );
        }
        Ok(artifacts)
    }
}

impl Provider for ManifestWebProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.manifest.id.clone(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            kind: self.manifest.kind.clone(),
            entry_url: self.manifest.entry_url.clone(),
            default_model: self.manifest.resolved_default_model(),
            source: if self.builtin { "builtin" } else { "runtime" }.into(),
            capabilities: self.manifest.capabilities.clone(),
            inline_prompt_chars: self.manifest.resolved_inline_prompt_chars(),
        }
    }

    fn default_login_url(&self) -> Option<&str> {
        self.manifest.entry_url.as_deref()
    }

    fn is_login_ready(&self, account: &Account, cancel: CancelToken) -> Result<bool> {
        if !self.manifest.capabilities.browser_auth {
            return Ok(true);
        }
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let Some(meta) = browser::inspect_running_cdp(account.port) else {
            return Ok(false);
        };
        if !meta.matches_account(account) {
            return Ok(false);
        }
        let Ok(config) = self.web_chat_config() else {
            return Ok(false);
        };
        let Ok(driver) = self.script("driver.js") else {
            return Ok(false);
        };
        let targets =
            browser::list_targets(account.port).map_err(|e| ServiceError::backend(e, false))?;
        for target in targets
            .into_iter()
            .filter(|t| {
                t.target_type == "page"
                    && config.url_matches_origin(&t.url)
                    && !config.is_auth_url(&t.url)
            })
            .take(4)
        {
            cancel
                .check()
                .map_err(|e| ServiceError::backend(e, false))?;
            let path = browser::ws_path_from_url(&target.ws_url).map_err(ServiceError::invalid)?;
            let mut cdp = CdpClient::connect(account.port, &path)
                .map_err(|e| ServiceError::backend(e, false))?;
            cdp.set_control(cancel.clone(), Instant::now() + Duration::from_secs(2));
            let result = cdp
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": format!("({})({})", driver, json!({"op": "probe"})),
                        "returnByValue": true,
                        "awaitPromise": true,
                        "timeout": 2000
                    }),
                )
                .map_err(|e| ServiceError::backend(e, false))?;
            let value = &result["result"]["value"];
            if result.get("exceptionDetails").is_none()
                && value["signed_in"] == true
                && value["editor"] == true
                && value["verification"] == false
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn discover_models(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: CancelToken,
    ) -> Result<ModelCatalogRecord> {
        if !self.manifest.capabilities.browser_auth
            || self.manifest.entry_url.is_none()
            || self.script("driver.js").is_err()
        {
            return Ok(self.default_catalog(!self.manifest.capabilities.browser_auth));
        }
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let config = self.web_chat_config()?;
        let opts = browser::LaunchOptions::headless_account(account, &config.entry_url);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let reused_target = ctx.store.and_then(|s| {
            web_chat::with_tab_pool_matcher(&config, |matcher| {
                tab_pool::find_system_target_for(s, &account.id, host.port, matcher)
            })
        });
        let (target, mode, created_new) = if let Some(target) = reused_target {
            (target, ChatSessionMode::Continue, false)
        } else {
            (
                browser::create_tab(host.port, &config.entry_url)
                    .map_err(|e| ServiceError::backend(e, false))?,
                ChatSessionMode::NewChat,
                true,
            )
        };
        let keep_in_pool = ctx
            .store
            .is_some_and(|s| tab_pool::record_system_tab_id(s, &account.id, &target.id));
        let id = target.id.clone();
        let deadline = Instant::now() + MODEL_DISCOVERY_TIMEOUT;
        if !created_new && !config.is_reusable_url(&target.url) {
            crate::gemini::navigate_tab_commit(
                host.port,
                &target,
                &config.entry_url,
                &cancel,
                deadline,
            )
            .map_err(|e| model_discovery_error(e, "prepare", deadline))?;
        }
        let result = (|| {
            let mut tab = WebChatTab::attach(
                host.port,
                target,
                mode,
                config.clone(),
                cancel.clone(),
                deadline,
                false,
            )
            .map_err(|e| model_discovery_error(e, "prepare", deadline))?;
            let auth = tab
                .check_auth_status()
                .map_err(|e| ServiceError::backend(e, false))?;
            if !auth.signed_in {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "probe",
                    format!(
                        "Sign in with /browser login or `anticrumb accounts login {}`. Model choices load automatically after sign-in.",
                        account.id
                    ),
                ));
            }
            let has_models_menu =
                self.manifest.requires_models_control || tab.initial_probe()["models"] == true;
            if !has_models_menu {
                cancel
                    .check()
                    .map_err(|e| ServiceError::backend(e, false))?;
                let _ = tab.into_target();
                return Ok(self.default_catalog(true));
            }
            let catalog = tab
                .model_catalog()
                .map_err(|e| model_discovery_error(e, "models", deadline))?;
            cancel
                .check()
                .map_err(|e| ServiceError::backend(e, false))?;
            let _ = tab.into_target();
            Ok(ModelCatalogRecord {
                provider: Some(self.manifest.id.clone()),
                adapter: Some(self.adapter_id()),
                checked_at: Some(now_ms()),
                signed_in: Some(true),
                chat: Some("probed".into()),
                temporary: Some(if self.manifest.capabilities.temporary_sessions {
                    "verify_on_use".into()
                } else {
                    "unsupported".into()
                }),
                uploads: Some(if self.manifest.capabilities.file_uploads {
                    "verify_on_use".into()
                } else {
                    "unsupported".into()
                }),
                images: Some(if self.manifest.capabilities.image_generation {
                    "verify_on_use".into()
                } else {
                    "unsupported".into()
                }),
                token_usage: Some("unavailable".into()),
                native_tools: Some(self.manifest.capabilities.native_tools),
                models: catalog.models,
                thinking: catalog.thinking,
                model_thinking: catalog.model_thinking,
                ..Default::default()
            })
        })();
        if created_new && !keep_in_pool {
            if ctx.store.is_some() {
                let _ = browser::close_tab(host.port, &id);
            } else if let Ok(targets) = browser::list_targets(host.port) {
                let provider_count = targets
                    .iter()
                    .filter(|t| t.target_type == "page" && config.is_reusable_url(&t.url))
                    .count();
                if provider_count > 1 {
                    let _ = browser::close_tab(host.port, &id);
                }
            }
        }
        result
    }

    fn generate(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        let store = ctx.store.ok_or_else(|| {
            ServiceError::new(
                ErrorCode::Internal,
                "provider",
                "provider operation requires persistent store context",
            )
        })?;
        let config = self.web_chat_config()?;
        self.require_web_turn_scripts()?;
        if request.mode == SessionMode::Temporary && !self.manifest.capabilities.temporary_sessions
        {
            return Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "prepare",
                format!(
                    "provider '{}' does not support temporary sessions",
                    self.manifest.id
                ),
            ));
        }
        let start = Instant::now();
        let deadline = start + Duration::from_secs(request.timeout_secs);
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let (mut tab, is_continue, same_warm_tab) = self.acquire_tab(
            store, &config, account, session, request, &cancel, deadline, emit,
        )?;
        tab.set_pace(load_human_pace(Some(store)));
        if tab.initial_probe()["signed_in"] != true
            && !tab
                .check_auth_status()
                .map_err(|e| ServiceError::backend(e, false))?
                .signed_in
        {
            return Err(ServiceError::new(
                ErrorCode::AuthRequired,
                "prepare",
                "account is signed out; use accounts login and accounts resolve",
            ));
        }
        if is_continue {
            if let Some(count) = tab.initial_probe()["response_count"].as_u64() {
                if count != session.remote_turns {
                    return Err(ServiceError::backend(
                        "UI contract changed: owned conversation was modified outside this service"
                            .into(),
                        false,
                    ));
                }
            } else {
                tab.verify_owned_turns(session.remote_turns)
                    .map_err(|e| ServiceError::backend(e, false))?;
            }
        }
        self.prepare_tab(
            store,
            &mut tab,
            account,
            session,
            request,
            same_warm_tab,
            emit,
        )?;
        let prompt = crate::protocol::prompt(request)?;
        emit(
            "prompt_size",
            json!({"chars": prompt.chars().count(), "tool_contract_omitted": request.omit_tool_contract}),
        )?;
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        emit(
            "submitted",
            json!({"session_id": session.id, "chars": prompt.chars().count()}),
        )?;
        let result = self.stream_turn(&mut tab, &config, account, session, &prompt, emit)?;
        session.remote_url = Some(result.chat_url.clone());
        session.remote_turns += 1;
        let (text, tool_calls) = crate::protocol::decode(&result.response, request)?;
        let mut artifacts = Vec::new();
        if request.generate_image {
            if !self.manifest.capabilities.image_generation {
                return Err(ServiceError::new(
                    ErrorCode::UnsupportedCapability,
                    "download",
                    format!(
                        "provider '{}' does not support image generation",
                        self.manifest.id
                    ),
                ));
            }
            emit("downloading", json!({}))?;
            artifacts =
                Self::capture_media(store, &mut tab, &account.id).map_err(|mut error| {
                    error.code = ErrorCode::MediaDownloadFailed;
                    error.stage = "download".into();
                    error.retry = "download_only".into();
                    error
                })?;
        }
        let _ = tab.into_target();
        Ok(TurnResult {
            session_id: session.id.clone(),
            text,
            tool_calls,
            artifacts,
            model: result.model,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    fn recover_media(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &Session,
    ) -> Result<Vec<String>> {
        if !self.manifest.capabilities.image_generation {
            return Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "download",
                format!(
                    "provider '{}' does not support media recovery",
                    self.manifest.id
                ),
            ));
        }
        let store = ctx.store.ok_or_else(|| {
            ServiceError::new(
                ErrorCode::Internal,
                "provider",
                "provider operation requires persistent store context",
            )
        })?;
        let config = self.web_chat_config()?;
        let owned = crate::tab_pool::owned_media_target(
            store,
            session,
            browser::list_targets(account.port).unwrap_or_default(),
        )?;
        let (target, created) = if let Some(target) = owned {
            (target, false)
        } else {
            let url = session
                .remote_url
                .as_deref()
                .ok_or_else(|| ServiceError::invalid("no saved remote conversation"))?;
            if !config.url_matches_origin(url) || !config.is_chat_url(url) {
                return Err(ServiceError::invalid(
                    "this temporary or unsaved conversation cannot be reopened",
                ));
            }
            (
                browser::create_tab(account.port, url)
                    .map_err(|e| ServiceError::backend(e, false))?,
                true,
            )
        };
        let _close = browser::CloseTabGuard {
            port: account.port,
            target_id: created.then(|| target.id.clone()),
        };
        let mut tab = WebChatTab::attach(
            account.port,
            target,
            ChatSessionMode::Continue,
            config,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
            true,
        )
        .map_err(|e| ServiceError::backend(e, false))?;
        tab.wait_owned_response(session.remote_turns)
            .map_err(|e| ServiceError::backend(e, false))?;
        Self::capture_media(store, &mut tab, &account.id)
    }

    fn delete_conversation(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &Session,
        cancel: CancelToken,
    ) -> Result<()> {
        history::delete(self, ctx, account, session, cancel)
    }

    fn clear_web_history(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: CancelToken,
        progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        history::clear(self, ctx, account, cancel, progress)
    }

    fn read_usage(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: CancelToken,
    ) -> Result<WebUsageReport> {
        let (Some(usage_url), true) = (
            self.manifest.usage_url.as_deref(),
            self.manifest.capabilities.web_usage && self.script("usage.js").is_ok(),
        ) else {
            return Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "usage",
                format!(
                    "provider '{}' does not expose account usage",
                    self.manifest.id
                ),
            ));
        };
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let config = self.web_chat_config()?;
        let opts = browser::LaunchOptions::headless_account(account, &config.entry_url);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let reserved = ctx.store.and_then(|s| {
            web_chat::with_tab_pool_matcher(&config, |matcher| {
                tab_pool::reserve_system_target_for(s, &account.id, host.port, matcher)
            })
        });
        let (target, created_new, _lease) = if let Some((t, lease)) = reserved {
            (t, false, lease)
        } else {
            let created = browser::create_tab(host.port, usage_url)
                .map_err(|e| ServiceError::backend(e, false))?;
            let lease = tab_pool::SystemTabLease::acquire(created.id.clone());
            (created, true, lease)
        };
        let keep_persistent = ctx
            .store
            .is_some_and(|s| tab_pool::record_system_tab_id(s, &account.id, &target.id));
        let deadline = Instant::now() + WEB_USAGE_TIMEOUT;
        if !created_new {
            crate::gemini::navigate_tab_commit(host.port, &target, usage_url, &cancel, deadline)
                .map_err(|message| {
                    let mut error = ServiceError::backend(message, false);
                    error.stage = "usage".into();
                    error
                })?;
        }
        let result =
            web_chat::read_web_usage(host.port, target.clone(), &config, cancel.clone(), deadline);
        if created_new && !keep_persistent {
            let _ = browser::close_tab(host.port, &target.id);
        }
        let value = result.map_err(|message| {
            let mut error = ServiceError::backend(message, false);
            error.stage = "usage".into();
            error
        })?;
        if !value.is_object() {
            return Err(ServiceError::new(
                ErrorCode::UiContractChanged,
                "usage",
                "UI contract changed: usage page did not return an account usage report",
            ));
        }
        let mut report: WebUsageReport =
            serde_json::from_value(value).map_err(ServiceError::storage)?;
        report.account_id = account.id.clone();
        report.checked_at = now_ms();
        Ok(report)
    }

    fn close_session(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &Session,
    ) -> Result<()> {
        let (Some(store), Ok(config)) = (ctx.store, self.web_chat_config()) else {
            return Ok(());
        };
        web_chat::with_tab_pool_matcher(&config, |matcher| {
            tab_pool::release_session_tab_for(store, account, session, matcher)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderRegistry;
    use std::fs;

    fn sample_claude_manifest() -> ProviderManifest {
        ProviderManifest {
            id: "claude-web".into(),
            name: "Claude Web".into(),
            version: "v1".into(),
            kind: "web_provider".into(),
            entry_url: Some("https://claude.ai/new".into()),
            allowed_origins: vec!["https://claude.ai".into()],
            auth_url_substrings: vec!["claude.ai/login".into()],
            chat_url_prefixes: vec!["https://claude.ai/chat/".into()],
            reusable_path_prefixes: vec!["/new".into(), "/chat/".into()],
            usage_url: Some("https://claude.ai/settings/usage".into()),
            default_model: Some("claude-web-default".into()),
            capabilities: ProviderCapabilities {
                browser_auth: true,
                temporary_sessions: true,
                file_uploads: true,
                web_usage: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn manifest_validates_webchat_routing_fields() {
        let mut m = sample_claude_manifest();
        m.observer_binding = Some("valid_binding_1".into());
        assert!(m.validate().is_ok());

        let mut bad_binding = m.clone();
        bad_binding.observer_binding = Some("123-bad".into());
        assert!(bad_binding.validate().is_err());

        let mut bad_prefix = m.clone();
        bad_prefix.reusable_path_prefixes = vec!["no-leading-slash".into()];
        assert!(bad_prefix.validate().is_err());

        let mut bad_usage = m;
        bad_usage.usage_url = Some("javascript:alert(1)".into());
        assert!(bad_usage.validate().is_err());
    }

    #[test]
    fn webchat_config_matches_origins_auth_chat_and_reusable_urls() {
        let provider = ManifestWebProvider::new(sample_claude_manifest()).unwrap();
        let cfg = provider.web_chat_config().unwrap();

        assert!(cfg.url_matches_origin("https://claude.ai/new"));
        assert!(!cfg.url_matches_origin("https://claude.ai.example.invalid/new"));
        assert!(cfg.is_auth_url("https://claude.ai/login?returnTo=%2Fnew"));
        assert!(!cfg.is_auth_url("https://claude.ai/new"));

        assert!(cfg.is_chat_url("https://claude.ai/chat/abc-123"));
        assert!(!cfg.is_chat_url("https://claude.ai/chat/"));
        assert!(!cfg.is_chat_url("https://claude.ai/new"));

        assert!(cfg.is_system_page_url("https://claude.ai/settings/usage"));
        assert!(!cfg.is_system_page_url("https://claude.ai/new"));

        assert!(cfg.is_reusable_url("https://claude.ai/new"));
        assert!(cfg.is_reusable_url("https://claude.ai/chat/abc-123"));
        assert!(!cfg.is_reusable_url("https://claude.ai/login"));
        assert!(!cfg.is_reusable_url("https://claude.ai/settings/usage"));
    }

    #[test]
    fn bundled_provider_can_override_one_dom_file_and_use_remaining_embedded_scripts() {
        for id in ["claude-web", "chatgpt-web"] {
            let mut manifest: ProviderManifest =
                serde_json::from_str(super::super::bundled::manifest(id).unwrap()).unwrap();
            let dir = tempfile::tempdir().unwrap();
            manifest.scripts_dir = Some(dir.path().to_path_buf());
            let provider = ManifestWebProvider::new(manifest).unwrap();
            let original = provider.script("observer.js").unwrap();
            fs::write(dir.path().join("driver.js"), "// updated DOM").unwrap();
            assert_eq!(provider.script("driver.js").unwrap(), "// updated DOM");
            assert_eq!(provider.script("observer.js").unwrap(), original);
            fs::write(dir.path().join("driver.js"), "// hot DOM update").unwrap();
            assert_eq!(provider.script("driver.js").unwrap(), "// hot DOM update");
        }
    }

    #[test]
    fn manifest_web_provider_loads_disk_directory_scripts_without_recompile() {
        let dir = tempfile::tempdir().unwrap();
        let provider_dir = dir.path().join("chatgpt-web");
        let scripts_dir = provider_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("driver.js"), "(args => ({op: args.op}))").unwrap();
        std::fs::write(scripts_dir.join("observer.js"), "(() => true)()").unwrap();
        std::fs::write(scripts_dir.join("extract-response.js"), "(() => ({}))()").unwrap();

        let manifest = ProviderManifest {
            id: "chatgpt-web".into(),
            name: "ChatGPT Web".into(),
            version: "v1".into(),
            kind: "web_provider".into(),
            entry_url: Some("https://chatgpt.com/".into()),
            allowed_origins: vec!["https://chatgpt.com".into()],
            auth_url_substrings: vec!["auth.openai.com".into()],
            chat_url_prefixes: vec!["https://chatgpt.com/c/".into()],
            default_model: Some("chatgpt-web-default".into()),
            capabilities: ProviderCapabilities {
                browser_auth: true,
                temporary_sessions: true,
                file_uploads: true,
                image_generation: true,
                ..Default::default()
            },
            ..Default::default()
        };
        std::fs::write(
            provider_dir.join("provider.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let registry = ProviderRegistry::with_builtins();
        let loaded = registry.load_providers_dir(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "chatgpt-web");
        assert!(registry.resolve("chatgpt-web").is_ok());

        registry.unregister("chatgpt-web").unwrap();
        assert!(!registry.contains("chatgpt-web"));
    }
}
