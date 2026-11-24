use crate::{browser_host as browser, domain::*, provider::Provider, store::Store};
use std::time::Duration;
pub(crate) mod login;
pub(crate) const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const WEB_USAGE_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const BROWSER_FETCH_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) fn browser_tool_tab(
    account: &Account,
) -> Result<(
    u16,
    browser::TargetInfo,
    Option<browser::BrowserContextGuard>,
)> {
    let port = if crate::web::allow_private_env() {
        let opts = browser::LaunchOptions::headless_account(account, "about:blank");
        browser::ensure_browser(&opts)
            .map_err(|e| ServiceError::backend(e, false))?
            .port
    } else {
        account.port
    };
    browser::create_browser_tool_tab(port).map_err(|e| ServiceError::backend(e, false))
}

/// Render an external page in an isolated browser without touching provider chats.
pub fn browser_fetch(
    account: &Account,
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
    let (port, target, _context) = browser_tool_tab(account)?;
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
    account: &Account,
    provider: &dyn Provider,
    cancel: crate::agent::CancelToken,
) -> Result<WebUsageReport> {
    web_usage_with_store(None, account, provider, cancel)
}

/// Delegate web usage inspection to the account's configured provider.
pub fn web_usage_with_store(
    store: Option<&Store>,
    account: &Account,
    provider: &dyn Provider,
    cancel: crate::agent::CancelToken,
) -> Result<WebUsageReport> {
    provider.read_usage(&crate::provider::ProviderContext { store }, account, cancel)
}

pub fn login_status(store: &Store, id: &str) -> Option<LoginStatus> {
    store
        .get::<Option<LoginStatus>>(id, "login", "current")
        .ok()
        .flatten()
}

