use crate::{
    agent::CancelToken,
    browser_host as browser,
    cdp::CdpClient,
    domain::{
        Account, BrowserProfile, ErrorCode, Result, ServiceError, Session, SessionMode,
        SessionState,
    },
    provider::AccountRoute,
};
use serde_json::json;
use std::time::{Duration, Instant};

fn recorded_url(session: &Session) -> Result<crate::url_lite::Url> {
    let raw = session.remote_url.as_deref().ok_or_else(|| {
        ServiceError::new(
            ErrorCode::NotFound,
            "chat_cleanup_identity",
            "web cleanup has no recorded conversation URL",
        )
    })?;
    super::url::parse_gemini_chat_url(raw).map_err(|err| {
        ServiceError::invalid(if err == "invalid recorded conversation URL" {
            "invalid recorded conversation URL"
        } else {
            "web cleanup requires an exact recorded Gemini conversation URL"
        })
    })
}

fn rebased_recorded_url(session: &Session, slot: Option<&str>) -> Result<crate::url_lite::Url> {
    let parsed = recorded_url(session)?;
    if slot.is_some() {
        let rebased = super::url::rebase_gemini_chat_url(parsed.as_str(), slot).map_err(|_| {
            ServiceError::invalid("web cleanup requires an exact recorded Gemini conversation URL")
        })?;
        return crate::url_lite::Url::parse(&rebased).map_err(ServiceError::storage);
    }
    Ok(parsed)
}

fn account_route(account: &Account) -> AccountRoute {
    let slot = super::url::normalized_slot(account.slot.as_deref());
    AccountRoute {
        entry_url: super::url::gemini_app_url(Some(slot)),
        usage_url: Some(super::url::gemini_usage_url(Some(slot))),
        scope: Some(slot.to_owned()),
    }
}

fn verify_target_identity(
    port: u16,
    target: &browser::TargetInfo,
    account: &Account,
    cancel: &CancelToken,
) -> Result<()> {
    let expected_identity = account
        .identity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let expected_slot = account
        .slot
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if expected_identity.is_none() && expected_slot.is_none() {
        return Ok(());
    }
    let mut cdp = CdpClient::connect(
        port,
        &browser::ws_path_from_url(&target.ws_url).map_err(ServiceError::storage)?,
    )
    .map_err(|e| ServiceError::backend(e, false))?;
    cdp.set_control(cancel.clone(), Instant::now() + Duration::from_secs(5));
    let script = crate::adapter::script("driver.js").map_err(ServiceError::storage)?;
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": format!("({script})({})", json!({"op": "probe"})),
                "returnByValue": true,
                "awaitPromise": true
            }),
        )
        .map_err(|e| ServiceError::backend(e, false))?;
    if result.get("exceptionDetails").is_some() {
        return Ok(());
    }
    let probe = &result["result"]["value"];
    if let Some(expected) = expected_identity {
        if let Some(observed) = probe["identity"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !observed.eq_ignore_ascii_case(expected) {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "identity_mismatch",
                    "browser tab switched to a different account; rescanning accounts",
                ));
            }
        }
    }
    if let Some(slot) = expected_slot {
        let observed_slot = probe["slot"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| probe["url"].as_str().and_then(super::url::gemini_url_scope));
        if observed_slot.as_deref().is_some_and(|obs| obs != slot) {
            return Err(ServiceError::new(
                ErrorCode::AuthRequired,
                "identity_mismatch",
                "browser tab switched to a different account; rescanning accounts",
            ));
        }
    }
    Ok(())
}

fn verify_absence(url: &crate::url_lite::Url, remaining: &[String]) -> Result<()> {
    let conversation = url.path_segments().and_then(|mut s| s.next_back());
    if remaining.iter().any(|raw| {
        crate::url_lite::Url::parse(raw).ok().is_some_and(|other| {
            other.path_segments().and_then(|mut s| s.next_back()) == conversation
        })
    }) {
        return Err(ServiceError::backend(
            "the conversation still exists after the delete action; web deletion was not confirmed"
                .into(),
            false,
        ));
    }
    Ok(())
}

