use super::{
    AccountRoute, DiscoveredAccount, Provider, ProviderCapabilities, ProviderContext, ProviderInfo,
    DEFAULT_PROVIDER_ID,
};
use crate::{
    account::{ACCOUNT_DISCOVERY_TIMEOUT, MODEL_DISCOVERY_TIMEOUT, WEB_USAGE_TIMEOUT},
    agent::CancelToken,
    browser_host as browser,
    cdp::CdpClient,
    domain::*,
    gemini::{ChatSessionMode, GeminiTab},
    service::{Emit, TempDirGuard},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

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
                multi_account: true,
            },
        }
    }

    fn default_login_url(&self) -> Option<&str> {
        Some(browser::GEMINI_APP_URL)
    }

    fn account_route(&self, account: &Account) -> AccountRoute {
        let slot = crate::gemini::normalized_slot(account.slot.as_deref());
        AccountRoute {
            entry_url: crate::gemini::gemini_app_url(Some(slot)),
            usage_url: Some(crate::gemini::gemini_usage_url(Some(slot))),
            scope: Some(slot.to_owned()),
        }
    }

    fn rebase_chat_url(&self, account: &Account, remote_url: &str) -> Result<String> {
        crate::gemini::rebase_gemini_chat_url(remote_url, account.slot.as_deref())
            .map_err(|_| ServiceError::invalid("invalid recorded Gemini conversation URL"))
    }

    fn is_login_ready(
        &self,
        browser_profile: &BrowserProfile,
        cancel: CancelToken,
    ) -> Result<bool> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let Some(meta) = browser::inspect_running_cdp(browser_profile.port) else {
            return Ok(false);
        };
        if !meta.matches_browser(browser_profile) {
            return Ok(false);
        }
        let targets = browser::list_targets(browser_profile.port)
            .map_err(|e| ServiceError::backend(e, false))?;
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
            let mut cdp = CdpClient::connect(browser_profile.port, &path)
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

    fn discover_accounts(
        &self,
        _ctx: &ProviderContext<'_>,
        browser_profile: &BrowserProfile,
        cancel: CancelToken,
    ) -> Result<Vec<DiscoveredAccount>> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let opts = browser::LaunchOptions::headless_browser(
            browser_profile,
            "https://gemini.google.com/u/0/app",
        );
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        discover_accounts_on_port(host.port, cancel, Duration::from_secs(8))
    }

    fn discover_models(
        &self,
        ctx: &ProviderContext<'_>,
        browser_profile: &BrowserProfile,
        account: &Account,
        cancel: CancelToken,
    ) -> Result<ModelCatalogRecord> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let route = self.account_route(account);
        let opts = browser::LaunchOptions::headless_browser(browser_profile, &route.entry_url);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let reused_target = ctx.store.and_then(|s| {
            crate::tab_pool::find_system_gemini_target_for_route(s, &account.id, host.port, &route)
        });
        let (target, mode, created_new) = if let Some(target) = reused_target {
            (target, ChatSessionMode::Continue, false)
        } else {
            (
                browser::create_tab(host.port, &route.entry_url)
                    .map_err(|e| ServiceError::backend(e, false))?,
                ChatSessionMode::NewChat,
                true,
            )
        };
        let keep_in_pool = ctx.store.is_some_and(|s| {
            crate::tab_pool::record_system_tab_id(s, &account.id, host.port, &target.id)
        });
        let id = target.id.clone();
        // A cold page can expose the editor well before its model controls. Page
        // preparation and the full model scan share this budget without restarting it.
        let deadline = Instant::now() + MODEL_DISCOVERY_TIMEOUT;
        if !created_new
            && !crate::gemini::is_reusable_gemini_url_for(&target.url, route.scope.as_deref())
        {
            crate::gemini::navigate_tab_commit(
                host.port,
                &target,
                &route.entry_url,
                &cancel,
                deadline,
            )
            .map_err(|e| model_discovery_error(e, "prepare", deadline))?;
        }
        let result = (|| {
            let mut tab = GeminiTab::attach_with_home_url(
                host.port,
                target,
                &route.entry_url,
                mode,
                cancel.clone(),
                deadline,
                false,
            )
            .map_err(|e| model_discovery_error(e, "prepare", deadline))?;
            tab.set_input_lock(ctx.input_lock.cloned());
            verify_probe_identity(account, tab.initial_probe())?;
            let auth = tab
                .check_auth_status()
                .map_err(|e| ServiceError::backend(e, false))?;
            if !auth.signed_in {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "probe",
                    "Sign in with /browser login or `anticrumb login`. Model choices load automatically after sign-in.",
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
                        t.target_type == "page"
                            && crate::gemini::is_reusable_gemini_url_for(
                                &t.url,
                                route.scope.as_deref(),
                            )
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
        browser_profile: &BrowserProfile,
        account: &Account,
        cancel: CancelToken,
    ) -> Result<WebUsageReport> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let route = self.account_route(account);
        let usage_url = route
            .usage_url
            .as_deref()
            .unwrap_or("https://gemini.google.com/u/0/usage");
        let opts = browser::LaunchOptions::headless_browser(browser_profile, &route.entry_url);
        let host = browser::ensure_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
        let mut tab = crate::gemini::SystemGeminiTab::acquire_for_route(
            ctx.store,
            &account.id,
            host.port,
            &route,
            usage_url,
        )
        .map_err(|e| ServiceError::backend(e, false))?;
        let deadline = Instant::now() + WEB_USAGE_TIMEOUT;
        if !tab.created_new {
            crate::gemini::navigate_tab_commit(
                host.port,
                &tab.target,
                usage_url,
                &cancel,
                deadline,
            )
            .map_err(|message| {
                let mut error = ServiceError::backend(message, false);
                error.stage = "usage".into();
                error
            })?;
        }
        let result = crate::gemini::read_web_usage_checked(
            host.port,
            tab.target.clone(),
            route.scope.as_deref(),
            account.identity.as_deref(),
            cancel.clone(),
            deadline,
        );
        let keep_warm = if tab.keep_persistent || !tab.created_new {
            true
        } else {
            browser::list_targets(host.port).is_ok_and(|targets| {
                !targets.iter().any(|t| {
                    t.id != tab.target.id
                        && t.target_type == "page"
                        && crate::gemini::is_reusable_gemini_url_for(&t.url, route.scope.as_deref())
                })
            })
        };
        if keep_warm && !cancel.is_cancelled() {
            tab.keep_persistent = true;
            let return_deadline = Instant::now() + Duration::from_secs(4);
            let _ = crate::gemini::navigate_tab_commit(
                host.port,
                &tab.target,
                &route.entry_url,
                &cancel,
                return_deadline,
            );
        }
        drop(tab);
        let value = result.map_err(|message| {
            if message == "identity_mismatch" {
                return identity_mismatch_error();
            }
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
        browser_profile: &BrowserProfile,
        account: &Account,
        cancel: CancelToken,
        progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        crate::gemini::clear_web_history_with_store(
            ctx.store,
            browser_profile,
            account,
            cancel,
            progress,
        )
    }

    fn delete_conversation(
        &self,
        ctx: &ProviderContext<'_>,
        browser_profile: &BrowserProfile,
        account: &Account,
        session: &Session,
        cancel: CancelToken,
    ) -> Result<()> {
        crate::gemini::delete_web_conversation_with_store(
            ctx.store,
            browser_profile,
            account,
            session,
            cancel,
        )
    }

    fn generate(
        &self,
        ctx: &ProviderContext<'_>,
        browser_profile: &BrowserProfile,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        let store = require_store(ctx)?;
        let route = self.account_route(account);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(request.timeout_secs);
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let (mut tab, is_continue, same_warm_tab) = acquire_tab(
            store,
            browser_profile,
            account,
            &route,
            ctx.input_lock.cloned(),
            session,
            request,
            &cancel,
            deadline,
            emit,
        )?;
        tab.set_pace(crate::gemini::load_human_pace(Some(store)));
        verify_probe_identity(account, tab.initial_probe())?;
        if tab.initial_probe()["signed_in"] != true
            && !tab
                .check_auth_status()
                .map_err(|e| ServiceError::backend(e, false))?
                .signed_in
        {
            return Err(ServiceError::new(
                ErrorCode::AuthRequired,
                "prepare",
                "account is signed out; use /browser login or `anticrumb login`",
            ));
        }
        if is_continue {
            let probe_count = tab.initial_probe()["response_count"].as_u64();
            super::verify_continued_session_turns(
                store,
                probe_count,
                session,
                |expected| tab.verify_owned_turns(expected),
                |s| self.close_session(ctx, browser_profile, account, s),
            )?;
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
        let result = stream_turn(&mut tab, browser_profile.port, session, &prompt, emit)?;
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
        browser_profile: &BrowserProfile,
        account: &Account,
        session: &Session,
    ) -> Result<Vec<String>> {
        let store = require_store(ctx)?;
        let route = self.account_route(account);
        let owned = crate::tab_pool::owned_media_target(
            store,
            session,
            browser::list_targets(browser_profile.port).unwrap_or_default(),
        )?;
        let (target, created) = if let Some(target) = owned {
            (target, false)
        } else {
            let raw_url = session
                .remote_url
                .as_deref()
                .ok_or_else(|| ServiceError::invalid("no saved remote conversation"))?;
            let parsed = crate::url_lite::Url::parse(raw_url).map_err(ServiceError::storage)?;
            if !crate::gemini::is_gemini_origin(&parsed) || !crate::gemini::url_has_chat_id(raw_url)
            {
                return Err(ServiceError::invalid(
                    "this temporary or unsaved conversation cannot be reopened",
                ));
            }
            let url = self.rebase_chat_url(account, raw_url)?;
            (
                browser::create_tab(browser_profile.port, &url)
                    .map_err(|e| ServiceError::backend(e, false))?,
                true,
            )
        };
        let _close = browser::CloseTabGuard {
            port: browser_profile.port,
            target_id: created.then(|| target.id.clone()),
        };
        let mut tab = GeminiTab::attach_with_home_url(
            browser_profile.port,
            target,
            &route.entry_url,
            ChatSessionMode::Continue,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
            true,
        )
        .map_err(|e| ServiceError::backend(e, false))?;
        tab.set_input_lock(ctx.input_lock.cloned());
        verify_probe_identity(account, tab.initial_probe())?;
        tab.wait_owned_response(session.remote_turns)
            .map_err(|e| ServiceError::backend(e, false))?;
        capture_media(store, &mut tab, &account.id)
    }

    fn close_session(
        &self,
        ctx: &ProviderContext<'_>,
        browser_profile: &BrowserProfile,
        account: &Account,
        session: &Session,
    ) -> Result<()> {
        let store = require_store(ctx)?;
        let route = self.account_route(account);
        crate::tab_pool::release_session_tab_for_route(
            store,
            &account.id,
            browser_profile.port,
            session,
            &route,
        )
    }
}

pub(crate) fn identity_mismatch_error() -> ServiceError {
    ServiceError::new(
        ErrorCode::AuthRequired,
        "identity_mismatch",
        "browser tab switched to a different account; rescanning accounts",
    )
}

pub(crate) fn verify_probe_identity(account: &Account, probe: &Value) -> Result<()> {
    if let Some(expected) = account
        .identity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(observed) = probe["identity"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !observed.eq_ignore_ascii_case(expected) {
                return Err(identity_mismatch_error());
            }
        }
    }
    if let Some(expected_slot) = account
        .slot
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let observed_slot = probe["slot"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                probe["url"]
                    .as_str()
                    .and_then(crate::gemini::gemini_url_scope)
            });
        if observed_slot
            .as_deref()
            .is_some_and(|obs| obs != expected_slot)
        {
            return Err(identity_mismatch_error());
        }
    }
    Ok(())
}