pub fn probe(
    store: &Store,
    account: &mut Account,
    provider: &dyn Provider,
) -> Result<ModelCatalogRecord> {
    let result = provider.discover_models(
        &crate::provider::ProviderContext::with_store(store),
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
pub fn browser_status(store: &Store, id: &str) -> Result<BrowserStatusRecord> {
    let account: Account = store.get("local", "account", id)?;
    let version = browser::fetch_version(account.port).ok();
    let running = version
        .as_ref()
        .and_then(|_| browser::inspect_running_cdp(account.port));
    let profile_matches = running.as_ref().map(|meta| meta.matches_account(&account));
    let capability: ModelCatalogRecord = store
        .get(&account.id, "capability", "current")
        .unwrap_or_default();
    let owned = store
        .list::<Session>("local", "session")?
        .iter()
        .filter(|s| s.account_id == id && s.state != SessionState::Closed)
        .count() as u64;
    Ok(BrowserStatusRecord {
        account_id: id.into(),
        account_state: account.state,
        connected: version.is_some(),
        endpoint: format!("http://127.0.0.1:{}", account.port),
        browser: version.map(|v| v.browser),
        profile: account.profile,
        profile_matches,
        headless: running.map(|m| m.headless),
        owned_sessions: owned,
        last_verified_sign_in: capability.signed_in,
        checked_at: capability.checked_at,
        login: login_status(store, id),
    })
}
pub fn attach(store: &Store, id: &str, port: u16) -> Result<BrowserStatusRecord> {
    let mut account: Account = store.get("local", "account", id)?;
    if port == 0
        || store
            .list::<Account>("local", "account")?
            .iter()
            .any(|a| a.id != id && a.port == port)
    {
        return Err(ServiceError::invalid(
            "CDP port is zero or assigned to another account",
        ));
    }
    if store
        .list::<Session>("local", "session")?
        .iter()
        .any(|s| s.account_id == id && s.state != SessionState::Closed)
    {
        return Err(ServiceError::invalid(
            "close owned sessions before attaching another browser",
        ));
    }
    let running = browser::inspect_running_cdp(port).ok_or_else(|| {
        ServiceError::invalid("cannot verify a running Chrome profile on this port")
    })?;
    account.profile =
        std::fs::canonicalize(&running.user_data_dir).map_err(ServiceError::storage)?;
    account.subprofile = Some(running.profile_dir);
    account.port = port;
    account.headless = running.headless;
    account.state = "auth_required".into();
    store.put("local", "account", id, &account)?;
    store.put(id, "catalog", "models", &ModelCatalogRecord::default())?;
    store.put(id, "capability", "current", &ModelCatalogRecord::default())?;
    store.put(id, "runtime", "selection", &RuntimeSelection::default())?;
    browser_status(store, id)
}
fn ensure_no_active_runs(store: &Store, id: &str, action: &str) -> Result<()> {
    if store.list::<Run>("local", "run")?.iter().any(|r| {
        r.request.account_id == id
            && ["queued", "preparing", "submitted", "running"].contains(&r.state.as_str())
    }) {
        return Err(ServiceError::invalid(format!(
            "cancel active account runs before {action}"
        )));
    }
    Ok(())
}

fn verify_running_profile_ownership(account: &Account) -> Result<browser::RunningCdpMeta> {
    let meta = browser::inspect_running_cdp(account.port)
        .ok_or_else(|| ServiceError::invalid("cannot verify browser ownership"))?;
    if !meta.matches_account(account) {
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
    Ok(())
}

pub fn set_browser_mode(
    store: &Store,
    id: &str,
    headless: bool,
    provider: &dyn Provider,
) -> Result<BrowserStatusRecord> {
    let mut account: Account = store.get("local", "account", id)?;
    let info = provider.info();
    if !info.capabilities.browser_auth {
        return Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "browser",
            format!("provider '{}' does not use a managed browser", info.id),
        ));
    }
    ensure_no_active_runs(store, id, "switching browser mode")?;
    browser::validate_managed_profile(&account.profile).map_err(ServiceError::invalid)?;
    if browser::is_port_alive(account.port) {
        let meta = verify_running_profile_ownership(&account)?;
        if !account.headless {
            account.headless = true;
            store.put("local", "account", id, &account)?;
        }
        if meta.headless == headless {
            if !headless {
                let _ = browser::focus_browser_tab(account.port);
            }
            return browser_status(store, id);
        }
        browser::close_browser_port(account.port).map_err(ServiceError::invalid)?;
        browser::wait_for_profile_unlock(&account.profile, Duration::from_secs(5));
        invalidate_account_sessions(store, id)?;
    } else {
        if !account.headless {
            account.headless = true;
            store.put("local", "account", id, &account)?;
        }
        invalidate_account_sessions(store, id)?;
        if headless {
            return browser_status(store, id);
        }
    }
    let launch_url = provider.default_login_url().unwrap_or("about:blank");
    let opts = browser::LaunchOptions::for_account(&account, headless, launch_url);
    browser::launch_browser(&opts).map_err(|e| ServiceError::backend(e, false))?;
    browser_status(store, id)
}

pub fn login(store: &Store, id: &str, provider: &dyn Provider) -> Result<()> {
    let mut account: Account = store.get("local", "account", id)?;
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
    ensure_no_active_runs(store, id, "opening login")?;
    if store
        .list::<Session>("local", "session")?
        .iter()
        .any(|s| s.account_id == id && s.state == SessionState::Ready && s.target_id.is_some())
    {
        return Err(ServiceError::invalid(
            "close active sessions before restarting the account browser for login",
        ));
    }
    browser::validate_managed_profile(&account.profile).map_err(ServiceError::invalid)?;
    if browser::is_port_alive(account.port) {
        let _ = verify_running_profile_ownership(&account)?;
        browser::close_browser_port(account.port).map_err(ServiceError::invalid)?;
        browser::wait_for_profile_unlock(&account.profile, Duration::from_secs(5));
    }
    account.state = "awaiting_human".into();
    // Cold starts always default back to headless once this visible login window exits.
    account.headless = true;
    store.put("local", "account", id, &account)?;
    let opts = browser::LaunchOptions::for_account(&account, false, login_url);
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
        let acct = Account {
            id: "default".into(),
            provider: default_provider(),
            profile: dir.path().join("external-profile"),
            subprofile: None,
            port: 59999,
            headless: false,
            state: "ready".into(),
        };
        store.put("local", "account", &acct.id, &acct).unwrap();

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
                &make_session("s-ready", "default", SessionState::Ready, Some("t1")),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-human",
                &make_session(
                    "s-human",
                    "default",
                    SessionState::AwaitingHuman,
                    Some("t2"),
                ),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-deleted",
                &make_session("s-deleted", "default", SessionState::Deleted, Some("t3")),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-unknown",
                &make_session(
                    "s-unknown",
                    "default",
                    SessionState::OutcomeUnknown,
                    Some("t4"),
                ),
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "s-other",
                &make_session("s-other", "other", SessionState::Ready, Some("t5")),
            )
            .unwrap();

        let mut resumable_chat = make_session(
            "s-chat-resumable",
            "default",
            SessionState::Ready,
            Some("t-chat"),
        );
        resumable_chat.remote_url = Some("https://claude.ai/chat/123".into());
        resumable_chat.remote_turns = 1;
        store
            .put("local", "session", "s-chat-resumable", &resumable_chat)
            .unwrap();

        let mut temp_session =
            make_session("s-temp", "default", SessionState::Ready, Some("t-temp"));
        temp_session.mode = SessionMode::Temporary;
        temp_session.remote_url = Some("https://gemini.google.com/app/temp".into());
        store
            .put("local", "session", "s-temp", &temp_session)
            .unwrap();

        let mut human_with_url = make_session(
            "s-human-url",
            "default",
            SessionState::AwaitingHuman,
            Some("t-h"),
        );
        human_with_url.remote_url = Some("https://claude.ai/chat/456".into());
        store
            .put("local", "session", "s-human-url", &human_with_url)
            .unwrap();

        // Non-managed profile must be rejected before attempting launch or shutdown.
        let err =
            set_browser_mode(&store, "default", true, &GeminiWebProvider::default()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        invalidate_account_sessions(&store, "default").unwrap();
        let ready: Session = store.get("local", "session", "s-ready").unwrap();
        assert_eq!(ready.state, SessionState::Invalidated);
        assert!(ready.target_id.is_none());
        let chat_resumable: Session = store.get("local", "session", "s-chat-resumable").unwrap();
        assert_eq!(chat_resumable.state, SessionState::Closed);
        assert!(chat_resumable.target_id.is_none());
        assert_eq!(
            chat_resumable.remote_url.as_deref(),
            Some("https://claude.ai/chat/123")
        );
        let temp: Session = store.get("local", "session", "s-temp").unwrap();
        assert_eq!(temp.state, SessionState::Invalidated);
        assert!(temp.target_id.is_none());
        let human: Session = store.get("local", "session", "s-human").unwrap();
        assert_eq!(human.state, SessionState::Invalidated);
        assert!(human.target_id.is_none());
        let human_url: Session = store.get("local", "session", "s-human-url").unwrap();
        assert_eq!(human_url.state, SessionState::Invalidated);
        assert!(human_url.target_id.is_none());
        let deleted: Session = store.get("local", "session", "s-deleted").unwrap();
        assert_eq!(deleted.state, SessionState::Deleted);
        let unknown: Session = store.get("local", "session", "s-unknown").unwrap();
        assert_eq!(unknown.state, SessionState::OutcomeUnknown);
        let other: Session = store.get("local", "session", "s-other").unwrap();
        assert_eq!(other.state, SessionState::Ready);
        assert_eq!(other.target_id.as_deref(), Some("t5"));

        // Active run check rejects mode switching while queued/running.
        let run = Run {
            id: "run_1".into(),
            provider: default_provider(),
            request: TurnRequest::text("hi"),
            state: "running".into(),
            result: None,
            error: None,
            created_at: 1,
            updated_at: 1,
        };
        store.put("local", "run", &run.id, &run).unwrap();
        let active_err =
            ensure_no_active_runs(&store, "default", "switching browser mode").unwrap_err();
        assert_eq!(active_err.code, ErrorCode::InvalidRequest);
    }
}