/// Snapshot the complete account history before deletion, then independently
/// enumerate it again. Never call an incomplete sidebar view "all deleted".
pub fn clear_web_history(
    browser_profile: &BrowserProfile,
    account: &Account,
    cancel: CancelToken,
    progress: &mut dyn FnMut(&str, u32) -> Result<()>,
) -> Result<u32> {
    clear_web_history_with_store(None, browser_profile, account, cancel, progress)
}

/// Snapshot and delete all account web conversations inside one serialized
/// system Gemini tab when `store` provides an unowned target.
pub fn clear_web_history_with_store(
    store: Option<&crate::store::Store>,
    browser_profile: &BrowserProfile,
    account: &Account,
    cancel: CancelToken,
    progress: &mut dyn FnMut(&str, u32) -> Result<()>,
) -> Result<u32> {
    cancel
        .check()
        .map_err(|e| ServiceError::backend(e, false))?;
    let route = account_route(account);
    let host = browser::ensure_browser(&browser::LaunchOptions::headless_browser(
        browser_profile,
        &route.entry_url,
    ))
    .map_err(|e| ServiceError::backend(e, false))?;
    let tab = super::SystemGeminiTab::acquire_for_route(
        store,
        &account.id,
        host.port,
        &route,
        "about:blank",
    )
    .map_err(|e| ServiceError::backend(e, false))?;
    let urls = super::history::list_web_conversations_in_target(
        host.port,
        &tab.target,
        cancel.clone(),
        &route.entry_url,
    )?;
    verify_target_identity(host.port, &tab.target, account, &cancel)?;
    let mut deleted = 0;
    for (index, url) in urls.into_iter().enumerate() {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        if index > 0
            && cancel.sleep_or_cancelled(Duration::from_millis(super::jitter_u64(400, 900)))
        {
            return Err(ServiceError::backend("cancelled".into(), false));
        }
        let session = Session {
            id: "explicit-history-cleanup".into(),
            provider: account.provider.clone(),
            account_id: account.id.clone(),
            mode: SessionMode::Chat,
            target_id: None,
            remote_url: Some(url.clone()),
            remote_session_id: None,
            history: Vec::new(),
            state: SessionState::Closed,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 0,
            last_used: crate::domain::now_ms(),
            reuse_scope: None,
        };
        delete_web_conversation_in_target(
            account,
            host.port,
            &tab.target,
            &session,
            cancel.clone(),
        )?;
        deleted += 1;
        progress(&url, deleted)?;
    }
    let remaining = super::history::list_web_conversations_in_target(
        host.port,
        &tab.target,
        cancel,
        &route.entry_url,
    )?;
    if !remaining.is_empty() {
        return Err(ServiceError::backend(format!(
            "web history verification found {} remaining conversations after {deleted} confirmed deletions; retry explicitly to remove them", remaining.len()
        ), false));
    }
    Ok(deleted)
}

/// Delete only the recorded web conversation.
/// The caller serializes this operation through the account gate.
pub fn delete_web_conversation(
    browser_profile: &BrowserProfile,
    account: &Account,
    session: &Session,
    cancel: CancelToken,
) -> Result<()> {
    delete_web_conversation_with_store(None, browser_profile, account, session, cancel)
}

/// Delete the recorded web conversation inside an existing unowned Gemini tab
/// when available, avoiding repeated Chrome tab open/close churn.
pub fn delete_web_conversation_with_store(
    store: Option<&crate::store::Store>,
    browser_profile: &BrowserProfile,
    account: &Account,
    session: &Session,
    cancel: CancelToken,
) -> Result<()> {
    if account.id != session.account_id {
        return Err(ServiceError::invalid(
            "web cleanup account does not match the recorded session",
        ));
    }
    if session.mode == SessionMode::Temporary
        || (session.remote_url.is_none() && session.remote_turns == 0)
    {
        return Ok(());
    }
    let url = rebased_recorded_url(session, account.slot.as_deref())?;
    let mut home = url.clone();
    home.set_path(url.path().rsplit_once('/').unwrap().0);
    home.set_fragment(None);
    cancel
        .check()
        .map_err(|e| ServiceError::backend(e, false))?;
    let route = account_route(account);
    let host = browser::ensure_browser(&browser::LaunchOptions::headless_browser(
        browser_profile,
        home.as_str(),
    ))
    .map_err(|e| ServiceError::backend(e, false))?;
    let tab = super::SystemGeminiTab::acquire_for_route(
        store,
        &account.id,
        host.port,
        &route,
        "about:blank",
    )
    .map_err(|e| ServiceError::backend(e, false))?;
    delete_web_conversation_in_target(account, host.port, &tab.target, session, cancel)
}