/// Sequentially probe `/u/0/app`, `/u/1/app`, ... in one temporary tab to discover
/// all signed-in Google accounts in the Gemini Chrome profile.
pub(crate) fn discover_accounts_on_port(
    port: u16,
    cancel: CancelToken,
    per_slot_timeout: Duration,
) -> Result<Vec<DiscoveredAccount>> {
    cancel
        .check()
        .map_err(|e| ServiceError::backend(e, false))?;
    let target =
        browser::create_tab(port, "about:blank").map_err(|e| ServiceError::backend(e, false))?;
    let _close = browser::CloseTabGuard::new(port, target.id.clone());
    let driver = crate::adapter::script_for(DEFAULT_PROVIDER_ID, "driver.js")
        .map_err(ServiceError::invalid)?;
    let probe_expr = format!("({})({})", driver, json!({"op": "probe"}));
    let overall_deadline = Instant::now() + ACCOUNT_DISCOVERY_TIMEOUT;
    let mut discovered = Vec::new();
    let mut seen_identities = HashSet::new();

    for idx in 0usize..10 {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let now = Instant::now();
        if now >= overall_deadline {
            break;
        }
        let slot_str = idx.to_string();
        let url = crate::gemini::gemini_app_url(Some(&slot_str));
        let slot_deadline = overall_deadline.min(now + per_slot_timeout);
        if let Err(err) =
            crate::gemini::navigate_tab_commit(port, &target, &url, &cancel, slot_deadline)
        {
            if idx == 0 {
                return Err(ServiceError::backend(err, false));
            }
            break;
        }
        let path = browser::ws_path_from_url(&target.ws_url).map_err(ServiceError::invalid)?;
        let mut cdp =
            CdpClient::connect(port, &path).map_err(|e| ServiceError::backend(e, false))?;
        cdp.set_control(cancel.clone(), slot_deadline);
        let _ = cdp.call("Runtime.enable", json!({}));

        let mut last_probe = Value::Null;
        let mut identity_wait_polls = 0u8;
        let mut resolved_slot = None;
        let mut stop_scanning = false;
        while Instant::now() < slot_deadline {
            cancel
                .check()
                .map_err(|e| ServiceError::backend(e, false))?;
            let eval = match cdp.call(
                "Runtime.evaluate",
                json!({
                    "expression": &probe_expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "timeout": 2000
                }),
            ) {
                Ok(v) => v,
                Err(_) => {
                    if cancel.sleep_or_cancelled(Duration::from_millis(120)) {
                        return Err(ServiceError::backend("cancelled".into(), false));
                    }
                    continue;
                }
            };
            if eval.get("exceptionDetails").is_some() {
                if cancel.sleep_or_cancelled(Duration::from_millis(120)) {
                    return Err(ServiceError::backend("cancelled".into(), false));
                }
                continue;
            }
            let probe = eval["result"]["value"].clone();
            last_probe = probe.clone();
            if probe["verification"] == true {
                return Err(ServiceError::new(
                    ErrorCode::VerificationRequired,
                    "account_discovery",
                    "complete Google's human verification in Chrome, then refresh accounts",
                ));
            }
            if probe["url"]
                .as_str()
                .is_some_and(|u| u.contains("accounts.google.com"))
            {
                if idx == 0 {
                    return Err(ServiceError::new(
                        ErrorCode::AuthRequired,
                        "account_discovery",
                        "Sign in with /browser login or `anticrumb login` before discovering accounts.",
                    ));
                }
                stop_scanning = true;
                break;
            }
            if authenticated(&probe) {
                let observed_scope = probe["slot"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        probe["url"]
                            .as_str()
                            .and_then(crate::gemini::gemini_url_scope)
                    });
                if idx > 0 && observed_scope.as_deref() != Some(slot_str.as_str()) {
                    // Out-of-range `/u/{idx}/app` redirected back to `/u/0/app` (or another slot).
                    stop_scanning = true;
                    break;
                }
                let identity = probe["identity"]
                    .as_str()
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty());
                if identity.is_none() && identity_wait_polls < 3 {
                    identity_wait_polls += 1;
                    if cancel.sleep_or_cancelled(Duration::from_millis(100)) {
                        return Err(ServiceError::backend("cancelled".into(), false));
                    }
                    continue;
                }
                let label = probe["label"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .or_else(|| identity.clone());
                if idx > 0 {
                    match &identity {
                        Some(id_str) if !seen_identities.contains(id_str) => {}
                        _ => {
                            stop_scanning = true;
                            break;
                        }
                    }
                }
                if let Some(ref id_str) = identity {
                    seen_identities.insert(id_str.clone());
                }
                resolved_slot = Some(DiscoveredAccount {
                    identity: identity.clone(),
                    label,
                    slot: Some(slot_str.clone()),
                });
                if idx == 0 && identity.is_none() {
                    // Graceful single-account fallback when page does not expose email.
                    stop_scanning = true;
                }
                break;
            }
            if cancel.sleep_or_cancelled(Duration::from_millis(120)) {
                return Err(ServiceError::backend("cancelled".into(), false));
            }
        }
        if let Some(found) = resolved_slot {
            discovered.push(found);
            if stop_scanning {
                break;
            }
            continue;
        }
        if stop_scanning {
            break;
        }
        if idx == 0 {
            if last_probe["signed_in"] == false {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "account_discovery",
                    "Sign in with /browser login or `anticrumb login` before discovering accounts.",
                ));
            }
            return Err(ServiceError::new(
                ErrorCode::DeadlineExceeded,
                "account_discovery",
                "Gemini page did not become ready while scanning accounts; verify login in Chrome and retry",
            ));
        }
        break;
    }

    Ok(discovered)
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

