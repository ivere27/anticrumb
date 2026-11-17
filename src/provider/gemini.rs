use super::{Provider, ProviderCapabilities, ProviderContext, ProviderInfo, DEFAULT_PROVIDER_ID};
use crate::{
    account::{MODEL_DISCOVERY_TIMEOUT, WEB_USAGE_TIMEOUT},
    agent::CancelToken,
    browser_host as browser,
    cdp::CdpClient,
    domain::*,
    gemini::{ChatSessionMode, GeminiTab},
    service::{Emit, TempDirGuard},
    store::Store,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const DEFAULT_MODEL_ID: &str = "gemini-web-default";

pub struct GeminiWebProvider {
    pub inline_prompt_chars: u32,
    pub(crate) source: &'static str,
}

impl Default for GeminiWebProvider {
    fn default() -> Self {
        Self {
            inline_prompt_chars: super::DEFAULT_INLINE_PROMPT_CHARS,
            source: "builtin",
        }
    }
}

impl Provider for GeminiWebProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: DEFAULT_PROVIDER_ID.into(),
            name: "Gemini Web".into(),
            version: "v1".into(),
            kind: "builtin_web".into(),
            entry_url: Some(browser::GEMINI_APP_URL.into()),
            default_model: DEFAULT_MODEL_ID.into(),
            source: self.source.into(),
            inline_prompt_chars: self.inline_prompt_chars,
            capabilities: ProviderCapabilities {
                browser_auth: true,
                temporary_sessions: true,
                file_uploads: true,
                image_generation: true,
                web_usage: true,
                web_history_cleanup: true,
                native_tools: false,
            },
        }
    }

    fn default_login_url(&self) -> Option<&str> {
        Some(browser::GEMINI_APP_URL)
    }

    fn is_login_ready(&self, account: &Account, cancel: CancelToken) -> Result<bool> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let Some(meta) = browser::inspect_running_cdp(account.port) else {
            return Ok(false);
        };
        if !meta.matches_account(account) {
            return Ok(false);
        }
        let targets =
            browser::list_targets(account.port).map_err(|e| ServiceError::backend(e, false))?;
        // The tab opened for login may navigate through accounts.google.com. Wait
        // until it returns to Gemini; never inject a script into the sign-in form.
        for target in targets
            .into_iter()
            .filter(|target| {
                target.target_type == "page" && crate::gemini::is_gemini_page_url(&target.url)
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
                        "expression": format!(
                            "location.origin === 'https://gemini.google.com' ? ({})({}) : null",
                            crate::adapter::script_for(DEFAULT_PROVIDER_ID, "driver.js")
                                .map_err(ServiceError::invalid)?,
                            json!({"op":"probe"})
                        ),
                        "returnByValue": true,
                        "awaitPromise": true,
                        "timeout": 2000
                    }),
                )
                .map_err(|e| ServiceError::backend(e, false))?;
            if result.get("exceptionDetails").is_none() && authenticated(&result["result"]["value"])
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
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let opts = browser::LaunchOptions::headless_account(account, browser::GEMINI_APP_URL);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let reused_target = ctx
            .store
            .and_then(|s| crate::tab_pool::find_system_gemini_target(s, &account.id, host.port));
        let (target, mode, created_new) = if let Some(target) = reused_target {
            (target, ChatSessionMode::Continue, false)
        } else {
            (
                browser::create_tab(host.port, browser::GEMINI_APP_URL)
                    .map_err(|e| ServiceError::backend(e, false))?,
                ChatSessionMode::NewChat,
                true,
            )
        };
        let keep_in_pool = ctx
            .store
            .is_some_and(|s| crate::tab_pool::record_system_tab_id(s, &account.id, &target.id));
        let id = target.id.clone();
        // A cold page can expose the editor well before its model controls. Page
        // preparation and the full model scan share this budget without restarting it.
        let deadline = Instant::now() + MODEL_DISCOVERY_TIMEOUT;
        if !created_new && !crate::gemini::is_reusable_gemini_url(&target.url) {
            crate::gemini::navigate_tab_commit(
                host.port,
                &target,
                browser::GEMINI_APP_URL,
                &cancel,
                deadline,
            )
            .map_err(|e| model_discovery_error(e, "prepare", deadline))?;
        }
        let result = (|| {
            let mut tab =
                GeminiTab::attach(host.port, target, mode, cancel.clone(), deadline, false)
                    .map_err(|e| model_discovery_error(e, "prepare", deadline))?;
            let auth = tab
                .check_auth_status()
                .map_err(|e| ServiceError::backend(e, false))?;
            if !auth.signed_in {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "probe",
                    format!("Sign in with /browser login or `anticrumb accounts login {}`. Model choices load automatically after sign-in.", account.id),
                ));
            }
            let catalog = tab
                .model_catalog()
                .map_err(|e| model_discovery_error(e, "models", deadline))?;
            let capabilities = ModelCatalogRecord {
                provider: Some(DEFAULT_PROVIDER_ID.into()),
                adapter: Some(self.adapter_id()),
                account_id: None,
                server_id: None,
                checked_at: Some(now_ms()),
                signed_in: Some(true),
                chat: Some("probed".into()),
                temporary: Some("verify_on_use".into()),
                uploads: Some("verify_on_use".into()),
                images: Some("verify_on_use".into()),
                notebooks: Some("exported_sources_only".into()),
                token_usage: Some("unavailable".into()),
                native_tools: Some(false),
                models: catalog.models,
                thinking: catalog.thinking,
                model_thinking: catalog.model_thinking,
            };
            cancel
                .check()
                .map_err(|e| ServiceError::backend(e, false))?;
            let _ = tab.into_target();
            Ok(capabilities)
        })();
        if created_new && !keep_in_pool {
            if ctx.store.is_some() {
                let _ = browser::close_tab(host.port, &id);
            } else if let Ok(targets) = browser::list_targets(host.port) {
                let gemini_count = targets
                    .iter()
                    .filter(|t| {
                        t.target_type == "page" && crate::gemini::is_reusable_gemini_url(&t.url)
                    })
                    .count();
                if gemini_count > 1 {
                    let _ = browser::close_tab(host.port, &id);
                }
            }
        }
        result
    }

    fn read_usage(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: CancelToken,
    ) -> Result<WebUsageReport> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let opts = browser::LaunchOptions::headless_account(account, browser::GEMINI_APP_URL);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let mut tab = crate::gemini::SystemGeminiTab::acquire_for_store(
            ctx.store,
            &account.id,
            host.port,
            "https://gemini.google.com/usage",
        )
        .map_err(|e| ServiceError::backend(e, false))?;
        let deadline = Instant::now() + WEB_USAGE_TIMEOUT;
        if !tab.created_new {
            crate::gemini::navigate_tab_commit(
                host.port,
                &tab.target,
                "https://gemini.google.com/usage",
                &cancel,
                deadline,
            )
            .map_err(|message| {
                let mut error = ServiceError::backend(message, false);
                error.stage = "usage".into();
                error
            })?;
        }
        let result =
            crate::gemini::read_web_usage(host.port, tab.target.clone(), cancel.clone(), deadline);
        let keep_warm = if tab.keep_persistent || !tab.created_new {
            true
        } else {
            browser::list_targets(host.port).is_ok_and(|targets| {
                !targets.iter().any(|t| {
                    t.id != tab.target.id
                        && t.target_type == "page"
                        && crate::gemini::is_reusable_gemini_url(&t.url)
                })
            })
        };
        if keep_warm && !cancel.is_cancelled() {
            tab.keep_persistent = true;
            let return_deadline = Instant::now() + Duration::from_secs(4);
            let _ = crate::gemini::navigate_tab_commit(
                host.port,
                &tab.target,
                browser::GEMINI_APP_URL,
                &cancel,
                return_deadline,
            );
        }
        drop(tab);
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

    fn clear_web_history(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: CancelToken,
        progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        crate::gemini::clear_web_history_with_store(ctx.store, account, cancel, progress)
    }

    fn delete_conversation(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &Session,
        cancel: CancelToken,
    ) -> Result<()> {
        crate::gemini::delete_web_conversation_with_store(ctx.store, account, session, cancel)
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
        let store = require_store(ctx)?;
        let start = Instant::now();
        let deadline = start + Duration::from_secs(request.timeout_secs);
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let (mut tab, is_continue, same_warm_tab) =
            acquire_tab(store, account, session, request, &cancel, deadline, emit)?;
        tab.set_pace(crate::gemini::load_human_pace(Some(store)));
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
        prepare_tab(
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
            json!({"chars":prompt.chars().count(),"tool_contract_omitted":request.omit_tool_contract}),
        )?;
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        emit(
            "submitted",
            json!({"session_id":session.id,"chars":prompt.chars().count()}),
        )?;
        let result = stream_turn(&mut tab, account, session, &prompt, emit)?;
        session.remote_url = Some(result.chat_url.clone());
        session.remote_turns += 1;
        let (text, tool_calls) = crate::protocol::decode(&result.response, request)?;
        let mut artifacts = Vec::new();
        if request.generate_image {
            emit("downloading", json!({}))?;
            artifacts = capture_media(store, &mut tab, &account.id).map_err(|mut error| {
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
        let store = require_store(ctx)?;
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
            let parsed = crate::url_lite::Url::parse(url).map_err(ServiceError::storage)?;
            if !crate::gemini::is_gemini_origin(&parsed) || !crate::gemini::url_has_chat_id(url) {
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
        let mut tab = GeminiTab::attach(
            account.port,
            target,
            ChatSessionMode::Continue,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
            true,
        )
        .map_err(|e| ServiceError::backend(e, false))?;
        tab.wait_owned_response(session.remote_turns)
            .map_err(|e| ServiceError::backend(e, false))?;
        capture_media(store, &mut tab, &account.id)
    }

    fn close_session(
        &self,
        ctx: &ProviderContext<'_>,
        account: &Account,
        session: &Session,
    ) -> Result<()> {
        let store = require_store(ctx)?;
        crate::tab_pool::release_session_tab(store, account, session)
    }
}

fn require_store<'a>(ctx: &ProviderContext<'a>) -> Result<&'a Store> {
    ctx.store.ok_or_else(|| {
        ServiceError::new(
            ErrorCode::Internal,
            "provider",
            "provider operation requires persistent store context",
        )
    })
}

pub(crate) fn authenticated(value: &Value) -> bool {
    value["url"]
        .as_str()
        .is_some_and(crate::gemini::is_gemini_page_url)
        && value["signed_in"] == true
        && value["editor"] == true
        && value["verification"] == false
}

pub(crate) fn model_discovery_error(
    message: String,
    stage: &str,
    deadline: Instant,
) -> ServiceError {
    let mut error = ServiceError::backend(message, false);
    error.stage = stage.into();
    if error.code == ErrorCode::DeadlineExceeded {
        let condition = if stage == "prepare" {
            "Browser editor/model controls were not ready"
        } else {
            "Model discovery did not finish"
        };
        error.message = if Instant::now() >= deadline {
            format!(
                "{condition} within {} seconds: {}",
                MODEL_DISCOVERY_TIMEOUT.as_secs(),
                error.message
            )
        } else {
            format!(
                "{condition}; a browser operation timed out: {}",
                error.message
            )
        };
    } else if stage == "models" && error.code != ErrorCode::Cancelled {
        error.code = ErrorCode::UiContractChanged;
        error.retry = "after_adapter_update".into();
    }
    error
}

pub(crate) fn unique_model_aliases(catalog: Option<&Value>, query: &str) -> Vec<String> {
    let Some(models) = catalog.and_then(|c| c["models"].as_array()) else {
        return vec![query.to_owned()];
    };
    let Some(idx) = models.iter().position(|m| {
        m["mode_id"].as_str() == Some(query)
            || m["label"].as_str() == Some(query)
            || m["sublabel"].as_str() == Some(query)
    }) else {
        return vec![query.to_owned()];
    };
    let other_norms: std::collections::HashSet<String> = models
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .flat_map(|(_, m)| {
            ["mode_id", "label", "sublabel"]
                .into_iter()
                .filter_map(|k| m[k].as_str())
                .map(crate::gemini::normalize_model_label)
        })
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::new();
    for field in ["mode_id", "label", "sublabel"] {
        if let Some(val) = models[idx][field]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let norm = crate::gemini::normalize_model_label(val);
            if !other_norms.contains(&norm) && !out.iter().any(|existing| existing == val) {
                out.push(val.to_owned());
            }
        }
    }
    out
}

