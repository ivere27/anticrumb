use crate::{
    browser_host as browser,
    domain::*,
    provider::{
        suggest_account_id, suggest_account_id_for_identity, DiscoveredAccount, Provider,
        ProviderContext, ProviderRegistry,
    },
    store::Store,
};
use std::{collections::HashSet, time::Duration};
pub(crate) mod login;
pub(crate) const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const ACCOUNT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const WEB_USAGE_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const BROWSER_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn browser_tool_tab(
    browser_profile: &BrowserProfile,
) -> Result<(
    u16,
    browser::TargetInfo,
    Option<browser::BrowserContextGuard>,
)> {
    let port = if crate::web::allow_private_env() {
        let opts = browser::LaunchOptions::headless_browser(browser_profile, "about:blank");
        browser::ensure_browser(&opts)
            .map_err(|e| ServiceError::backend(e, false))?
            .port
    } else {
        browser_profile.port
    };
    browser::create_browser_tool_tab(port).map_err(|e| ServiceError::backend(e, false))
}

/// Render an external page in an isolated browser without touching provider chats.
pub fn browser_fetch(
    browser_profile: &BrowserProfile,
    url: &str,
    wait_ms: u32,
    max_chars: usize,
    cancel: crate::agent::CancelToken,
) -> Result<crate::gemini::BrowserFetchOutput> {
    cancel
        .check()
        .map_err(|e| ServiceError::backend(e, false))?;
    crate::web::validate_http_url(url, crate::web::allow_private_env()).map_err(|e| {
        let mut error = ServiceError::invalid(e);
        error.stage = "browser_fetch".into();
        error
    })?;
    let (port, target, _context) = browser_tool_tab(browser_profile)?;
    let _close = browser::CloseTabGuard::new(port, target.id.clone());
    crate::gemini::fetch_page(
        port,
        target,
        url,
        wait_ms,
        max_chars,
        cancel,
        std::time::Instant::now() + BROWSER_FETCH_TIMEOUT,
    )
    .map_err(|message| {
        let mut error = ServiceError::backend(message, false);
        error.stage = "browser_fetch".into();
        error
    })
}

/// Read the account's own usage page without creating a model turn or updating
/// its model catalog.
pub fn web_usage(
    browser_profile: &BrowserProfile,
    account: &Account,
    provider: &dyn Provider,
    cancel: crate::agent::CancelToken,
) -> Result<WebUsageReport> {
    web_usage_with_store(None, browser_profile, account, provider, cancel)
}

/// Delegate web usage inspection to the account's configured provider.
pub fn web_usage_with_store(
    store: Option<&Store>,
    browser_profile: &BrowserProfile,
    account: &Account,
    provider: &dyn Provider,
    cancel: crate::agent::CancelToken,
) -> Result<WebUsageReport> {
    provider.read_usage(
        &ProviderContext {
            store,
            input_lock: None,
        },
        browser_profile,
        account,
        cancel,
    )
}

pub fn provider_login_status(store: &Store, provider_id: &str) -> Option<LoginStatus> {
    store
        .get::<Option<LoginStatus>>("local", "provider_login", provider_id)
        .ok()
        .flatten()
}