#[allow(clippy::too_many_arguments)]
fn acquire_tab(
    store: &Store,
    browser_profile: &BrowserProfile,
    account: &Account,
    route: &AccountRoute,
    input_lock: Option<Arc<Mutex<()>>>,
    session: &mut Session,
    request: &TurnRequest,
    cancel: &CancelToken,
    deadline: Instant,
    emit: Emit<'_>,
) -> Result<(GeminiTab, bool, bool)> {
    let opts = browser::LaunchOptions::headless_browser(browser_profile, &route.entry_url);
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
        if let Some(expected_raw) = &session.remote_url {
            let expected =
                crate::gemini::rebase_gemini_chat_url(expected_raw, account.slot.as_deref())
                    .unwrap_or_else(|_| expected_raw.clone());
            let expected_url =
                crate::url_lite::Url::parse(&expected).map_err(ServiceError::storage)?;
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
                        host.port, &target, &expected, cancel, deadline,
                    )
                    .map_err(|e| ServiceError::backend(e, false))?;
                    navigated = true;
                } else {
                    return Err(super::invalidate_unavailable_session(
                        store,
                        session,
                        |_| Ok(()),
                        "owned tab navigated to another conversation; start a new session",
                    ));
                }
            }
        }
        same_warm_tab = !navigated;
        (target, ChatSessionMode::Continue, false)
    } else if continuing {
        if session.mode == SessionMode::Chat {
            if let Some(remote_url_raw) = session.remote_url.clone() {
                let remote_url =
                    crate::gemini::rebase_gemini_chat_url(&remote_url_raw, account.slot.as_deref())
                        .unwrap_or(remote_url_raw);
                emit(
                    "browser_stage",
                    json!({"stage":"navigating_tab","detail":"acquiring tab & opening saved Gemini chat"}),
                )?;
                let (target, _) = crate::tab_pool::acquire_session_tab_for_route(
                    store,
                    &account.id,
                    host.port,
                    &session.id,
                    &SessionMode::Chat,
                    route,
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
                return Err(super::invalidate_unavailable_session(
                    store,
                    session,
                    |_| Ok(()),
                    "owned browser tab is gone; start a new session",
                ));
            }
        } else {
            return Err(super::invalidate_unavailable_session(
                store,
                session,
                |_| Ok(()),
                "owned browser tab is gone; start a new session",
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
        let (target, reused) = crate::tab_pool::acquire_session_tab_for_route(
            store,
            &account.id,
            host.port,
            &session.id,
            &request.mode,
            route,
        )?;
        (target, mode, reused)
    };
    session.target_id = Some(target.id.clone());
    store.put("local", "session", &session.id, session)?;
    let mut tab = if reused {
        GeminiTab::attach_reused_with_home_url(
            host.port,
            target,
            &route.entry_url,
            mode,
            cancel.clone(),
            deadline,
            request.generate_image,
        )
    } else {
        GeminiTab::attach_with_home_url(
            host.port,
            target,
            &route.entry_url,
            mode,
            cancel.clone(),
            deadline,
            request.generate_image,
        )
    }
    .map_err(|e| ServiceError::backend(e, false))?;
    tab.set_control(cancel.clone(), deadline);
    tab.set_input_lock(input_lock);
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
        emit("uploading", json!({"count":files.len()}))?;
        tab.attach_files(&files, None::<&mut fn(&crate::gemini::AttachmentProgress)>)
            .map_err(|e| ServiceError::new(ErrorCode::UploadFailed, "upload", e))?;
    }
    Ok(())
}

fn stream_turn(
    tab: &mut GeminiTab,
    browser_port: u16,
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
            session.state = SessionState::OutcomeUnknown;
            if matches!(
                err.code,
                ErrorCode::Cancelled
                    | ErrorCode::DeadlineExceeded
                    | ErrorCode::SubmissionOutcomeUnknown
            ) {
                if let Some(target) = &session.target_id {
                    let _ = GeminiTab::stop_owned(browser_port, target);
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
        let name = Path::new(&path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        artifacts.push(
            store
                .artifact(account, &name, mime, &bytes, "generated")?
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
