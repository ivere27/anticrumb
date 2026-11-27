use super::{client::RpcClient, codec, host::Host, pb};
use crate::{domain::*, store::Store};
use prost::Message;

fn decode_proto<T: Message + Default>(bytes: &[u8]) -> Result<T> {
    T::decode(bytes).map_err(|e| ServiceError::invalid(format!("invalid protobuf: {e}")))
}

pub(super) async fn dispatch_remote(
    host: &Host,
    remote: &RpcClient,
    path: &str,
    bytes: &[u8],
) -> Result<Option<Vec<u8>>> {
    let value = match path {
        "/anticrumb.v1.ProviderService/ListProviders" => Some(
            remote
                .list_providers(decode_proto::<pb::Empty>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.ProviderService/GetProvider" => Some(
            remote
                .get_provider(decode_proto::<pb::ProviderRef>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.ProviderService/RegisterProvider" => Some(
            remote
                .register_provider(decode_proto::<pb::RegisterProviderRequest>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.ProviderService/UnregisterProvider" => Some(
            remote
                .unregister_provider(decode_proto::<pb::ProviderRef>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.AccountService/ListAccounts" => {
            let value = remote
                .list_accounts(decode_proto::<pb::Empty>(bytes)?)
                .await?;
            for a in &value.accounts {
                host.service.store.put(
                    "local",
                    "account",
                    &a.id,
                    &codec::account_from(a.clone())?,
                )?;
            }
            Some(value.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/ListBrowsers" => {
            let value = remote
                .list_browsers(decode_proto::<pb::Empty>(bytes)?)
                .await?;
            for b in &value.browsers {
                host.service
                    .store
                    .put_browser_profile(&codec::browser_profile_from(b.clone())?)?;
            }
            Some(value.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/EnsureProviderAccount" => {
            let a = remote
                .ensure_provider_account(decode_proto::<pb::ProviderRef>(bytes)?)
                .await?;
            host.service
                .store
                .put("local", "account", &a.id, &codec::account_from(a.clone())?)?;
            if let Ok(browsers) = remote.list_browsers(pb::Empty {}).await {
                for b in browsers.browsers {
                    if let Ok(profile) = codec::browser_profile_from(b) {
                        let _ = host.service.store.put_browser_profile(&profile);
                    }
                }
            }
            Some(a.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/SetActiveAccount" => {
            let a = remote
                .set_active_account(decode_proto::<pb::AccountRef>(bytes)?)
                .await?;
            host.service
                .store
                .put("local", "account", &a.id, &codec::account_from(a.clone())?)?;
            host.service.store.set_active_account_id(&a.id)?;
            Some(a.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/RefreshAccounts" => {
            let value = remote
                .refresh_accounts(decode_proto::<pb::ProviderRef>(bytes)?)
                .await?;
            for a in &value.accounts {
                host.service.store.put(
                    "local",
                    "account",
                    &a.id,
                    &codec::account_from(a.clone())?,
                )?;
            }
            Some(value.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/AttachBrowser" => {
            let r = decode_proto::<pb::AttachBrowserRequest>(bytes)?;
            let value = remote.attach_browser(r.clone()).await?;
            for account in host.service.store.accounts_for_provider(&r.provider_id)? {
                host.service.store.put(
                    &account.id,
                    "catalog",
                    "models",
                    &ModelCatalogRecord::default(),
                )?;
            }
            if let Ok(browsers) = remote.list_browsers(pb::Empty {}).await {
                for b in browsers.browsers {
                    if let Ok(profile) = codec::browser_profile_from(b) {
                        let _ = host.service.store.put_browser_profile(&profile);
                    }
                }
            }
            Some(value.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/OpenLogin" => Some(
            remote
                .open_login(decode_proto::<pb::ProviderRef>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.AccountService/SetBrowserMode" => Some(
            remote
                .set_browser_mode(decode_proto::<pb::SetBrowserModeRequest>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.AccountService/PauseAccount" => {
            let a = remote
                .pause_account(decode_proto::<pb::AccountRef>(bytes)?)
                .await?;
            host.service
                .store
                .put("local", "account", &a.id, &codec::account_from(a.clone())?)?;
            Some(a.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/GetBrowserStatus" => Some(
            remote
                .get_browser_status(decode_proto::<pb::ProviderRef>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.AccountService/GetWebUsage" => Some(
            remote
                .get_web_usage(decode_proto::<pb::AccountRef>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.AccountService/BrowserFetch" => Some(
            remote
                .browser_fetch(decode_proto::<pb::BrowserFetchRequest>(bytes)?)
                .await?
                .encode_to_vec(),
        ),
        "/anticrumb.v1.AccountService/BrowserControl" => {
            let request = decode_proto::<pb::BrowserControlRequest>(bytes)?;
            let account = if request.account_id.is_empty() {
                host.service
                    .store
                    .resolve_active_account_id()
                    .unwrap_or_else(|_| default_account())
            } else {
                request.account_id.clone()
            };
            let mut response = remote.browser_control(request).await?;
            if let Some(shot) = response
                .page
                .as_mut()
                .and_then(|page| page.screenshot.as_mut())
            {
                // Artifact IDs belong to their host's store. Import the pixels before
                // returning an ID that the workspace can attach to the next model turn.
                shot.artifact_id = super::relay::download_artifact(
                    remote,
                    &host.service.store,
                    &account,
                    &shot.artifact_id,
                )
                .await
                .map_err(|mut error| {
                    error.message = format!(
                        "browser action completed, but screenshot transfer failed: {}; observe again before retrying the action",
                        error.message
                    );
                    error.retry = "never".into();
                    error
                })?;
                shot.png.clear();
            }
            Some(response.encode_to_vec())
        }
        "/anticrumb.v1.AccountService/ClearWebHistory"
        | "/anticrumb.v1.AccountService/GetWebHistoryCleanup" => Some(
            dispatch_clear_web_history(
                host,
                remote,
                path.ends_with("/ClearWebHistory"),
                decode_proto::<pb::AccountRef>(bytes)?,
            )
            .await?,
        ),
        "/anticrumb.v1.ModelService/RefreshCatalog" => {
            let catalog = remote
                .refresh_catalog(decode_proto::<pb::AccountRef>(bytes)?)
                .await?;
            let record = codec::catalog_from(catalog.clone());
            host.service
                .store
                .put(&catalog.account_id, "catalog", "models", &record)?;
            let mut a: Account = host
                .service
                .store
                .get("local", "account", &catalog.account_id)?;
            a.state = "ready".into();
            host.service.store.put("local", "account", &a.id, &a)?;
            Some(catalog.encode_to_vec())
        }
        _ => None,
    };
    Ok(value)
}

async fn dispatch_clear_web_history(
    host: &Host,
    remote: &RpcClient,
    starting: bool,
    request: pb::AccountRef,
) -> Result<Vec<u8>> {
    let local_cutoff = now_ms();
    if starting {
        host.service.ensure_no_active_agents(&request.account_id)?;
    }
    let info = remote.get_info(pb::Empty {}).await?;
    if info.role != "browser"
        || host
            .expected_browser_server_id()
            .is_some_and(|expected| expected != info.server_id)
    {
        return Err(ServiceError::invalid(
            "browser daemon identity changed; reconnect before clearing web history",
        ));
    }
    let status = if starting {
        remote.clear_web_history(request).await?
    } else {
        remote.get_web_history_cleanup(request).await?
    };
    let scope_id = format!("{}:{}", info.server_id, status.account_id);
    if starting {
        host.service.store.put(
            "local",
            "web_history_scope",
            &scope_id,
            &WebHistoryScope {
                started_at: status.started_at,
                local_cutoff,
            },
        )?;
    }
    if status.state == "completed"
        || status.deleted > 0 && matches!(status.state.as_str(), "failed" | "interrupted")
    {
        let scope =
            match host
                .service
                .store
                .get::<WebHistoryScope>("local", "web_history_scope", &scope_id)
            {
                Ok(scope) => Some(scope),
                Err(error) if error.code == ErrorCode::NotFound => None,
                Err(error) => return Err(error),
            };
        if let Some(scope) = scope.filter(|scope| scope.started_at == status.started_at) {
            if status.state == "completed" {
                host.service.mark_web_history_deleted(
                    &status.account_id,
                    None,
                    Some((&info.server_id, scope.local_cutoff)),
                )?;
            } else {
                // Invalidate cache reuse without claiming
                // that every session was actually deleted.
                host.service.invalidate_web_history_sessions(
                    &status.account_id,
                    &info.server_id,
                    scope.local_cutoff,
                )?;
            }
        }
        // A different workspace may have started this batch.
        // Exact remote IDs avoid guessing a local clock cutoff
        // from an old terminal status on startup.
        let deleted = remote
            .list_sessions(pb::ListSessionsRequest {
                account_id: Some(status.account_id.clone()),
                mode: pb::SessionMode::Unspecified as i32,
                state: pb::SessionState::Deleted as i32,
            })
            .await?
            .sessions
            .into_iter()
            .filter(|session| {
                session.account_id == status.account_id
                    && session.state == pb::SessionState::Deleted as i32
            })
            .map(|session| session.id)
            .collect::<std::collections::HashSet<_>>();
        for mut session in host.service.store.list::<Session>("local", "session")? {
            if session.account_id != status.account_id
                || !session
                    .remote_session_id
                    .as_ref()
                    .is_some_and(|id| deleted.contains(id))
            {
                continue;
            }
            let record =
                match host
                    .service
                    .store
                    .get::<RemoteTurnRef>(&session.id, "remote", "turn")
                {
                    Ok(record) => record,
                    Err(error) if error.code == ErrorCode::NotFound => continue,
                    Err(error) => return Err(error),
                };
            if record.server_id == info.server_id {
                session.state = SessionState::Deleted;
                host.service
                    .store
                    .put("local", "session", &session.id, &session)?;
            }
        }
    }
    let state = codec::web_history_from(&status);
    host.service
        .store
        .put("local", "web_history_cleanup", &status.account_id, &state)?;
    Ok(status.encode_to_vec())
}

pub(super) fn sync_browser_cache(store: &Store, status: &pb::HostStatus) -> Result<()> {
    for browser in &status.browsers {
        let record = codec::browser_profile_from(browser.clone())?;
        if store.browser_profile(&record.provider_id).ok().as_ref() != Some(&record) {
            store.put_browser_profile(&record)?;
        }
    }
    let mut synced_provider_logins = std::collections::HashSet::new();
    for account in &status.accounts {
        let incoming = Account {
            id: account.id.clone(),
            provider: if account.provider.is_empty() {
                default_provider()
            } else {
                account.provider.clone()
            },
            state: account.state.clone(),
            identity: account.identity.clone().filter(|s| !s.trim().is_empty()),
            label: account.label.clone().filter(|s| !s.trim().is_empty()),
            slot: account.slot.clone().filter(|s| !s.trim().is_empty()),
        };
        if store
            .get::<Account>("local", "account", &account.id)
            .ok()
            .as_ref()
            != Some(&incoming)
        {
            store.put("local", "account", &account.id, &incoming)?;
        }
        if let Some(catalog) = &account.catalog {
            let record = codec::catalog_from(catalog.clone());
            if store.model_catalog_record(&account.id)? != record {
                store.put(&account.id, "catalog", "models", &record)?;
            }
        }
        if synced_provider_logins.insert(incoming.provider.clone()) {
            let login = account.login.clone().map(codec::login_status_from);
            let saved = crate::account::provider_login_status(store, &incoming.provider);
            if saved != login {
                store.put("local", "provider_login", &incoming.provider, &login)?;
            }
        }
    }
    if let Some(active) = status
        .active_account_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        if store.active_account_id()?.as_deref() != Some(active)
            && store.get::<Account>("local", "account", active).is_ok()
        {
            store.set_active_account_id(active)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod login_cache_tests {
    use super::*;

    #[test]
    fn remote_login_catalog_is_cached_without_rewriting_unchanged_values() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let browser = BrowserProfile {
            provider_id: default_provider(),
            profile: directory.path().join("profiles/gemini-web"),
            subprofile: None,
            port: 9222,
            headless: false,
        };
        store.put_browser_profile(&browser).unwrap();
        let account = Account {
            id: "fixture".into(),
            provider: default_provider(),
            state: "awaiting_human".into(),
            identity: Some("alice@gmail.com".into()),
            label: Some("alice@gmail.com".into()),
            slot: Some("0".into()),
        };
        store
            .put("local", "account", &account.id, &account)
            .unwrap();
        let catalog = ModelCatalogRecord {
            models: vec![ModelOption {
                label: "fixture model".into(),
                mode_id: "fixture".into(),
                ..Default::default()
            }],
            checked_at: Some(123),
            ..Default::default()
        };
        let status = pb::HostStatus {
            server_id: "browser-fixture".into(),
            browsers: vec![codec::browser_profile_to(browser)],
            active_account_id: Some(account.id.clone()),
            accounts: vec![pb::AccountStatus {
                id: account.id.clone(),
                provider: account.provider.clone(),
                state: "ready".into(),
                identity: account.identity.clone(),
                label: account.label.clone(),
                slot: account.slot.clone(),
                active: true,
                selected_model: None,
                catalog: Some(codec::catalog_to(&account.id, "browser-fixture", catalog)),
                login: Some(pb::LoginStatus {
                    state: "ready".into(),
                    error: String::new(),
                }),
            }],
            ..Default::default()
        };
        sync_browser_cache(&store, &status).unwrap();
        assert_eq!(
            store
                .get::<Account>("local", "account", &account.id)
                .unwrap()
                .state,
            "ready"
        );
        assert_eq!(
            store.model_catalog_record(&account.id).unwrap().models[0].label,
            "fixture model"
        );
        assert_eq!(
            crate::account::provider_login_status(&store, &account.provider)
                .unwrap()
                .state,
            "ready"
        );
        assert_eq!(
            store.active_account_id().unwrap().as_deref(),
            Some(account.id.as_str())
        );
        let before = store.total_changes();
        sync_browser_cache(&store, &status).unwrap();
        assert_eq!(store.total_changes(), before);
        let mut cleared = status;
        cleared.accounts[0].login = None;
        sync_browser_cache(&store, &cleared).unwrap();
        assert!(crate::account::provider_login_status(&store, &account.provider).is_none());
    }
}