fn acquire_tab(
    store: &Store,
    account: &Account,
    session: &mut Session,
    request: &TurnRequest,
    cancel: &CancelToken,
    deadline: Instant,
    emit: Emit<'_>,
) -> Result<(GeminiTab, bool, bool)> {
    let opts = browser::LaunchOptions::headless_account(account, browser::GEMINI_APP_URL);
    let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
    let other_owned: std::collections::HashSet<String> = store
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
            json!({"stage":"checking_tab","detail":format!("verifying owned tab {}", target.id)}),
        )?;
        let mut navigated = false;
        if let Some(expected) = &session.remote_url {
            let expected_url =
                crate::url_lite::Url::parse(expected).map_err(ServiceError::storage)?;
            let current_url =
                crate::url_lite::Url::parse(&target.url).map_err(ServiceError::storage)?;
            if expected_url.host_str() != current_url.host_str()
                || expected_url.path() != current_url.path()
            {
                if session.mode == SessionMode::Chat {
                    emit(
                        "browser_stage",
                        json!({"stage":"navigating_tab","detail":"restoring saved Gemini conversation URL"}),
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
                    json!({"stage":"navigating_tab","detail":"acquiring tab & opening saved Gemini chat"}),
                )?;
                let (target, _) = crate::tab_pool::acquire_session_tab(
                    store,
                    &account.id,
                    host.port,
                    &session.id,
                    &SessionMode::Chat,
                )?;
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
            json!({"stage":"acquiring_tab","detail":format!("opening {:?} Gemini tab", request.mode)}),
        )?;
        let mode = if request.mode == SessionMode::Temporary {
            ChatSessionMode::TempChat
        } else {
            ChatSessionMode::NewChat
        };
        let (target, reused) = crate::tab_pool::acquire_session_tab(
            store,
            &account.id,
            host.port,
            &session.id,
            &request.mode,
        )?;
        (target, mode, reused)
    };
    session.target_id = Some(target.id.clone());
    store.put("local", "session", &session.id, session)?;
    let mut tab = if reused {
        GeminiTab::attach_reused(
            host.port,
            target,
            mode,
            cancel.clone(),
            deadline,
            request.generate_image,
        )
    } else {
        GeminiTab::attach(
            host.port,
            target,
            mode,
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

fn prepare_tab(
    store: &Store,
    tab: &mut GeminiTab,
    account: &Account,
    session: &Session,
    request: &TurnRequest,
    same_warm_tab: bool,
    emit: Emit<'_>,
) -> Result<()> {
    let initial_probe = tab.initial_probe().clone();
    let mut requested_model = request.model.as_deref().filter(|m| {
        *m != DEFAULT_MODEL_ID && (!same_warm_tab || session.model.as_deref() != Some(*m))
    });
    let mut requested_thinking = request
        .thinking
        .as_deref()
        .filter(|t| !same_warm_tab || session.thinking.as_deref() != Some(*t));
    if let Some(model_query) = requested_model {
        let catalog = store.model_catalog(&account.id).ok();
        let aliases = unique_model_aliases(catalog.as_ref(), model_query);
        if crate::gemini::probe_matches_model_aliases(&initial_probe, &aliases) {
            requested_model = None;
        }
    }
    if let Some(thinking_query) = requested_thinking {
        if crate::gemini::probe_matches_thinking(&initial_probe, thinking_query) {
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
        &json!({"model":model,"at":now_ms()}),
    )?;
    emit("model_info", json!({"model":model}))?;
    let mut files = Vec::new();
    let upload_root = store.root().join("uploads").join(id("turn"));
    let _cleanup = TempDirGuard::new(upload_root.clone())?;
    for artifact_id in &request.artifacts {
        let (art, bytes) = store.artifact_bytes(&account.id, artifact_id)?;
        let target = upload_root.join(format!("{}-{}", files.len(), art.name));
        std::fs::write(&target, bytes).map_err(ServiceError::storage)?;
        files.push(target);
    }
    if !files.is_empty() {
        emit("uploading", json!({"count":files.len()}))?;
        tab.attach_files(&files, None::<&mut fn(&crate::gemini::AttachmentProgress)>)
            .map_err(|e| ServiceError::new(ErrorCode::UploadFailed, "upload", e))?;
    }
    Ok(())
}

fn stream_turn(
    tab: &mut GeminiTab,
    account: &Account,
    session: &mut Session,
    prompt: &str,
    emit: Emit<'_>,
) -> Result<crate::gemini::AskResult> {
    let mut emit_error = None;
    let result = {
        let mut on_stream = |update: crate::gemini::StreamUpdate<'_>| {
            if emit_error.is_some() {
                return;
            }
            let res = match update {
                crate::gemini::StreamUpdate::Delta(text) => {
                    emit("text_delta", json!({"text": text}))
                }
                crate::gemini::StreamUpdate::Thinking {
                    summary,
                    elapsed_secs,
                } => {
                    let clean_summary = summary.trim();
                    let detail = if clean_summary.is_empty()
                        || clean_summary.eq_ignore_ascii_case("Thinking...")
                    {
                        format!("Gemini Web thinking ({elapsed_secs}s)")
                    } else {
                        format!("Gemini Web thinking ({elapsed_secs}s): {clean_summary}")
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
                crate::gemini::StreamUpdate::Answering { chars } => emit(
                    "browser_stage",
                    json!({
                        "stage": "generating",
                        "detail": format!("Gemini Web responding ({chars}c)"),
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
                    let _ = GeminiTab::stop_owned(account.port, target);
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

fn capture_media(store: &Store, tab: &mut GeminiTab, account: &str) -> Result<Vec<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_model_aliases_matches_labels_and_drops_ambiguous_names() {
        let catalog = json!({
            "models": [
                {"mode_id": "56fdd199312815e2", "label": "Fast mode", "sublabel": "Gemini 2.5 Flash"},
                {"mode_id": "e051ce1aa80aa576", "label": "Pro", "sublabel": "Gemini 2.5 Pro"},
                {"mode_id": "legacy-pro", "label": "Pro", "sublabel": "2.5 Pro Legacy"}
            ]
        });
        assert_eq!(
            unique_model_aliases(Some(&catalog), "56fdd199312815e2"),
            vec![
                "56fdd199312815e2".to_string(),
                "Fast mode".to_string(),
                "Gemini 2.5 Flash".to_string()
            ]
        );
        assert_eq!(
            unique_model_aliases(Some(&catalog), "e051ce1aa80aa576"),
            vec!["e051ce1aa80aa576".to_string(), "Gemini 2.5 Pro".to_string()]
        );
        assert_eq!(unique_model_aliases(None, "Fast"), vec!["Fast".to_string()]);
    }

    #[test]
    fn catalog_deadline_names_the_waited_condition_without_hiding_cancellation() {
        let elapsed = Instant::now();
        let error = model_discovery_error("deadline exceeded".into(), "prepare", elapsed);
        assert_eq!(error.code, ErrorCode::DeadlineExceeded);
        assert_eq!(error.stage, "prepare");
        assert!(error.message.contains("editor/model controls"));
        assert!(error.message.contains("within 120 seconds"));

        let error = model_discovery_error("deadline exceeded".into(), "models", elapsed);
        assert_eq!(error.code, ErrorCode::DeadlineExceeded);
        assert_eq!(error.stage, "models");
        assert!(error.message.contains("Model discovery did not finish"));

        let early = model_discovery_error(
            "CDP deadline exceeded".into(),
            "prepare",
            Instant::now() + MODEL_DISCOVERY_TIMEOUT,
        );
        assert!(early.message.contains("browser operation timed out"));
        assert!(!early.message.contains("within 120 seconds"));

        let cancelled = model_discovery_error("cancelled".into(), "models", elapsed);
        assert_eq!(cancelled.code, ErrorCode::Cancelled);
        assert_eq!(cancelled.message, "cancelled");
    }

    #[test]
    fn authentication_requires_gemini_origin_and_no_verification_challenge() {
        let good = json!({"url":"https://gemini.google.com/app", "signed_in":true,"editor":true,"models":false,"verification":false});
        assert!(authenticated(&good));
        for (key, value) in [
            ("url", json!("https://accounts.google.com/")),
            ("url", json!("https://gemini.google.com.example.invalid/")),
            ("signed_in", json!(false)),
            ("editor", json!(false)),
            ("verification", json!(true)),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(!authenticated(&bad));
        }
    }
}
