//! Script-defined web history maintenance for manifest providers.
use super::*;
use serde_json::Value;

fn failure(message: impl Into<String>) -> ServiceError {
    ServiceError::backend(message.into(), false)
}

struct Tab {
    port: u16,
    target: browser::TargetInfo,
    close: bool,
    _lease: tab_pool::SystemTabLease,
}

impl Drop for Tab {
    fn drop(&mut self) {
        if self.close {
            let _ = browser::close_tab(self.port, &self.target.id);
        }
    }
}

impl Tab {
    fn acquire(
        provider: &ManifestWebProvider,
        ctx: &ProviderContext<'_>,
        account: &Account,
        cancel: &CancelToken,
    ) -> Result<Self> {
        if account.provider != provider.manifest.id {
            return Err(ServiceError::invalid(
                "web cleanup provider/account mismatch",
            ));
        }
        // Resolve both adapters before opening a browser or touching history.
        provider.script("delete-conversation.js")?;
        provider.script("read-history.js")?;
        cancel.check().map_err(failure)?;
        let config = provider.web_chat_config()?;
        let host = browser::ensure_browser(&browser::LaunchOptions::headless_account(
            account,
            &config.entry_url,
        ))
        .map_err(failure)?;
        let reserved = ctx.store.and_then(|store| {
            web_chat::with_tab_pool_matcher(&config, |matcher| {
                tab_pool::reserve_system_target_for(store, &account.id, host.port, matcher)
            })
        });
        let (target, lease, created) = match reserved {
            Some((target, lease)) => (target, lease, false),
            None => {
                let target = browser::create_tab(host.port, "about:blank").map_err(failure)?;
                let lease = tab_pool::SystemTabLease::acquire(target.id.clone());
                (target, lease, true)
            }
        };
        let persistent = ctx
            .store
            .is_some_and(|store| tab_pool::record_system_tab_id(store, &account.id, &target.id));
        let tab = Self {
            port: host.port,
            target,
            close: created && !persistent,
            _lease: lease,
        };
        if !config.is_reusable_url(&tab.target.url) {
            crate::gemini::navigate_tab_commit(
                tab.port,
                &tab.target,
                &config.entry_url,
                cancel,
                Instant::now() + Duration::from_secs(30),
            )
            .map_err(failure)?;
        }
        Ok(tab)
    }

    fn client(&self, cancel: &CancelToken, deadline: Instant) -> Result<CdpClient> {
        let mut cdp = CdpClient::connect(
            self.port,
            &browser::ws_path_from_url(&self.target.ws_url).map_err(failure)?,
        )
        .map_err(failure)?;
        cdp.set_control(cancel.clone(), deadline);
        Ok(cdp)
    }
}

fn evaluate(cdp: &mut CdpClient, script: &str, args: Value) -> Result<Value> {
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression":format!("({script})({args})"),"returnByValue":true,"awaitPromise":true
            }),
        )
        .map_err(failure)?;
    if result.get("exceptionDetails").is_some() {
        return Err(failure("web history adapter failed"));
    }
    let value = result["result"]["value"].clone();
    match value["state"].as_str() {
        Some("auth_required") => Err(ServiceError::new(
            ErrorCode::AuthRequired,
            "web_history",
            "sign in before deleting web history",
        )),
        Some("verification_required") => Err(ServiceError::new(
            ErrorCode::VerificationRequired,
            "web_history",
            "complete the provider's security verification in Chrome",
        )),
        Some("error") => Err(failure(
            value["error"]
                .as_str()
                .unwrap_or("web history adapter contract changed"),
        )),
        Some(_) => Ok(value),
        None => Err(failure("web history adapter returned no state")),
    }
}

fn exact_url(provider: &ManifestWebProvider, raw: &str) -> Result<String> {
    let config = provider.web_chat_config()?;
    let url =
        Url::parse(raw).map_err(|_| ServiceError::invalid("invalid recorded conversation URL"))?;
    if !config.is_chat_url(raw)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url
            .path_segments()
            .and_then(|mut parts| parts.next_back())
            .is_some_and(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            })
    {
        return Err(ServiceError::invalid(
            "web cleanup requires an exact provider conversation URL",
        ));
    }
    Ok(url.to_string())
}

