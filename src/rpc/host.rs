use super::{
    client::RpcClient, codec, host_sync, pb, stream_chunks_to_sink, ChunkPayload, ChunkStreamSpec,
    ARTIFACT_CHUNK_BYTES, MAX_ARTIFACT, MAX_MESSAGE, MAX_MESSAGE_LABEL,
};
use crate::{domain::*, provider::ProviderRegistry, service::Service, store::Store};
use fs2::FileExt;
use futures_util::{Stream, StreamExt};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, fs, io::Read as _, path::Path, sync::Arc};

pub struct Host {
    pub service: Arc<Service>,
    pub providers: Arc<ProviderRegistry>,
    pub remote: Option<RpcClient>,
    browser_connection: Option<pb::BrowserConnection>,
    agents: Option<Arc<crate::agent_service::AgentManager>>,
    logins: Arc<crate::account::login::Monitors>,
    browser_control: Arc<crate::browser_control::BrowserControlRegistry>,
    usage_gates: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
    pub server_id: String,
    pub role: String,
    pub _lease: Option<fs::File>,
    pub(super) cache_cancel: crate::agent::CancelToken,
}
impl Host {
    pub fn browser(root: &Path) -> Result<Arc<Self>> {
        let providers_dir = crate::browser_host::anticrumb_home().join("providers");
        Self::browser_with_providers_dir(root, Some(&providers_dir))
    }