pub fn allocate_cdp_port(existing: &[BrowserProfile], requested: u16) -> Result<u16> {
    if requested > 0 {
        if existing.iter().any(|b| b.port == requested) {
            return Err(ServiceError::invalid("browser CDP port already assigned"));
        }
        return Ok(requested);
    }
    let used: HashSet<u16> = existing.iter().map(|b| b.port).collect();
    for port in browser::DEFAULT_CDP_PORT..=u16::MAX {
        if used.contains(&port) || browser::is_port_alive(port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(ServiceError::invalid("no free CDP port available"))
}

/// Idempotently ensures a registered provider has a managed `BrowserProfile`
/// and at least one `Account` record in the store. Does NOT launch Chrome.
pub fn ensure_provider_account(
    store: &Store,
    providers: &ProviderRegistry,
    provider_id: &str,
) -> Result<(BrowserProfile, Account)> {
    let provider_id = if provider_id.trim().is_empty() {
        default_provider()
    } else {
        provider_id.trim().to_owned()
    };
    providers.resolve(&provider_id)?;
    let browser_profile = match store.browser_profile(&provider_id) {
        Ok(existing) => existing,
        Err(err) if err.code == ErrorCode::NotFound => {
            let existing_browsers = store.list_browser_profiles()?;
            let port = allocate_cdp_port(&existing_browsers, 0)?;
            let profile = BrowserProfile {
                provider_id: provider_id.clone(),
                profile: browser::provider_profile_dir(&provider_id),
                subprofile: None,
                port,
                headless: true,
            };
            store.put_browser_profile(&profile)?;
            profile
        }
        Err(err) => return Err(err),
    };
    let provider_accounts = store.accounts_for_provider(&provider_id)?;
    if !provider_accounts.is_empty() {
        let active_id = store.active_account_id()?;
        let chosen = active_id
            .as_deref()
            .and_then(|aid| provider_accounts.iter().find(|a| a.id == aid))
            .or_else(|| provider_accounts.iter().find(|a| a.state == "ready"))
            .unwrap_or(&provider_accounts[0])
            .clone();
        if active_id.is_none() {
            let _ = store.set_active_account_id(&chosen.id);
        }
        return Ok((browser_profile, chosen));
    }
    let all_accounts = store.list::<Account>("local", "account")?;
    let existing_ids: Vec<&str> = all_accounts.iter().map(|a| a.id.as_str()).collect();
    let new_id = suggest_account_id(&provider_id, &existing_ids);
    let account = Account {
        id: new_id,
        provider: provider_id,
        state: "auth_required".into(),
        identity: None,
        label: None,
        slot: None,
    };
    store.put("local", "account", &account.id, &account)?;
    if store.active_account_id()?.is_none() {
        store.set_active_account_id(&account.id)?;
    }
    Ok((browser_profile, account))
}

/// Atomically reconcile a provider's discovered Chrome accounts with persisted `Account` records.
pub fn sync_discovered_accounts(
    store: &Store,
    provider_id: &str,
    multi_account: bool,
    discovered: Vec<DiscoveredAccount>,
) -> Result<Vec<Account>> {
    let mut existing = store.accounts_for_provider(provider_id)?;
    let mut all_ids: Vec<String> = store
        .list::<Account>("local", "account")?
        .into_iter()
        .map(|a| a.id)
        .collect();

    // Normalize and deduplicate discovered accounts.
    let mut normalized = Vec::new();
    let mut seen_identities = HashSet::new();
    for item in discovered {
        let identity = item
            .identity
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        if let Some(ref id_str) = identity {
            if !seen_identities.insert(id_str.clone()) {
                continue;
            }
        }
        let label = item
            .label
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| identity.clone());
        let slot = item
            .slot
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        normalized.push(DiscoveredAccount {
            identity,
            label,
            slot,
        });
        if !multi_account {
            break;
        }
    }

    let mut matched_ids = HashSet::new();
    let mut synced = Vec::new();

    for disc in normalized {
        let matched_idx = if let Some(ref disc_identity) = disc.identity {
            // 1. Match by permanent external identity first.
            existing
                .iter()
                .position(|a| {
                    !matched_ids.contains(&a.id)
                        && a.identity
                            .as_deref()
                            .is_some_and(|id| id.eq_ignore_ascii_case(disc_identity))
                })
                // 2. Otherwise promote an unpromoted placeholder account (identity == None).
                .or_else(|| {
                    existing
                        .iter()
                        .position(|a| !matched_ids.contains(&a.id) && a.identity.is_none())
                })
                // 3. For single-account providers, reuse the provider's single account even if identity changed.
                .or_else(|| {
                    (!multi_account)
                        .then(|| existing.iter().position(|a| !matched_ids.contains(&a.id)))
                        .flatten()
                })
        } else {
            // Anonymous / identity-less discovery (e.g. single-account provider or fallback slot 0).
            existing
                .iter()
                .position(|a| !matched_ids.contains(&a.id) && a.slot == disc.slot)
                .or_else(|| existing.iter().position(|a| !matched_ids.contains(&a.id)))
        };

        if let Some(idx) = matched_idx {
            let acct = &mut existing[idx];
            let slot_changed = acct.slot != disc.slot;
            let identity_changed = disc.identity.is_some()
                && acct
                    .identity
                    .as_deref()
                    .is_some_and(|prev| Some(prev) != disc.identity.as_deref());
            if let Some(id_str) = disc.identity {
                acct.identity = Some(id_str);
            }
            if let Some(lbl) = disc.label {
                acct.label = Some(lbl);
            }
            acct.slot = disc.slot;
            if acct.state != "disabled" {
                acct.state = "ready".into();
            }
            store.put("local", "account", &acct.id, acct)?;
            if slot_changed || identity_changed {
                invalidate_account_sessions(store, &acct.id)?;
            }
            matched_ids.insert(acct.id.clone());
            synced.push(acct.clone());
        } else {
            let id_refs: Vec<&str> = all_ids.iter().map(String::as_str).collect();
            let new_id = if let Some(ref ident) = disc.identity {
                suggest_account_id_for_identity(provider_id, ident, &id_refs)
            } else {
                suggest_account_id(provider_id, &id_refs)
            };
            let acct = Account {
                id: new_id.clone(),
                provider: provider_id.into(),
                state: "ready".into(),
                identity: disc.identity,
                label: disc.label,
                slot: disc.slot,
            };
            store.put("local", "account", &acct.id, &acct)?;
            all_ids.push(new_id.clone());
            matched_ids.insert(new_id);
            synced.push(acct);
        }
    }

    // Any existing account of this provider not seen in this scan is now signed out
    // (preserve record for /resume history, clear volatile slot, invalidate live tabs).
    for acct in &mut existing {
        if matched_ids.contains(&acct.id) {
            continue;
        }
        let changed = acct.slot.is_some() || acct.state != "auth_required";
        acct.slot = None;
        if acct.state != "disabled" {
            acct.state = "auth_required".into();
        }
        if changed {
            store.put("local", "account", &acct.id, acct)?;
            invalidate_account_sessions(store, &acct.id)?;
        }
    }

    // Ensure active_account_id points to an existing account (prefer ready).
    let active_valid = store
        .active_account_id()?
        .is_some_and(|aid| store.get::<Account>("local", "account", &aid).is_ok());
    if !active_valid {
        if let Some(first_ready) = synced.iter().find(|a| a.state == "ready") {
            let _ = store.set_active_account_id(&first_ready.id);
        }
    }

    store.accounts_for_provider(provider_id)
}

/// Discover signed-in accounts in a provider's Chrome profile and synchronize local records.
pub fn refresh_accounts(
    store: &Store,
    provider_id: &str,
    provider: &dyn Provider,
    cancel: crate::agent::CancelToken,
) -> Result<Vec<Account>> {
    let browser_profile = store.browser_profile(provider_id)?;
    let discovered = provider.discover_accounts(
        &ProviderContext::with_store(store),
        &browser_profile,
        cancel,
    )?;
    sync_discovered_accounts(
        store,
        provider_id,
        provider.info().capabilities.multi_account,
        discovered,
    )
}

pub fn probe(
    store: &Store,
    browser_profile: &BrowserProfile,
    account: &mut Account,
    provider: &dyn Provider,
) -> Result<ModelCatalogRecord> {
    let result = provider.discover_models(
        &ProviderContext::with_store(store),
        browser_profile,
        account,
        crate::agent::CancelToken::new(),
    );
    persist_probe(store, account, &result)?;
    result
}

pub(crate) fn persist_probe(
    store: &Store,
    account: &mut Account,
    result: &Result<ModelCatalogRecord>,
) -> Result<()> {
    if let Ok(capabilities) = result {
        store.put(&account.id, "catalog", "models", capabilities)?;
        store.put(&account.id, "capability", "current", capabilities)?;
    }
    account.state = match &result {
        Ok(_) => "ready",
        Err(e) if e.code == ErrorCode::AuthRequired => "auth_required",
        Err(e) if e.code == ErrorCode::VerificationRequired => "awaiting_human",
        Err(_) => "adapter_error",
    }
    .into();
    store.put("local", "account", &account.id, account)?;
    Ok(())
}

pub fn browser_status(store: &Store, provider_id: &str) -> Result<BrowserStatusRecord> {
    let browser_profile = store.browser_profile(provider_id)?;
    let accounts = store.accounts_for_provider(provider_id)?;
    let account_ids: HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    let version = browser::fetch_version(browser_profile.port).ok();
    let running = version
        .as_ref()
        .and_then(|_| browser::inspect_running_cdp(browser_profile.port));
    let profile_matches = running
        .as_ref()
        .map(|meta| meta.matches_browser(&browser_profile));
    let mut latest_checked_at: Option<i64> = None;
    let mut last_verified_sign_in: Option<bool> = None;
    for acct in &accounts {
        let capability: ModelCatalogRecord = store
            .get(&acct.id, "capability", "current")
            .unwrap_or_default();
        if let Some(ts) = capability.checked_at {
            if latest_checked_at.is_none_or(|prev| ts >= prev) {
                latest_checked_at = Some(ts);
                last_verified_sign_in = capability.signed_in;
            }
        }
    }
    let owned = store
        .list::<Session>("local", "session")?
        .iter()
        .filter(|s| account_ids.contains(s.account_id.as_str()) && s.state != SessionState::Closed)
        .count() as u64;
    let ready_accounts = accounts.iter().filter(|a| a.state == "ready").count() as u32;
    Ok(BrowserStatusRecord {
        provider_id: provider_id.into(),
        connected: version.is_some(),
        endpoint: format!("http://127.0.0.1:{}", browser_profile.port),
        browser: version.map(|v| v.browser),
        profile: browser_profile.profile,
        profile_matches,
        headless: running.map(|m| m.headless),
        owned_sessions: owned,
        account_count: accounts.len() as u32,
        ready_accounts,
        last_verified_sign_in,
        checked_at: latest_checked_at,
        login: provider_login_status(store, provider_id),
    })
}

pub fn attach(store: &Store, provider_id: &str, port: u16) -> Result<BrowserStatusRecord> {
    let mut browser_profile = store.browser_profile(provider_id)?;
    if port == 0
        || store
            .list_browser_profiles()?
            .iter()
            .any(|b| b.provider_id != provider_id && b.port == port)
    {
        return Err(ServiceError::invalid(
            "CDP port is zero or assigned to another provider",
        ));
    }
    let accounts = store.accounts_for_provider(provider_id)?;
    let account_ids: HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    if store
        .list::<Session>("local", "session")?
        .iter()
        .any(|s| account_ids.contains(s.account_id.as_str()) && s.state != SessionState::Closed)
    {
        return Err(ServiceError::invalid(
            "close owned sessions before attaching another browser",
        ));
    }
    let running = browser::inspect_running_cdp(port).ok_or_else(|| {
        ServiceError::invalid("cannot verify a running Chrome profile on this port")
    })?;
    browser_profile.profile =
        std::fs::canonicalize(&running.user_data_dir).map_err(ServiceError::storage)?;
    browser_profile.subprofile = Some(running.profile_dir);
    browser_profile.port = port;
    browser_profile.headless = running.headless;
    store.put_browser_profile(&browser_profile)?;
    for mut account in accounts {
        account.state = "auth_required".into();
        account.slot = None;
        store.put("local", "account", &account.id, &account)?;
        store.put(
            &account.id,
            "catalog",
            "models",
            &ModelCatalogRecord::default(),
        )?;
        store.put(
            &account.id,
            "capability",
            "current",
            &ModelCatalogRecord::default(),
        )?;
        store.put(
            &account.id,
            "runtime",
            "selection",
            &RuntimeSelection::default(),
        )?;
        invalidate_account_sessions(store, &account.id)?;
    }
    browser_status(store, provider_id)
}

pub(crate) fn ensure_no_active_provider_runs(
    store: &Store,
    provider_id: &str,
    action: &str,
) -> Result<()> {
    let account_ids: HashSet<String> = store
        .accounts_for_provider(provider_id)?
        .into_iter()
        .map(|a| a.id)
        .collect();
    if store.list::<Run>("local", "run")?.iter().any(|r| {
        (r.provider == provider_id || account_ids.contains(&r.request.account_id))
            && ["queued", "preparing", "submitted", "running"].contains(&r.state.as_str())
    }) {
        return Err(ServiceError::invalid(format!(
            "cancel active runs for provider '{provider_id}' before {action}"
        )));
    }
    Ok(())
}

fn verify_running_profile_ownership(
    browser_profile: &BrowserProfile,
) -> Result<browser::RunningCdpMeta> {
    let meta = browser::inspect_running_cdp(browser_profile.port)
        .ok_or_else(|| ServiceError::invalid("cannot verify browser ownership"))?;
    if !meta.matches_browser(browser_profile) {
        return Err(ServiceError::invalid("CDP port belongs to another profile"));
    }
    Ok(meta)
}

pub(crate) fn invalidate_account_sessions(store: &Store, id: &str) -> Result<()> {
    for mut session in store.list::<Session>("local", "session")? {
        if session.account_id != id {
            continue;
        }
        if !session.is_inactive() {
            let resumable_chat = session.state == SessionState::Ready
                && session.mode == SessionMode::Chat
                && (session.remote_url.is_some() || session.remote_session_id.is_some());
            session.state = if resumable_chat {
                SessionState::Closed
            } else {
                SessionState::Invalidated
            };
            session.target_id = None;
            store.put("local", "session", &session.id, &session)?;
        }
    }
    crate::service::clear_system_tab_id(store, id);
    let _ = crate::tab_pool::save_state(store, id, &crate::tab_pool::TabPoolState::default());
    Ok(())
}

pub(crate) fn invalidate_provider_sessions(store: &Store, provider_id: &str) -> Result<()> {
    for account in store.accounts_for_provider(provider_id)? {
        invalidate_account_sessions(store, &account.id)?;
    }
    Ok(())
}

pub fn set_browser_mode(
    store: &Store,
    provider_id: &str,
    headless: bool,
    provider: &dyn Provider,
) -> Result<BrowserStatusRecord> {
    let mut browser_profile = store.browser_profile(provider_id)?;
    let info = provider.info();
    if !info.capabilities.browser_auth {
        return Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "browser",
            format!("provider '{}' does not use a managed browser", info.id),
        ));
    }
    ensure_no_active_provider_runs(store, provider_id, "switching browser mode")?;
    browser::validate_managed_profile(&browser_profile.profile).map_err(ServiceError::invalid)?;
    if browser::is_port_alive(browser_profile.port) {
        let meta = verify_running_profile_ownership(&browser_profile)?;
        if !browser_profile.headless {
            browser_profile.headless = true;
            store.put_browser_profile(&browser_profile)?;
        }
        if meta.headless == headless {
            if !headless {
                let _ = browser::focus_browser_tab(browser_profile.port);
            }
            return browser_status(store, provider_id);
        }
        browser::close_browser_port(browser_profile.port).map_err(ServiceError::invalid)?;
        browser::wait_for_profile_unlock(&browser_profile.profile, Duration::from_secs(5));
        invalidate_provider_sessions(store, provider_id)?;
    } else {
        if !browser_profile.headless {
            browser_profile.headless = true;
            store.put_browser_profile(&browser_profile)?;
        }
        invalidate_provider_sessions(store, provider_id)?;
        if headless {
            return browser_status(store, provider_id);
        }
    }
    let launch_url = provider.default_login_url().unwrap_or("about:blank");
    let opts = browser::LaunchOptions::for_browser(&browser_profile, headless, launch_url);
    browser::launch_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
    browser_status(store, provider_id)
}