fn snapshot(
    provider: &ManifestWebProvider,
    tab: &Tab,
    cancel: &CancelToken,
    scope: Option<&str>,
) -> Result<(String, Vec<String>)> {
    let deadline = Instant::now() + Duration::from_secs(65);
    let mut cdp = tab.client(cancel, deadline)?;
    let value = evaluate(
        &mut cdp,
        &provider.script("read-history.js")?,
        json!({"scope":scope,"timeoutMs":60000}),
    )?;
    if value["state"] != "ready" || value["complete"] != true {
        return Err(failure("complete web history could not be established"));
    }
    let observed = value["scope"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| failure("web history account scope missing"))?;
    if scope.is_some_and(|expected| expected != observed) {
        return Err(failure("web history switched provider accounts"));
    }
    let urls = value["urls"]
        .as_array()
        .ok_or_else(|| failure("web history URL list missing"))?;
    if urls.len() > 100_000 {
        return Err(failure("web history exceeds enumeration limit"));
    }
    let urls = urls
        .iter()
        .map(|v| {
            exact_url(
                provider,
                v.as_str()
                    .ok_or_else(|| failure("invalid web conversation URL"))?,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((observed.to_owned(), urls))
}

fn delete_in_tab(
    provider: &ManifestWebProvider,
    tab: &Tab,
    cancel: &CancelToken,
    scope: &str,
    url: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let script = provider.script("delete-conversation.js")?;
    let mut cdp = tab.client(cancel, deadline)?;
    let mut navigated = false;
    let mut last_step = String::new();
    while Instant::now() < deadline {
        cancel.check().map_err(failure)?;
        let value = evaluate(&mut cdp, &script, json!({"url":url,"scope":scope}))?;
        if let Some(step) = value["step"].as_str() {
            last_step = step.to_owned();
        }
        match value["state"].as_str() {
            Some("deleted") | Some("missing_on_page") => {
                drop(cdp);
                // The list service may briefly lag a successful UI deletion.
                // Retry only fresh reads; never repeat the destructive action.
                for attempt in 0..4 {
                    let (_, remaining) = snapshot(provider, tab, cancel, Some(scope))?;
                    if !remaining.iter().any(|remaining| remaining == url) {
                        return Ok(());
                    }
                    if attempt < 3 {
                        cancel.sleep_or_cancelled(Duration::from_millis(500 << attempt));
                    }
                }
                return Err(failure(
                    "conversation still exists; web deletion was not confirmed",
                ));
            }
            Some("needs_navigation") if !navigated => {
                drop(cdp);
                // A deep/archived chat may not be rendered in the sidebar.
                // Keep the same tab, opening only this exact recorded URL.
                crate::gemini::navigate_tab_commit(tab.port, &tab.target, url, cancel, deadline)
                    .map_err(failure)?;
                snapshot(provider, tab, cancel, Some(scope))?;
                cdp = tab.client(cancel, deadline)?;
                navigated = true;
            }
            Some("needs_navigation") => {
                return Err(failure(
                    "recorded conversation has no supported delete control",
                ))
            }
            _ => {}
        }
        cancel.sleep_or_cancelled(Duration::from_millis(180));
    }
    Err(failure(format!(
        "web deletion was not confirmed before the deadline ({last_step})"
    )))
}

pub(super) fn delete(
    provider: &ManifestWebProvider,
    ctx: &ProviderContext<'_>,
    account: &Account,
    session: &Session,
    cancel: CancelToken,
) -> Result<()> {
    if session.account_id != account.id || session.provider != account.provider {
        return Err(ServiceError::invalid(
            "web cleanup account does not match recorded session",
        ));
    }
    if session.mode == SessionMode::Temporary
        || (session.remote_url.is_none() && session.remote_turns == 0)
    {
        return Ok(());
    }
    let url = exact_url(
        provider,
        session
            .remote_url
            .as_deref()
            .ok_or_else(|| ServiceError::invalid("recorded web conversation URL missing"))?,
    )?;
    let tab = Tab::acquire(provider, ctx, account, &cancel)?;
    let (scope, urls) = snapshot(provider, &tab, &cancel, None)?;
    if !urls.contains(&url) {
        return Ok(());
    }
    delete_in_tab(provider, &tab, &cancel, &scope, &url)
}

pub(super) fn clear(
    provider: &ManifestWebProvider,
    ctx: &ProviderContext<'_>,
    account: &Account,
    cancel: CancelToken,
    progress: &mut dyn FnMut(&str, u32) -> Result<()>,
) -> Result<u32> {
    let tab = Tab::acquire(provider, ctx, account, &cancel)?;
    let (scope, urls) = snapshot(provider, &tab, &cancel, None)?;
    let mut deleted = 0;
    for url in urls {
        delete_in_tab(provider, &tab, &cancel, &scope, &url)?;
        deleted += 1;
        progress(&url, deleted)?;
        cancel.sleep_or_cancelled(Duration::from_millis(500));
    }
    if !snapshot(provider, &tab, &cancel, Some(&scope))?
        .1
        .is_empty()
    {
        return Err(failure(
            "web history still contains conversations; retry cleanup explicitly",
        ));
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "deletes only explicitly supplied live test-chat URLs in a selected maintenance tab"]
    fn live_delete_test_chats_reuses_document_and_preserves_other_history() {
        let id = std::env::var("ANTICRUMB_LIVE_PROVIDER").unwrap();
        let manifest: ProviderManifest =
            serde_json::from_str(crate::provider::bundled::manifest(&id).unwrap()).unwrap();
        let provider = ManifestWebProvider::bundled(manifest).unwrap();
        let port = std::env::var("ANTICRUMB_LIVE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let target_id = std::env::var("ANTICRUMB_LIVE_TARGET").unwrap();
        let urls: Vec<String> =
            serde_json::from_str(&std::env::var("ANTICRUMB_LIVE_DELETE_URLS").unwrap()).unwrap();
        let allow_navigation =
            std::env::var("ANTICRUMB_LIVE_ALLOW_NAVIGATION").as_deref() == Ok("1");
        assert!(!urls.is_empty());
        let target = browser::list_targets(port)
            .unwrap()
            .into_iter()
            .find(|t| t.id == target_id)
            .unwrap();
        let tab = Tab {
            port,
            _lease: tab_pool::SystemTabLease::acquire(target.id.clone()),
            target,
            close: false,
        };
        let cancel = CancelToken::new();
        let (scope, before) = snapshot(&provider, &tab, &cancel, None).unwrap();
        let mut cdp = tab
            .client(&cancel, Instant::now() + Duration::from_secs(10))
            .unwrap();
        cdp.call(
            "Runtime.evaluate",
            json!({"expression":"window.__anticrumbLiveDeleteDocument=true"}),
        )
        .unwrap();
        drop(cdp);
        for url in &urls {
            assert!(before.contains(url), "test chat must exist before deletion");
            delete_in_tab(&provider, &tab, &cancel, &scope, url).unwrap();
        }
        let (_, after) = snapshot(&provider, &tab, &cancel, Some(&scope)).unwrap();
        let expected: std::collections::BTreeSet<_> = before
            .into_iter()
            .filter(|url| !urls.contains(url))
            .collect();
        assert_eq!(
            after.into_iter().collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        let mut cdp = tab
            .client(&cancel, Instant::now() + Duration::from_secs(10))
            .unwrap();
        let result=cdp.call("Runtime.evaluate",json!({"expression":"window.__anticrumbLiveDeleteDocument === true","returnByValue":true})).unwrap();
        if !allow_navigation {
            assert_eq!(
                result["result"]["value"], true,
                "deletion must not reload the maintenance document"
            );
        }
        eprintln!(
            "LIVE {id}: {} test chats deleted; same {} retained; all other chats preserved",
            urls.len(),
            if allow_navigation { "tab" } else { "document" }
        );
    }
}