    pub fn browser_with_providers_dir(
        root: &Path,
        providers_dir: Option<&Path>,
    ) -> Result<Arc<Self>> {
        let store = Store::open(root)?;
        let lease = Self::lease(root)?;
        let providers = ProviderRegistry::with_builtins();
        if let Some(dir) = providers_dir {
            providers.load_providers_dir(dir)?;
        }
        providers.load_persisted_manifests(&store)?;
        if store.get::<Account>("local", "account", "default").is_err() {
            store.put(
                "local",
                "account",
                "default",
                &Account {
                    id: "default".into(),
                    provider: default_provider(),
                    profile: crate::browser_host::default_profile_dir(),
                    subprofile: None,
                    port: 9222,
                    headless: true,
                    state: "auth_required".into(),
                },
            )?;
        }
        let backend = Arc::new(crate::service::WebBackend::with_providers(
            store.clone(),
            providers.clone(),
        ));
        let service = Service::new(store, backend)?;
        Self::with_service_and_providers(service, None, "browser", Some(lease), providers)
    }
    pub fn lease(root: &Path) -> Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("host.lock"))
            .map_err(ServiceError::storage)?;
        file.try_lock_exclusive()
            .map_err(|_| ServiceError::invalid(format!(
                "another daemon owns state directory {}; connect with `anticrumb`, or set ANTICRUMB_HOME for a separate daemon", root.display()
            )))?;
        Ok(file)
    }
    pub fn with_service(
        service: Arc<Service>,
        remote: Option<RpcClient>,
        role: &str,
        lease: Option<fs::File>,
    ) -> Result<Arc<Self>> {
        Self::with_service_and_providers(
            service,
            remote,
            role,
            lease,
            ProviderRegistry::with_builtins(),
        )
    }
    pub fn with_service_and_providers(
        service: Arc<Service>,
        remote: Option<RpcClient>,
        role: &str,
        lease: Option<fs::File>,
        providers: Arc<ProviderRegistry>,
    ) -> Result<Arc<Self>> {
        Self::with_connection(service, remote, role, lease, None, providers)
    }
    pub(super) fn with_connection(
        service: Arc<Service>,
        remote: Option<RpcClient>,
        role: &str,
        lease: Option<fs::File>,
        browser_connection: Option<pb::BrowserConnection>,
        providers: Arc<ProviderRegistry>,
    ) -> Result<Arc<Self>> {
        if role == "browser" {
            crate::account::login::Monitors::recover(&service.store)?;
        }
        let server_id = match service.store.get::<String>("local", "host", "identity") {
            Ok(id) => id,
            Err(error) if error.code == ErrorCode::NotFound => {
                let id = id("host");
                service.store.put("local", "host", "identity", &id)?;
                id
            }
            Err(error) => return Err(error),
        };
        let agents = if role == "workspace" {
            Some(crate::agent_service::AgentManager::new(
                service.store.clone(),
            )?)
        } else {
            None
        };
        let host = Arc::new(Self {
            agents,
            logins: Arc::new(crate::account::login::Monitors::default()),
            browser_control: Arc::new(crate::browser_control::BrowserControlRegistry::default()),
            usage_gates: Default::default(),
            service,
            providers,
            remote,
            browser_connection,
            server_id,
            role: role.into(),
            _lease: lease,
            cache_cancel: crate::agent::CancelToken::new(),
        });
        if role == "workspace" {
            host.service.start_chat_cleanup();
        }
        if host._lease.is_some() {
            host.start_cache_maintenance();
        }
        Ok(host)
    }
    pub(super) fn expected_browser_server_id(&self) -> Option<&str> {
        self.browser_connection
            .as_ref()
            .map(|c| c.server_id.as_str())
    }
    fn usage_gate(&self, id: &str) -> Arc<tokio::sync::Semaphore> {
        self.usage_gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.into())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
    }
    fn agents(&self) -> Result<&Arc<crate::agent_service::AgentManager>> {
        self.agents.as_ref().ok_or_else(|| {
            ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "workspace",
                "workspace service is absent from this host",
            )
        })
    }
    pub fn shutdown(&self) {
        self.cache_cancel.cancel();
        self.logins.cancel_all(&self.service.store);
        self.browser_control.close_all();
        if let Some(agents) = &self.agents {
            agents.cancel_all();
        }
        self.service.begin_shutdown();
    }
    pub fn shutdown_managed_browsers(&self) {
        if self.role != "browser" {
            return;
        }
        let accounts = self
            .service
            .store
            .list::<Account>("local", "account")
            .unwrap_or_default();
        let closed_ids = crate::browser_host::shutdown_managed_browsers(&accounts);
        for id in closed_ids {
            let _ = crate::account::invalidate_account_sessions(&self.service.store, &id);
        }
    }
    pub fn info(&self) -> pb::HostInfo {
        let mut services = vec![
            "PlatformService",
            "ProviderService",
            "AccountService",
            "ModelService",
            "TurnService",
            "SessionService",
            "ArtifactService",
        ];
        if self.role == "workspace" {
            services.push("WorkspaceService");
        }
        pb::HostInfo {
            server_id: self.server_id.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            platform: std::env::consts::OS.into(),
            services: services
                .into_iter()
                .map(|s| format!("anticrumb.v1.{s}"))
                .collect(),
            max_artifact_bytes: MAX_ARTIFACT as u32,
            max_message_bytes: MAX_MESSAGE as u32,
            role: self.role.clone(),
        }
    }
    async fn browser_connection_status(
        &self,
        host_status: &mut pb::HostStatus,
    ) -> Option<pb::BrowserConnection> {
        let mut status = self.browser_connection.clone()?;
        status.connected = false;
        let Some(remote) = &self.remote else {
            status.error = "Browser daemon connection is unavailable".into();
            return Some(status);
        };
        match tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::try_join!(
                remote.get_info(pb::Empty {}),
                remote.get_status(pb::Empty {})
            )
        })
        .await
        {
            Ok(Ok((info, cached)))
                if info.role == "browser"
                    && info.server_id == status.server_id
                    && cached.server_id == status.server_id =>
            {
                // This only copies the daemon's stored catalog and login state.
                // Polling status never opens Chrome or discovers models.
                let sync = cached.clone();
                match self
                    .service
                    .store
                    .blocking(move |store| host_sync::sync_browser_cache(&store, &sync))
                    .await
                {
                    Ok(()) => {
                        host_status.accounts = cached.accounts;
                        host_status.providers = cached.providers;
                        status.connected = true;
                        status.error.clear();
                    }
                    Err(_) => {
                        status.error =
                            "Browser daemon is reachable, but its cached status could not be saved"
                                .into()
                    }
                }
            }
            Ok(Ok(_)) => {
                status.error =
                    "Browser daemon identity changed; reconnect the workspace host".into();
            }
            Ok(Err(error)) if error.code == ErrorCode::AuthRequired => {
                status.error =
                    "Browser daemon authentication failed; check the connection token".into();
            }
            Ok(Err(_)) => {
                status.error = "Browser daemon connection failed".into();
            }
            Err(_) => {
                status.error = "Browser daemon did not respond within 2 seconds".into();
            }
        }
        Some(status)
    }
    pub async fn dispatch(self: &Arc<Self>, path: &str, bytes: Vec<u8>) -> Result<Vec<u8>> {
        if bytes.len() > MAX_MESSAGE {
            return Err(ServiceError::invalid(format!(
                "RPC message exceeds {MAX_MESSAGE_LABEL}"
            )));
        }
        if path.starts_with("/anticrumb.v1.WorkspaceService/") && self.role != "workspace" {
            return Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "rpc",
                "workspace tools are not available on a browser host",
            ));
        }
        macro_rules! decode {
            ($t:ty) => {
                <$t>::decode(bytes.as_slice())
                    .map_err(|e| ServiceError::invalid(format!("invalid protobuf: {e}")))?
            };
        }
        if let Some(remote) = &self.remote {
            if let Some(value) = host_sync::dispatch_remote(self, remote, path, &bytes).await? {
                return Ok(value);
            }
        }
        match path {
            "/anticrumb.v1.PlatformService/ManageCache" => Ok(self
                .manage_cache(decode!(pb::CacheRequest))
                .await?
                .encode_to_vec()),
            "/anticrumb.v1.AccountService/ClearWebHistory" => {
                let status = self
                    .service
                    .clear_web_history(&decode!(pb::AccountRef).account_id)?;
                Ok(codec::web_history_to(status).encode_to_vec())
            }
            "/anticrumb.v1.AccountService/GetWebHistoryCleanup" => {
                let status = self
                    .service
                    .web_history_cleanup(&decode!(pb::AccountRef).account_id)?;
                Ok(codec::web_history_to(status).encode_to_vec())
            }
            "/anticrumb.v1.TurnService/CreateTurn" => {
                let request = codec::request_from(decode!(pb::TurnRequest))?;
                Ok(pb::TurnRef {
                    id: self.service.submit_async(request).await?,
                }
                .encode_to_vec())
            }
            "/anticrumb.v1.SessionService/CloseSession" => {
                self.service
                    .close_session(&decode!(pb::SessionRef).id)
                    .await?;
                Ok(pb::Empty {}.encode_to_vec())
            }
            "/anticrumb.v1.SessionService/CloseSessions" => {
                let ids = decode!(pb::CloseSessionsRequest).session_ids;
                let (closed, skipped, errors) = self.service.close_sessions(&ids).await;
                Ok(pb::CloseSessionsResult {
                    closed,
                    skipped,
                    errors: errors
                        .into_iter()
                        .map(|(id, err)| (id, codec::error_to(err)))
                        .collect(),
                }
                .encode_to_vec())
            }
            "/anticrumb.v1.SessionService/DeleteConversation" => {
                self.service
                    .delete_conversation(&decode!(pb::SessionRef).id)
                    .await?;
                Ok(pb::Empty {}.encode_to_vec())
            }
            "/anticrumb.v1.SessionService/RecoverMedia" => {
                let id = decode!(pb::SessionRef).id;
                let session: Session = self.service.store.get("local", "session", &id)?;
                let ids = self.service.recover_media(&id).await?;
                let artifacts = ids
                    .into_iter()
                    .map(|id| {
                        self.service
                            .store
                            .get::<Artifact>(&session.account_id, "artifact", &id)
                            .map(codec::artifact_to)
                    })
                    .collect::<Result<_>>()?;
                Ok(pb::ArtifactList { artifacts }.encode_to_vec())
            }
            "/anticrumb.v1.AccountService/GetWebUsage" => {
                let account = decode!(pb::AccountRef).account_id;
                // Usage reads on an idle Worker (or temporary overflow) tab so active chat turns
                // do not block `/usage`. Only block during sign-in, history cleanup, or lifecycle ops.
                let login_active = crate::account::login_status(&self.service.store, &account)
                    .is_some_and(|v| matches!(v.state.as_str(), "waiting" | "refreshing"));
                let history_active = self.service.web_history_active(&account)?;
                if login_active || history_active {
                    return Err(ServiceError::new(
                        ErrorCode::QueueFull,
                        "usage",
                        "account is signing in or cleaning history; wait for it to finish, then try /usage again",
                    ));
                }
                let usage_gate = self.usage_gate(&account);
                let permit = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    usage_gate.acquire_owned(),
                )
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorCode::QueueFull,
                        "usage",
                        "account is busy; wait for the current browser operation to finish, then try /usage again",
                    )
                })?;
                let host = self.clone();
                let path = path.to_owned();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    host.account_action(&path, &account, &bytes)
                })
                .await
                .map_err(ServiceError::storage)?
            }
            "/anticrumb.v1.AccountService/BrowserFetch" => {
                let req = decode!(pb::BrowserFetchRequest);
                let account = if req.account_id.is_empty() {
                    "default".to_owned()
                } else {
                    req.account_id
                };
                // Browser fetch uses an isolated ephemeral tab for external pages/search
                // and is decoupled from the Gemini chat tab pool and account gate.
                let host = self.clone();
                let path = path.to_owned();
                tokio::task::spawn_blocking(move || host.account_action(&path, &account, &bytes))
                    .await
                    .map_err(ServiceError::storage)?
            }
            "/anticrumb.v1.AccountService/BrowserControl" => {
                let req = decode!(pb::BrowserControlRequest);
                let account = if req.account_id.is_empty() {
                    "default".to_owned()
                } else {
                    req.account_id
                };
                // Stateful browser control runs on dedicated controlled tabs, decoupled from
                // Gemini's account_gate and chat tab pool.
                let host = self.clone();
                let path = path.to_owned();
                tokio::task::spawn_blocking(move || host.account_action(&path, &account, &bytes))
                    .await
                    .map_err(ServiceError::storage)?
            }
            "/anticrumb.v1.ModelService/RefreshCatalog"
            | "/anticrumb.v1.AccountService/OpenLogin"
            | "/anticrumb.v1.AccountService/SetBrowserMode"
            | "/anticrumb.v1.AccountService/AttachBrowser"
            | "/anticrumb.v1.AccountService/PauseAccount" => {
                let account = if path.ends_with("/AttachBrowser") {
                    decode!(pb::AttachBrowserRequest).account_id
                } else if path.ends_with("/SetBrowserMode") {
                    decode!(pb::SetBrowserModeRequest).account_id
                } else {
                    decode!(pb::AccountRef).account_id
                };
                let cancelled = self.logins.cancel(&self.service.store, &account)?;
                let gate = self.service.account_gate(&account);
                let permit = if cancelled || path.ends_with("/RefreshCatalog") {
                    tokio::time::timeout(std::time::Duration::from_secs(15), gate.acquire_owned())
                        .await
                        .ok()
                        .and_then(std::result::Result::ok)
                } else {
                    gate.try_acquire_owned().ok()
                }
                .ok_or_else(|| {
                    ServiceError::new(ErrorCode::QueueFull, "account", "account is busy")
                })?;
                let usage_gate = self.usage_gate(&account);
                let usage_permit = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    usage_gate.acquire_owned(),
                )
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorCode::QueueFull,
                        "account",
                        "account is busy checking usage",
                    )
                })?;
                let host = self.clone();
                let path = path.to_owned();
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let _usage_permit = usage_permit;
                    host.account_action(&path, &account, &bytes)
                })
                .await
                .map_err(ServiceError::storage)?
            }
            "/anticrumb.v1.PlatformService/GetStatus" => {
                let host = self.clone();
                let path = path.to_owned();
                let bytes = bytes.to_vec();
                let response =
                    tokio::task::spawn_blocking(move || host.dispatch_sync(&path, &bytes))
                        .await
                        .map_err(ServiceError::storage)??;
                let mut status =
                    pb::HostStatus::decode(response.as_slice()).map_err(ServiceError::storage)?;
                status.browser_connection = self.browser_connection_status(&mut status).await;
                Ok(status.encode_to_vec())
            }
            _ => {
                let host = self.clone();
                let path = path.to_owned();
                tokio::task::spawn_blocking(move || host.dispatch_sync(&path, &bytes))
                    .await
                    .map_err(ServiceError::storage)?
            }
        }
    }
    fn account_action(&self, path: &str, id: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        let store = &self.service.store;
        match path {
            "/anticrumb.v1.AccountService/GetWebUsage" => {
                let account: Account = store.get("local", "account", id)?;
                let provider = self.providers.resolve(&account.provider)?;
                let mut usage = crate::account::web_usage_with_store(
                    Some(store),
                    &account,
                    provider.as_ref(),
                    crate::agent::CancelToken::new(),
                )?;
                usage.server_id = self.server_id.clone();
                Ok(codec::web_usage_to(usage).encode_to_vec())
            }
            "/anticrumb.v1.AccountService/BrowserFetch" => {
                let req = pb::BrowserFetchRequest::decode(bytes)
                    .map_err(|e| ServiceError::invalid(format!("invalid protobuf: {e}")))?;
                let account: Account = store.get("local", "account", id)?;
                let out = crate::account::browser_fetch(
                    &account,
                    &req.url,
                    req.wait_ms,
                    req.max_chars as usize,
                    crate::agent::CancelToken::new(),
                )?;
                Ok(pb::BrowserFetchResponse {
                    url: out.url,
                    title: out.title,
                    text: out.text,
                }
                .encode_to_vec())
            }
            "/anticrumb.v1.AccountService/BrowserControl" => {
                let req = pb::BrowserControlRequest::decode(bytes)
                    .map_err(|e| ServiceError::invalid(format!("invalid protobuf: {e}")))?;
                let account: Account = store.get("local", "account", id)?;
                let mut out = self.browser_control.execute(
                    &account,
                    &req,
                    crate::agent::CancelToken::new(),
                )?;
                if let Some(shot) = out.page.as_mut().and_then(|page| page.screenshot.as_mut()) {
                    let artifact = store.artifact(
                        &account.id, &format!("{}.png", shot.id), "image/png", &shot.png, "browser",
                    ).map_err(|e| ServiceError::backend(format!("browser action completed, but screenshot storage failed: {e}; observe again before retrying the action"), false))?;
                    shot.artifact_id = artifact.id;
                    shot.png.clear();
                }
                Ok(out.encode_to_vec())
            }
            "/anticrumb.v1.ModelService/RefreshCatalog" => {
                let mut account: Account = store.get("local", "account", id)?;
                let provider = self.providers.resolve(&account.provider)?;
                let catalog = crate::account::probe(store, &mut account, provider.as_ref())?;
                Ok(codec::catalog_to(id, &self.server_id, catalog).encode_to_vec())
            }
            "/anticrumb.v1.AccountService/OpenLogin" => {
                let account: Account = store.get("local", "account", id)?;
                let provider = self.providers.resolve(&account.provider)?;
                crate::account::login(store, id, provider.as_ref())?;
                self.logins.start(
                    store.clone(),
                    store.get("local", "account", id)?,
                    provider,
                    self.service.account_gate(id),
                )?;
                Ok(pb::Empty {}.encode_to_vec())
            }
            "/anticrumb.v1.AccountService/SetBrowserMode" => {
                let r = pb::SetBrowserModeRequest::decode(bytes)
                    .map_err(|e| ServiceError::invalid(format!("invalid protobuf: {e}")))?;
                let account: Account = store.get("local", "account", id)?;
                let provider = self.providers.resolve(&account.provider)?;
                let mut status =
                    crate::account::set_browser_mode(store, id, r.headless, provider.as_ref())?;
                if !r.headless {
                    let account: Account = store.get("local", "account", id)?;
                    if !matches!(account.state.as_str(), "ready" | "disabled") {
                        self.logins.start(
                            store.clone(),
                            account,
                            provider,
                            self.service.account_gate(id),
                        )?;
                        status = crate::account::browser_status(store, id)?;
                    }
                }
                Ok(codec::browser_status_to(status).encode_to_vec())
            }
            "/anticrumb.v1.AccountService/PauseAccount" => {
                let mut account: Account = store.get("local", "account", id)?;
                account.state = "disabled".into();
                store.put("local", "account", id, &account)?;
                Ok(codec::account_to(account).encode_to_vec())
            }
            _ => {
                let r = pb::AttachBrowserRequest::decode(bytes).map_err(ServiceError::storage)?;
                let port = u16::try_from(r.port).map_err(ServiceError::storage)?;
                crate::account::attach(store, id, port)?;
                Ok(
                    codec::browser_status_to(crate::account::browser_status(store, id)?)
                        .encode_to_vec(),
                )
            }
        }
    }
    fn dispatch_sync(&self, path: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        let store = &self.service.store;
        macro_rules! decode {
            ($t:ty) => {
                <$t>::decode(bytes)
                    .map_err(|e| ServiceError::invalid(format!("invalid protobuf: {e}")))?
            };
        }
        macro_rules! reply {
            ($value:expr) => {
                Ok($value.encode_to_vec())
            };
        }
        match path {
            "/anticrumb.v1.PlatformService/GetInfo" => reply!(self.info()),
            "/anticrumb.v1.PlatformService/GetUsage" => {
                reply!(codec::usage_to(store.usage_record()?))
            }
            "/anticrumb.v1.PlatformService/GetStatus" => {
                let accounts = store
                    .list::<Account>("local", "account")?
                    .into_iter()
                    .map(|a| {
                        let catalog = store.model_catalog_record(&a.id)?;
                        let observed: RuntimeSelection =
                            store.get(&a.id, "runtime", "selection").unwrap_or_default();
                        let selected_model = observed.model.or_else(|| {
                            catalog
                                .models
                                .iter()
                                .find(|m| m.selected)
                                .map(|m| m.display_name().to_owned())
                        });
                        Ok(pb::AccountStatus {
                            id: a.id.clone(),
                            provider: a.provider,
                            state: a.state,
                            selected_model,
                            catalog: Some(codec::catalog_to(&a.id, &self.server_id, catalog)),
                            login: crate::account::login_status(store, &a.id)
                                .map(codec::login_status_to),
                        })
                    })
                    .collect::<Result<_>>()?;
                reply!(pb::HostStatus {
                    runtime: None,
                    active_agents: self
                        .agents
                        .as_ref()
                        .map(|agents| agents.active_count())
                        .transpose()?
                        .unwrap_or(0),
                    server_id: self.server_id.clone(),
                    accounts,
                    usage: Some(codec::usage_to(store.usage_record()?)),
                    providers: self
                        .providers
                        .list()
                        .into_iter()
                        .map(codec::provider_info_to)
                        .collect(),
                    browser_connection: None,
                    chat_cleanup: Some(codec::chat_cleanup_to(crate::service::cleanup::status(
                        store
                    )?)),
                })
            }
            "/anticrumb.v1.PlatformService/PruneCache" => {
                reply!(codec::prune_to(
                    store.prune_summary(decode!(pb::PruneRequest).days.into())?
                ))
            }
            "/anticrumb.v1.PlatformService/Shutdown" => {
                self.shutdown();
                reply!(pb::Empty {})
            }
            "/anticrumb.v1.ProviderService/ListProviders" => reply!(pb::ProviderList {
                providers: self
                    .providers
                    .list()
                    .into_iter()
                    .map(codec::provider_info_to)
                    .collect(),
            }),
            "/anticrumb.v1.ProviderService/GetProvider" => {
                let r = decode!(pb::ProviderRef);
                let provider = self.providers.resolve(&r.id)?;
                reply!(codec::provider_info_to(provider.info()))
            }
            "/anticrumb.v1.ProviderService/RegisterProvider" => {
                let manifest = codec::provider_manifest_from(decode!(pb::RegisterProviderRequest));
                let info = self.providers.register_manifest(manifest.clone())?;
                if let Err(err) = store.put(
                    "local",
                    crate::provider::registry::PROVIDER_STORE_KIND,
                    &manifest.id,
                    &manifest,
                ) {
                    let _ = self.providers.unregister(&manifest.id);
                    return Err(err);
                }
                reply!(codec::provider_info_to(info))
            }
            "/anticrumb.v1.ProviderService/UnregisterProvider" => {
                let id = decode!(pb::ProviderRef).id;
                self.providers.unregister(&id)?;
                store.delete("local", crate::provider::registry::PROVIDER_STORE_KIND, &id)?;
                reply!(pb::Empty {})
            }
            "/anticrumb.v1.AccountService/ListAccounts" => reply!(pb::AccountList {
                accounts: store
                    .list::<Account>("local", "account")?
                    .into_iter()
                    .map(codec::account_to)
                    .collect()
            }),
            "/anticrumb.v1.AccountService/CreateAccount" => {
                let r = decode!(pb::CreateAccountRequest);
                validate_id(&r.id)?;
                let provider = if r.provider.is_empty() {
                    default_provider()
                } else {
                    r.provider
                };
                self.providers.resolve(&provider)?;
                let requested_port = u16::try_from(r.port).map_err(ServiceError::storage)?;
                let existing = store.list::<Account>("local", "account")?;
                if existing.iter().any(|a| a.id == r.id) {
                    return Err(ServiceError::invalid("account ID already exists"));
                }
                let port = allocate_cdp_port(&existing, requested_port)?;
                let a = Account {
                    id: r.id.clone(),
                    provider,
                    profile: crate::cli::app_dir()
                        .join("accounts")
                        .join(&r.id)
                        .join("profile"),
                    subprofile: None,
                    port,
                    headless: true,
                    state: "auth_required".into(),
                };
                store.put("local", "account", &a.id, &a)?;
                reply!(codec::account_to(a))
            }
            "/anticrumb.v1.AccountService/GetBrowserStatus" => {
                reply!(codec::browser_status_to(crate::account::browser_status(
                    store,
                    &decode!(pb::AccountRef).account_id
                )?))
            }
            "/anticrumb.v1.ModelService/GetCatalog" => {
                let id = decode!(pb::AccountRef).account_id;
                reply!(codec::catalog_to(
                    &id,
                    &self.server_id,
                    store.model_catalog_record(&id)?
                ))
            }
            "/anticrumb.v1.TurnService/GetTurn" => reply!(codec::turn_to(
                self.service.snapshot(&decode!(pb::TurnRef).id)?
            )),
            "/anticrumb.v1.TurnService/ListTurns" => {
                let limit = decode!(pb::ListTurnsRequest).limit;
                let turns = store
                    .summary_records()?
                    .into_iter()
                    .take(if limit == 0 {
                        100
                    } else {
                        limit.min(100) as usize
                    })
                    .map(codec::turn_summary_to)
                    .collect();
                reply!(pb::TurnList { turns })
            }
            "/anticrumb.v1.TurnService/CancelTurn" => {
                self.service.cancel(&decode!(pb::TurnRef).id)?;
                reply!(pb::Empty {})
            }
            "/anticrumb.v1.TurnService/ReadEvents"
            | "/anticrumb.v1.WorkspaceService/ReadAgentEvents" => {
                let r = decode!(pb::EventCursor);
                if path.contains("TurnService") {
                    self.service.snapshot(&r.run_id)?;
                } else {
                    self.agents()?.snapshot(&r.run_id)?;
                }
                reply!(pb::EventPage {
                    events: store
                        .events(&r.run_id, r.after)?
                        .into_iter()
                        .map(codec::event_to)
                        .collect()
                })
            }
            "/anticrumb.v1.SessionService/GetSession" => {
                let id = decode!(pb::SessionRef).id;
                reply!(codec::session_to(
                    store.get::<Session>("local", "session", &id)?
                ))
            }
            "/anticrumb.v1.SessionService/ListSessions" => {
                let req = decode!(pb::ListSessionsRequest);
                let mode = codec::session_mode_filter(req.mode)?;
                let state = codec::session_state_filter(req.state)?;
                reply!(pb::SessionList {
                    sessions: store
                        .list::<Session>("local", "session")?
                        .into_iter()
                        .filter(|s| {
                            req.account_id
                                .as_deref()
                                .is_none_or(|account| s.account_id == account)
                                && mode.is_none_or(|expected| s.mode == expected)
                                && state.is_none_or(|expected| s.state == expected)
                        })
                        .map(codec::session_to)
                        .collect()
                })
            }
            "/anticrumb.v1.WorkspaceService/StartAgent" => {
                let started = self
                    .agents()?
                    .start(codec::agent_request_from(decode!(pb::AgentRequest))?)?;
                reply!(pb::StartAgentResponse {
                    id: started.id,
                    conversation_id: started.conversation_id,
                })
            }
            "/anticrumb.v1.WorkspaceService/GetAgent" => reply!(codec::agent_to(
                self.agents()?.snapshot(&decode!(pb::AgentRef).id)?
            )),
            "/anticrumb.v1.WorkspaceService/ListAgents" => reply!(pb::AgentList {
                agents: store
                    .list::<AgentRecord>("local", "agent")?
                    .into_iter()
                    .map(codec::agent_to)
                    .collect()
            }),
            "/anticrumb.v1.WorkspaceService/GetConversation" => reply!(codec::conversation_to(
                self.agents()?
                    .conversation(&decode!(pb::ConversationRef).id)?
            )),
            "/anticrumb.v1.WorkspaceService/ListConversations" => {
                let r = decode!(pb::ListConversationsRequest);
                let workspace = r
                    .workspace
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(Path::new);
                let account_id = r.account_id.as_deref().filter(|s| !s.is_empty());
                reply!(pb::ConversationList {
                    conversations: self
                        .agents()?
                        .list_conversations(workspace, account_id, r.limit as usize)?
                        .into_iter()
                        .map(codec::conversation_summary_to)
                        .collect()
                })
            }
            "/anticrumb.v1.WorkspaceService/CancelAgent" => {
                self.agents()?.cancel(&decode!(pb::AgentRef).id)?;
                reply!(pb::Empty {})
            }
            "/anticrumb.v1.WorkspaceService/DeleteAgentConversation" => {
                let r = decode!(pb::DeleteConversationRequest);
                let workspace = r
                    .workspace
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(Path::new);
                let account_id = r.account_id.as_deref().filter(|s| !s.is_empty());
                self.agents()?
                    .delete_conversation(&r.id, workspace, account_id)?;
                self.service.start_chat_cleanup();
                reply!(pb::Empty {})
            }
            "/anticrumb.v1.WorkspaceService/SteerAgent" => {
                let r = decode!(pb::SteerRequest);
                self.agents()?.steer(&r.id, &r.message, r.artifacts)?;
                reply!(pb::Empty {})
            }
            "/anticrumb.v1.WorkspaceService/ResumeConversation" => {
                let started = self.agents()?.resume(&decode!(pb::ConversationRef).id)?;
                reply!(pb::StartAgentResponse {
                    id: started.id,
                    conversation_id: started.conversation_id,
                })
            }
            "/anticrumb.v1.WorkspaceService/QueryKnowledge" => {
                let r = decode!(pb::KnowledgeQuery);
                reply!(pb::KnowledgeResults {
                    excerpts: crate::knowledge::query(
                        store,
                        &r.query,
                        r.source.as_deref(),
                        if r.limit == 0 {
                            5
                        } else {
                            r.limit.min(50) as usize
                        }
                    )?
                    .into_iter()
                    .map(codec::knowledge_excerpt_to)
                    .collect()
                })
            }
            _ => Err(ServiceError::new(
                ErrorCode::UnsupportedCapability,
                "rpc",
                format!("unknown RPC method: {path}"),
            )),
        }
    }
    pub async fn put_artifact_stream(
        self: &Arc<Self>,
        stream: impl Stream<Item = Result<pb::PutArtifactChunk>> + Send,
    ) -> Result<pb::Artifact> {
        tokio::pin!(stream);
        let first = stream
            .next()
            .await
            .ok_or_else(|| ServiceError::invalid("missing artifact metadata chunk"))??;
        let Some(pb::put_artifact_chunk::Payload::Meta(meta)) = first.payload else {
            return Err(ServiceError::invalid(
                "first upload chunk must contain artifact metadata",
            ));
        };
        if !is_valid_artifact_name(&meta.name) {
            return Err(ServiceError::invalid("invalid artifact name"));
        }
        if meta.size > MAX_ARTIFACT as u64 {
            return Err(ServiceError::invalid(format!(
                "artifact exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        if !is_valid_sha256_hex(&meta.sha256) {
            return Err(ServiceError::invalid("invalid artifact SHA-256"));
        }
        let store = self.service.store.clone();
        let meta_account = meta.account_id.clone();
        let meta_name = meta.name.clone();
        let meta_mime = meta.mime.clone();
        let meta_sha256 = meta.sha256.clone();
        let expected_size = meta.size;
        let writer = store
            .blocking(move |s| {
                let _: Account = s.get("local", "account", &meta_account)?;
                s.artifact_writer(
                    &meta_account,
                    &meta_name,
                    &meta_mime,
                    Some(expected_size),
                    Some(&meta_sha256),
                    "input",
                )
            })
            .await?;

        let saved = stream_chunks_to_sink(
            stream,
            writer,
            ChunkStreamSpec {
                expected_size,
                max_chunk_bytes: Some(MAX_MESSAGE),
                duplicate_meta_msg: "duplicate artifact metadata chunk",
                empty_chunk_msg: "empty artifact upload chunk",
                empty_data_msg: "empty artifact upload data chunk",
                chunk_limit_msg: format!("RPC chunk exceeds {MAX_MESSAGE_LABEL}").into(),
                overflow_msg: "artifact byte count exceeds declared size",
                size_mismatch_msg: "artifact size mismatch",
                aborted_msg: "artifact upload aborted",
            },
            |chunk| match chunk.payload {
                Some(pb::put_artifact_chunk::Payload::Meta(_)) => ChunkPayload::Meta,
                Some(pb::put_artifact_chunk::Payload::Data(data)) => ChunkPayload::Data(data),
                None => ChunkPayload::Empty,
            },
        )
        .await?;
        Ok(codec::artifact_to(saved))
    }

    pub async fn get_artifact_stream(
        self: &Arc<Self>,
        req: pb::ArtifactRef,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<pb::GetArtifactChunk>>> {
        let store = self.service.store.clone();
        let (art, mut file) = store
            .blocking(move |s| s.open_artifact(&req.account_id, &req.id))
            .await?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.try_send(Ok(pb::GetArtifactChunk {
            payload: Some(pb::get_artifact_chunk::Payload::Artifact(
                codec::artifact_to(art.clone()),
            )),
        }))
        .map_err(|_| ServiceError::storage("artifact response channel closed"))?;
        tokio::task::spawn_blocking(move || {
            let mut hasher = Sha256::new();
            let mut total = 0u64;
            let mut buf = vec![0u8; ARTIFACT_CHUNK_BYTES];
            loop {
                if tx.is_closed() {
                    return;
                }
                let n = match file.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = tx.blocking_send(Err(ServiceError::storage(e)));
                        return;
                    }
                };
                if n == 0 {
                    if total != art.size {
                        let _ = tx.blocking_send(Err(ServiceError::new(
                            ErrorCode::StorageFailure,
                            "blob",
                            "artifact integrity check failed",
                        )));
                        return;
                    }
                    let mut actual_hash = String::with_capacity(64);
                    for byte in hasher.finalize() {
                        let _ = write!(&mut actual_hash, "{byte:02x}");
                    }
                    if actual_hash != art.hash {
                        let _ = tx.blocking_send(Err(ServiceError::new(
                            ErrorCode::StorageFailure,
                            "blob",
                            "artifact integrity check failed",
                        )));
                    }
                    return;
                }
                hasher.update(&buf[..n]);
                let Some(next) = total.checked_add(n as u64) else {
                    let _ = tx.blocking_send(Err(ServiceError::new(
                        ErrorCode::StorageFailure,
                        "blob",
                        "artifact integrity check failed",
                    )));
                    return;
                };
                total = next;
                if total > art.size {
                    let _ = tx.blocking_send(Err(ServiceError::new(
                        ErrorCode::StorageFailure,
                        "blob",
                        "artifact integrity check failed",
                    )));
                    return;
                }
                if total == art.size {
                    // Verify no trailing bytes and full digest before releasing the final chunk.
                    let mut extra = [0u8; 1];
                    let trailing = loop {
                        match file.read(&mut extra) {
                            Ok(m) => break Ok(m),
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => break Err(e),
                        }
                    };
                    match trailing {
                        Ok(0) => {
                            let mut actual_hash = String::with_capacity(64);
                            for byte in hasher.finalize() {
                                let _ = write!(&mut actual_hash, "{byte:02x}");
                            }
                            if actual_hash != art.hash {
                                let _ = tx.blocking_send(Err(ServiceError::new(
                                    ErrorCode::StorageFailure,
                                    "blob",
                                    "artifact integrity check failed",
                                )));
                                return;
                            }
                            let _ = tx.blocking_send(Ok(pb::GetArtifactChunk {
                                payload: Some(pb::get_artifact_chunk::Payload::Data(
                                    buf[..n].to_vec(),
                                )),
                            }));
                            return;
                        }
                        Ok(_) => {
                            let _ = tx.blocking_send(Err(ServiceError::new(
                                ErrorCode::StorageFailure,
                                "blob",
                                "artifact integrity check failed",
                            )));
                            return;
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Err(ServiceError::storage(e)));
                            return;
                        }
                    }
                }
                if tx
                    .blocking_send(Ok(pb::GetArtifactChunk {
                        payload: Some(pb::get_artifact_chunk::Payload::Data(buf[..n].to_vec())),
                    }))
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(rx)
    }

    pub async fn watch(
        self: &Arc<Self>,
        cursor: pb::EventCursor,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<pb::Event>>> {
        self.read_run(&cursor.run_id).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let service = self.service.clone();
        tokio::spawn(async move {
            let mut after = cursor.after;
            let mut sub = service.subscribe_run(&cursor.run_id);
            loop {
                if tx.is_closed() {
                    break;
                }
                sub.mark_seen();
                if !forward_remaining_events(&service, &cursor.run_id, &mut after, &tx).await {
                    break;
                }
                match service.snapshot_async(&cursor.run_id).await {
                    Ok(run) if !run.is_active() => {
                        // A producer can finish events between the read and the terminal snapshot.
                        let _ = forward_remaining_events(&service, &cursor.run_id, &mut after, &tx)
                            .await;
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                    _ => {}
                }
                tokio::select! {
                    _ = tx.closed() => break,
                    _ = sub.wait() => {}
                }
            }
        });
        Ok(rx)
    }
    async fn read_run(&self, id: &str) -> Result<Run> {
        self.service.snapshot_async(id).await
    }
}

async fn forward_remaining_events(
    service: &crate::service::Service,
    run_id: &str,
    after: &mut i64,
    tx: &tokio::sync::mpsc::Sender<Result<pb::Event>>,
) -> bool {
    loop {
        match service.next_event_page(run_id, after).await {
            Ok((events, has_more)) => {
                for event in events {
                    if tx.send(Ok(codec::event_to(event))).await.is_err() {
                        return false;
                    }
                }
                if !has_more {
                    return true;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return false;
            }
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.logins.cancel_all(&self.service.store);
    }
}

fn allocate_cdp_port(existing: &[Account], requested: u16) -> Result<u16> {
    if requested > 0 {
        if existing.iter().any(|a| a.port == requested) {
            return Err(ServiceError::invalid("account port already exists"));
        }
        return Ok(requested);
    }
    let used: std::collections::HashSet<u16> = existing.iter().map(|a| a.port).collect();
    for port in crate::browser_host::DEFAULT_CDP_PORT..=u16::MAX {
        if used.contains(&port) || crate::browser_host::is_port_alive(port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(ServiceError::invalid("no free CDP port available"))
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || matches!(id, "local" | "ui")
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
    {
        Err(ServiceError::invalid("invalid account identifier"))
    } else {
        Ok(())
    }
}