fn delete_web_conversation_in_target(
    account: &Account,
    host_port: u16,
    target: &browser::TargetInfo,
    session: &Session,
    cancel: CancelToken,
) -> Result<()> {
    if account.id != session.account_id {
        return Err(ServiceError::invalid(
            "web cleanup account does not match the recorded session",
        ));
    }
    if session.mode == SessionMode::Temporary
        || (session.remote_url.is_none() && session.remote_turns == 0)
    {
        return Ok(());
    }
    let url = rebased_recorded_url(session, account.slot.as_deref())?;
    cancel
        .check()
        .map_err(|e| ServiceError::backend(e, false))?;
    let mut base = url.clone();
    base.set_path(url.path().rsplit_once('/').unwrap().0);
    base.set_fragment(None);
    super::history::prepare_web_history_in_target(
        host_port,
        target,
        cancel.clone(),
        base.as_str(),
    )?;
    verify_target_identity(host_port, target, account, &cancel)?;
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut cdp = CdpClient::connect(
        host_port,
        &browser::ws_path_from_url(&target.ws_url).map_err(ServiceError::storage)?,
    )
    .map_err(|e| ServiceError::backend(e, false))?;
    cdp.set_control(cancel.clone(), deadline);
    let script = crate::adapter::script("delete-conversation.js").map_err(ServiceError::storage)?;
    let expression = format!("({script})({})", json!({"url":url.as_str()}));
    let mut step = "conversation page".to_owned();
    while Instant::now() < deadline {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let result = cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true}),
            )
            .map_err(|e| ServiceError::backend(e, false))?;
        if result.get("exceptionDetails").is_some() {
            return Err(ServiceError::backend(
                "web cleanup adapter failed to inspect the page".into(),
                false,
            ));
        }
        let value = &result["result"]["value"];
        match value["state"].as_str() {
            Some("deleted") | Some("missing_on_page") => {
                // The web UI can optimistically hide a row even if its request
                // fails, or redirect away from an already-deleted chat ID to /app.
                // Confirm absence with a fresh complete server-backed history read
                // before committing any irreversible progress.
                drop(cdp);
                for attempt in 0..4 {
                    let remaining = super::history::list_web_conversations_in_target(
                        host_port,
                        target,
                        cancel.clone(),
                        base.as_str(),
                    )?;
                    match verify_absence(&url, &remaining) {
                        Ok(()) => return Ok(()),
                        Err(error) if attempt == 3 => return Err(error),
                        Err(_) => {
                            cancel.sleep_or_cancelled(Duration::from_millis(500 << attempt));
                        }
                    }
                }
                unreachable!();
            }
            Some("auth_required") => {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "web_cleanup",
                    "sign in to the recorded account before retrying web cleanup",
                ))
            }
            Some("verification_required") => {
                return Err(ServiceError::new(
                    ErrorCode::VerificationRequired,
                    "web_cleanup",
                    "complete Google's human verification in Chrome, then retry web cleanup",
                ));
            }
            Some("error") => {
                return Err(ServiceError::backend(
                    value["error"]
                        .as_str()
                        .unwrap_or("web cleanup could not identify the expected controls")
                        .into(),
                    false,
                ))
            }
            _ => {
                if let Some(next) = value["step"].as_str() {
                    step = next.into();
                }
            }
        }
        cancel.sleep_or_cancelled(Duration::from_millis(super::jitter_u64(130, 210)));
    }
    Err(ServiceError::new(ErrorCode::DeadlineExceeded,"web_cleanup",format!("web deletion was not confirmed while waiting for {step}; inspect the chat before retrying")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "deletes only explicitly supplied live test-chat URLs in a selected maintenance tab"]
    fn live_delete_test_chats_reuses_document_and_preserves_other_history() {
        let port = std::env::var("ANTICRUMB_LIVE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let target_id = std::env::var("ANTICRUMB_LIVE_TARGET").unwrap();
        let urls: Vec<String> =
            serde_json::from_str(&std::env::var("ANTICRUMB_LIVE_DELETE_URLS").unwrap()).unwrap();
        assert!(!urls.is_empty());
        let target = browser::list_targets(port)
            .unwrap()
            .into_iter()
            .find(|t| t.id == target_id)
            .unwrap();
        let store = crate::store::Store::open(&crate::cli::root()).unwrap();
        let account: Account = store.get("local", "account", "default").unwrap();
        let cancel = CancelToken::new();
        let before = super::super::history::list_web_conversations_in_target(
            port,
            &target,
            cancel.clone(),
            browser::GEMINI_APP_URL,
        )
        .unwrap();
        let mut cdp =
            CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
        cdp.call(
            "Runtime.evaluate",
            json!({"expression":"window.__anticrumbLiveDeleteDocument = true"}),
        )
        .unwrap();
        drop(cdp);
        for url in &urls {
            assert!(before.contains(url), "test chat must exist before deletion");
            let session: Session = serde_json::from_value(json!({"id":"explicit-live-test","provider":"gemini-web","account_id":"default","mode":"chat","remote_url":url,"state":"closed","history":[],"tool_digest":""})).unwrap();
            delete_web_conversation_in_target(&account, port, &target, &session, cancel.clone())
                .unwrap();
        }
        let after = super::super::history::list_web_conversations_in_target(
            port,
            &target,
            cancel,
            browser::GEMINI_APP_URL,
        )
        .unwrap();
        let expected: std::collections::BTreeSet<_> = before
            .into_iter()
            .filter(|url| !urls.contains(url))
            .collect();
        assert_eq!(
            after.into_iter().collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        let mut cdp =
            CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
        let result = cdp.call("Runtime.evaluate", json!({"expression":"window.__anticrumbLiveDeleteDocument === true","returnByValue":true})).unwrap();
        assert_eq!(
            result["result"]["value"], true,
            "deletion must not reload the maintenance document"
        );
        eprintln!(
            "LIVE Gemini: {} test chats deleted; same document retained; all other chats preserved",
            urls.len()
        );
    }
    #[test]
    fn optimistic_ui_removal_is_rejected_when_fresh_history_still_contains_the_chat() {
        let url = crate::url_lite::Url::parse("https://gemini.google.com/app/target").unwrap();
        let remaining = vec!["https://gemini.google.com/u/0/app/target".into()];
        assert!(verify_absence(&url, &remaining)
            .unwrap_err()
            .message
            .contains("not confirmed"));
        assert!(verify_absence(&url, &["https://gemini.google.com/app/other".into()]).is_ok());
        assert!(verify_absence(&url, &[]).is_ok());
    }
    #[test]
    fn deletion_url_requires_exact_gemini_chat_and_never_accepts_other_origins() {
        for (url, valid) in [
            ("https://gemini.google.com/app/abc123", true),
            ("https://gemini.google.com/u/1/app/abc123", true),
            ("https://gemini.google.com/app", false),
            ("https://gemini.google.com/usage", false),
            ("https://gemini.google.com.evil.test/app/abc", false),
            ("http://gemini.google.com/app/abc", false),
            ("https://user:secret@gemini.google.com/app/abc", false),
            ("https://gemini.google.com/app/a%2Fb", false),
        ] {
            let session: Session = serde_json::from_value(json!({"id":"s","provider":"gemini-web","account_id":"a","mode":"chat","target_id":null,"remote_url":url,"history":[],"state":"closed","model":null,"thinking":null,"tool_digest":""})).unwrap();
            assert_eq!(recorded_url(&session).is_ok(), valid, "{url}");
        }
    }
}