pub fn login(store: &Store, provider_id: &str, provider: &dyn Provider) -> Result<()> {
    let mut browser_profile = store.browser_profile(provider_id)?;
    let info = provider.info();
    if !info.capabilities.browser_auth {
        return Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "login",
            format!("provider '{}' does not use browser sign-in", info.id),
        ));
    }
    let login_url = provider.default_login_url().ok_or_else(|| {
        ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "login",
            format!("provider '{}' has no configured login entry_url", info.id),
        )
    })?;
    ensure_no_active_provider_runs(store, provider_id, "opening login")?;
    let accounts = store.accounts_for_provider(provider_id)?;
    let account_ids: HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    if store.list::<Session>("local", "session")?.iter().any(|s| {
        account_ids.contains(s.account_id.as_str())
            && s.state == SessionState::Ready
            && s.target_id.is_some()
    }) {
        return Err(ServiceError::invalid(
            "close active sessions before restarting the provider browser for login",
        ));
    }
    browser::validate_managed_profile(&browser_profile.profile).map_err(ServiceError::invalid)?;
    if browser::is_port_alive(browser_profile.port) {
        let _ = verify_running_profile_ownership(&browser_profile)?;
        browser::close_browser_port(browser_profile.port).map_err(ServiceError::invalid)?;
        browser::wait_for_profile_unlock(&browser_profile.profile, Duration::from_secs(5));
        invalidate_provider_sessions(store, provider_id)?;
    }
    // Cold starts always default back to headless once this visible login window exits.
    browser_profile.headless = true;
    store.put_browser_profile(&browser_profile)?;
    for mut account in accounts {
        if account.state != "disabled" {
            account.state = "awaiting_human".into();
            store.put("local", "account", &account.id, &account)?;
        }
    }
    let opts = browser::LaunchOptions::for_browser(&browser_profile, false, login_url);
    browser::launch_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::GeminiWebProvider;

    #[test]
    fn browser_mode_helpers_reject_active_runs_and_invalidate_dead_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let browser_profile = BrowserProfile {
            provider_id: default_provider(),
            profile: dir.path().join("external-profile"),
            subprofile: None,
            port: 59999,
            headless: false,
        };
        store.put_browser_profile(&browser_profile).unwrap();
        let acct = Account {
            id: "gemini".into(),
            provider: default_provider(),
            state: "ready".into(),
            identity: Some("alice@gmail.com".into()),
            label: Some("alice@gmail.com".into()),
            slot: Some("0".into()),
        };
        let sibling = Account {
            id: "gemini-bob".into(),
            provider: default_provider(),
            state: "ready".into(),
            identity: Some("bob@gmail.com".into()),
            label: Some("bob@gmail.com".into()),
            slot: Some("1".into()),
        };
        store.put("local", "account", &acct.id, &acct).unwrap();
        store
            .put("local", "account", &sibling.id, &sibling)
            .unwrap();

        let make_session =
            |sid: &str, account_id: &str, state: SessionState, target: Option<&str>| Session {
                id: sid.into(),
                provider: default_provider(),
                account_id: account_id.into(),
                mode: SessionMode::Chat,
                target_id: target.map(str::to_string),
                remote_url: None,
                remote_session_id: None,
                history: vec![],
                state,
                model: None,
                thinking: None,
                tool_digest: String::new(),
                remote_turns: 0,
                last_used: 1,
                reuse_scope: None,
            };
        store
            .put(
                "local",
                "session",
                "s-ready",
                &make_session("s-ready", "gemini", SessionState::Ready, Some("t1")),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-human",
                &make_session("s-human", "gemini", SessionState::AwaitingHuman, Some("t2")),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-deleted",
                &make_session("s-deleted", "gemini", SessionState::Deleted, Some("t3")),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-unknown",
                &make_session(
                    "s-unknown",
                    "gemini",
                    SessionState::OutcomeUnknown,
                    Some("t4"),
                ),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-sibling",
                &make_session("s-sibling", "gemini-bob", SessionState::Ready, Some("t5")),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-other",
                &Session {
                    provider: "claude-web".into(),
                    ..make_session("s-other", "claude", SessionState::Ready, Some("t6"))
                },
            )
            .unwrap();

        let mut resumable_chat = make_session(
            "s-chat-resumable",
            "gemini",
            SessionState::Ready,
            Some("t-chat"),
        );
        resumable_chat.remote_url = Some("https://gemini.google.com/u/0/app/123".into());
        resumable_chat.remote_turns = 1;
        store
            .put("local", "session", "s-chat-resumable", &resumable_chat)
            .unwrap();

        let mut temp_session =
            make_session("s-temp", "gemini", SessionState::Ready, Some("t-temp"));
        temp_session.mode = SessionMode::Temporary;
        temp_session.remote_url = Some("https://gemini.google.com/u/0/app".into());
        store
            .put("local", "session", "s-temp", &temp_session)
            .unwrap();

        // Non-managed profile must be rejected before attempting launch or shutdown.
        let err = set_browser_mode(
            &store,
            &default_provider(),
            true,
            &GeminiWebProvider::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        // Provider invalidation clears ALL sibling accounts of gemini-web without touching claude-web.
        invalidate_provider_sessions(&store, &default_provider()).unwrap();
        let ready: Session = store.get("local", "session", "s-ready").unwrap();
        assert_eq!(ready.state, SessionState::Invalidated);
        assert!(ready.target_id.is_none());
        let chat_resumable: Session = store.get("local", "session", "s-chat-resumable").unwrap();
        assert_eq!(chat_resumable.state, SessionState::Closed);
        assert!(chat_resumable.target_id.is_none());
        assert_eq!(
            chat_resumable.remote_url.as_deref(),
            Some("https://gemini.google.com/u/0/app/123")
        );
        let temp: Session = store.get("local", "session", "s-temp").unwrap();
        assert_eq!(temp.state, SessionState::Invalidated);
        assert!(temp.target_id.is_none());
        let human: Session = store.get("local", "session", "s-human").unwrap();
        assert_eq!(human.state, SessionState::Invalidated);
        assert!(human.target_id.is_none());
        let deleted: Session = store.get("local", "session", "s-deleted").unwrap();
        assert_eq!(deleted.state, SessionState::Deleted);
        let unknown: Session = store.get("local", "session", "s-unknown").unwrap();
        assert_eq!(unknown.state, SessionState::OutcomeUnknown);
        let sibling_sess: Session = store.get("local", "session", "s-sibling").unwrap();
        assert_eq!(sibling_sess.state, SessionState::Invalidated);
        assert!(sibling_sess.target_id.is_none());
        let other: Session = store.get("local", "session", "s-other").unwrap();
        assert_eq!(other.state, SessionState::Ready);
        assert_eq!(other.target_id.as_deref(), Some("t6"));

        // Active run on sibling account blocks provider browser mode switching.
        let mut req = TurnRequest::text("hi");
        req.account_id = "gemini-bob".into();
        let run = Run {
            id: "run_1".into(),
            provider: default_provider(),
            request: req,
            state: "running".into(),
            result: None,
            error: None,
            created_at: 1,
            updated_at: 1,
        };
        store.put("local", "run", &run.id, &run).unwrap();
        let active_err =
            ensure_no_active_provider_runs(&store, &default_provider(), "switching browser mode")
                .unwrap_err();
        assert_eq!(active_err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn sync_discovered_accounts_promotes_placeholder_tracks_slot_swaps_and_logouts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let providers = ProviderRegistry::with_builtins();
        let (_, initial) = ensure_provider_account(&store, &providers, "gemini-web").unwrap();
        assert_eq!(initial.id, "gemini");
        assert!(initial.identity.is_none());

        // 1. Discover two accounts: placeholder `gemini` promoted to alice (slot 0), new `gemini-bob` (slot 1).
        let synced = sync_discovered_accounts(
            &store,
            "gemini-web",
            true,
            vec![
                DiscoveredAccount {
                    identity: Some("Alice@Gmail.com".into()),
                    label: Some("alice@gmail.com".into()),
                    slot: Some("0".into()),
                },
                DiscoveredAccount {
                    identity: Some("bob.work@company.com".into()),
                    label: Some("bob.work@company.com".into()),
                    slot: Some("1".into()),
                },
            ],
        )
        .unwrap();
        assert_eq!(synced.len(), 2);
        let alice = store.get::<Account>("local", "account", "gemini").unwrap();
        assert_eq!(alice.identity.as_deref(), Some("alice@gmail.com"));
        assert_eq!(alice.slot.as_deref(), Some("0"));
        assert_eq!(alice.state, "ready");
        let bob = store
            .get::<Account>("local", "account", "gemini-bob-work")
            .unwrap();
        assert_eq!(bob.identity.as_deref(), Some("bob.work@company.com"));
        assert_eq!(bob.slot.as_deref(), Some("1"));
        assert_eq!(bob.state, "ready");

        // Record a live session on bob so we can verify slot-swap invalidates its target_id but keeps remote_url.
        let sess = Session {
            id: "sess_bob".into(),
            provider: "gemini-web".into(),
            account_id: bob.id.clone(),
            mode: SessionMode::Chat,
            target_id: Some("tab-old-bob".into()),
            remote_url: Some("https://gemini.google.com/u/1/app/chat_99".into()),
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: 1,
            reuse_scope: None,
        };
        store.put("local", "session", &sess.id, &sess).unwrap();

        // 2. Alice signs out; Bob shifts from slot "1" -> slot "0".
        sync_discovered_accounts(
            &store,
            "gemini-web",
            true,
            vec![DiscoveredAccount {
                identity: Some("bob.work@company.com".into()),
                label: None,
                slot: Some("0".into()),
            }],
        )
        .unwrap();

        let alice_after = store.get::<Account>("local", "account", "gemini").unwrap();
        assert_eq!(alice_after.state, "auth_required");
        assert!(alice_after.slot.is_none());

        let bob_after = store
            .get::<Account>("local", "account", "gemini-bob-work")
            .unwrap();
        assert_eq!(bob_after.state, "ready");
        assert_eq!(bob_after.slot.as_deref(), Some("0"));

        let sess_after: Session = store.get("local", "session", "sess_bob").unwrap();
        assert_eq!(sess_after.state, SessionState::Closed);
        assert!(sess_after.target_id.is_none());
        assert_eq!(
            sess_after.remote_url.as_deref(),
            Some("https://gemini.google.com/u/1/app/chat_99")
        );

        // 3. Query resolution works by ID, email, slot, and unique localpart.
        assert_eq!(
            store
                .resolve_account_query("bob.work@company.com", Some("gemini-web"))
                .unwrap()
                .id,
            "gemini-bob-work"
        );
        assert_eq!(
            store
                .resolve_account_query("u/0", Some("gemini-web"))
                .unwrap()
                .id,
            "gemini-bob-work"
        );
        assert_eq!(
            store.resolve_account_query("bob.work", None).unwrap().id,
            "gemini-bob-work"
        );
    }
}
