//! Presentation adapter for the local daemon's shared workspace service.
use super::{
    client::{Connection, RpcClient, WeakRpcClient},
    codec, pb,
};
use crate::{cli, domain::*};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

thread_local! {
    static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

pub fn selected_connection() -> Connection {
    CONNECTION.with(|connection| connection.borrow().clone().unwrap_or(Connection::Local))
}

pub fn with_connection(connection: Connection, action: impl FnOnce() -> Result<()>) -> Result<()> {
    let previous = CONNECTION.with(|slot| slot.replace(Some(connection)));
    struct Restore(Option<Connection>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONNECTION.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(previous);
    action()
}

struct Registered {
    rpc: WeakRpcClient,
    runtime: tokio::runtime::Handle,
}
fn registered() -> &'static Mutex<HashMap<PathBuf, Registered>> {
    static CLIENTS: OnceLock<Mutex<HashMap<PathBuf, Registered>>> = OnceLock::new();
    CLIENTS.get_or_init(Default::default)
}
fn sessions() -> &'static Mutex<HashMap<PathBuf, Weak<Session>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<PathBuf, Weak<Session>>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}
fn root_key(root: &Path) -> Result<PathBuf> {
    fs::canonicalize(root).map_err(ServiceError::storage)
}
pub(crate) fn register(
    root: &Path,
    rpc: &RpcClient,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    let mut clients = registered().lock().map_err(ServiceError::storage)?;
    clients.retain(|_, client| client.rpc.upgrade().is_some());
    clients.insert(
        root_key(root)?,
        Registered {
            rpc: rpc.downgrade(),
            runtime,
        },
    );
    Ok(())
}

struct Session {
    runtime: Option<tokio::runtime::Runtime>,
    rpc: RpcClient,
    connection: Connection,
    closed: Mutex<Option<Result<()>>>,
}
impl Session {
    fn close(&self) -> Result<()> {
        let mut closed = self.closed.lock().map_err(ServiceError::storage)?;
        if let Some(result) = &*closed {
            return result.clone();
        }
        let runtime = self.runtime.as_ref().expect("live session runtime");
        let close = || runtime.block_on(self.rpc.close());
        // Synchronous presentation calls can also be dropped from a foreign
        // Tokio context. Never nest Runtime::block_on on its worker thread.
        let result = if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| scope.spawn(close).join())
                .map_err(|_| ServiceError::invalid("embedded shutdown worker panicked"))?
        } else {
            close()
        };
        *closed = Some(result.clone());
        result
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let rpc = self.rpc.clone();
        let same_runtime = tokio::runtime::Handle::try_current()
            .ok()
            .is_some_and(|current| current.id() == runtime.handle().id());
        // Shutdown also joins blocking service workers. They borrow only a
        // Handle/RpcClient, so none can keep this Session alive or join itself.
        let worker = std::thread::spawn(move || {
            let _ = runtime.block_on(rpc.close());
            drop(rpc);
            drop(runtime);
        });
        if !same_runtime {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
pub struct Client {
    pub(crate) rpc: RpcClient,
    pub(crate) runtime: tokio::runtime::Handle,
    owner: Option<Arc<Session>>,
}
impl Client {
    pub fn cache(&self, options: crate::store::cache::Options) -> Result<pb::CacheReport> {
        options.validate()?;
        let execute = || {
            self.runtime
                .block_on(self.rpc.manage_cache(pb::CacheRequest {
                    days: options.days,
                    prune: options.prune,
                    dry_run: options.dry_run,
                }))
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| scope.spawn(execute).join())
                .map_err(|_| ServiceError::invalid("cache worker thread panicked"))?
        } else {
            execute()
        }
    }
    fn owned(session: Arc<Session>) -> Self {
        Self {
            rpc: session.rpc.clone(),
            runtime: session.runtime.as_ref().unwrap().handle().clone(),
            owner: Some(session),
        }
    }
    pub fn at(endpoint: &str, token: &str) -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new().map_err(ServiceError::storage)?;
        let rpc = runtime.block_on(RpcClient::network(endpoint, token, None, None))?;
        runtime.block_on(rpc.get_info(pb::Empty {}))?;
        Ok(Self::owned(Arc::new(Session {
            runtime: Some(runtime),
            rpc,
            connection: Connection::Local,
            closed: Mutex::new(None),
        })))
    }
    pub fn connect() -> Result<Self> {
        let root = cli::root();
        fs::create_dir_all(&root).map_err(ServiceError::storage)?;
        let key = root_key(&root)?;
        let connection = selected_connection();
        if let Some(session) = sessions()
            .lock()
            .map_err(ServiceError::storage)?
            .get(&key)
            .and_then(Weak::upgrade)
        {
            if session.connection != connection {
                return Err(ServiceError::invalid("this process already has a different workspace connection; close it before connecting elsewhere"));
            }
            if session
                .closed
                .lock()
                .map_err(ServiceError::storage)?
                .is_some()
            {
                return Err(ServiceError::invalid("this workspace runtime is closing"));
            }
            return Ok(Self::owned(session));
        }
        let runtime = tokio::runtime::Runtime::new().map_err(ServiceError::storage)?;
        let rpc = runtime.block_on(super::local::connect(&connection))?;
        let session = Arc::new(Session {
            runtime: Some(runtime),
            rpc,
            connection,
            closed: Mutex::new(None),
        });
        let mut cached = sessions().lock().map_err(ServiceError::storage)?;
        cached.retain(|_, session| session.strong_count() != 0);
        cached.insert(key, Arc::downgrade(&session));
        Ok(Self::owned(session))
    }
    /// Borrow the calling agent's actual workspace, never ambient CLI settings.
    /// The outer command owns shutdown; internal jobs must not retain its runtime.
    pub fn for_workspace(root: &Path) -> Result<Self> {
        let clients = registered().lock().map_err(ServiceError::storage)?;
        let client = clients
            .get(&root_key(root)?)
            .ok_or_else(|| ServiceError::invalid("workspace runtime is not registered"))?;
        let rpc = client
            .rpc
            .upgrade()
            .ok_or_else(|| ServiceError::invalid("workspace runtime has already stopped"))?;
        Ok(Self {
            rpc,
            runtime: client.runtime.clone(),
            owner: None,
        })
    }
    pub fn close(&self) -> Result<()> {
        if let Some(owner) = &self.owner {
            owner.close()
        } else {
            Err(ServiceError::invalid(
                "only the outer command can close an embedded workspace",
            ))
        }
    }
    pub fn artifact(&self, account: &str, id: &str) -> Result<super::client::ArtifactBytes> {
        self.runtime
            .block_on(self.rpc.get_artifact_bytes(pb::ArtifactRef {
                account_id: account.into(),
                id: id.into(),
            }))
    }
    pub fn download_artifact(&self, account: &str, id: &str, dest: &Path) -> Result<pb::Artifact> {
        self.runtime.block_on(self.rpc.get_artifact_to_file(
            pb::ArtifactRef {
                account_id: account.into(),
                id: id.into(),
            },
            dest,
        ))
    }
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.runtime
            .block_on(presentation(&self.rpc, method, path, body))
    }
    pub(crate) fn get_session(&self, id: &str) -> Result<pb::Session> {
        self.runtime
            .block_on(self.rpc.get_session(pb::SessionRef { id: id.into() }))
    }
    pub(crate) fn close_session(&self, id: &str) -> Result<()> {
        self.runtime
            .block_on(self.rpc.close_session(pb::SessionRef { id: id.into() }))?;
        Ok(())
    }
    pub(crate) fn close_sessions(&self, ids: &[String]) -> Result<pb::CloseSessionsResult> {
        if ids.is_empty() {
            return Ok(pb::CloseSessionsResult::default());
        }
        self.runtime
            .block_on(self.rpc.close_sessions(pb::CloseSessionsRequest {
                session_ids: ids.to_vec(),
            }))
    }
    pub(crate) fn create_turn(&self, request: &TurnRequest) -> Result<String> {
        Ok(self
            .runtime
            .block_on(self.rpc.create_turn(codec::request_to(request.clone())))?
            .id)
    }
    pub(crate) fn get_turn(&self, id: &str) -> Result<Run> {
        codec::turn_from(
            self.runtime
                .block_on(self.rpc.get_turn(pb::TurnRef { id: id.into() }))?,
        )
    }
    pub(crate) fn cancel_turn(&self, id: &str) -> Result<()> {
        self.runtime
            .block_on(self.rpc.cancel_turn(pb::TurnRef { id: id.into() }))?;
        Ok(())
    }
    pub(crate) fn read_turn_events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        self.runtime
            .block_on(self.rpc.read_events(pb::EventCursor {
                run_id: id.into(),
                after,
            }))?
            .events
            .into_iter()
            .map(codec::event_from)
            .collect()
    }
    pub fn put_file(&self, account: &str, path: &Path, mime: &str) -> Result<String> {
        // Reject oversized files before starting a streaming upload.
        if fs::metadata(path).map_err(ServiceError::storage)?.len() > crate::input::MAX_FILE_BYTES {
            return Err(ServiceError::invalid(format!(
                "attachment exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let value = self
            .runtime
            .block_on(self.rpc.put_artifact_file(account, path, &name, mime))?;
        Ok(value.id)
    }
    pub(crate) fn put_bytes(
        &self,
        account: &str,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        if bytes.len() as u64 > crate::input::MAX_FILE_BYTES {
            return Err(ServiceError::invalid(format!(
                "attachment exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        let value = self
            .runtime
            .block_on(self.rpc.put_artifact_bytes(account, name, mime, bytes))?;
        Ok(value.id)
    }
    pub(crate) fn provider_for_account(
        &self,
        account: &str,
    ) -> Result<crate::provider::ProviderInfo> {
        self.runtime.block_on(async {
            let account = self
                .rpc
                .list_accounts(pb::Empty {})
                .await?
                .accounts
                .into_iter()
                .find(|entry| entry.id == account)
                .ok_or_else(|| ServiceError::invalid("account not found"))?;
            let provider = self
                .rpc
                .get_provider(pb::ProviderRef {
                    id: account.provider,
                })
                .await?;
            Ok(codec::provider_info_from(provider))
        })
    }
    pub fn browser_fetch(
        &self,
        account_id: &str,
        url: &str,
        wait_ms: u32,
        max_chars: u32,
        cancel: &crate::agent::CancelToken,
    ) -> Result<pb::BrowserFetchResponse> {
        let req = pb::BrowserFetchRequest {
            account_id: account_id.into(),
            url: url.into(),
            wait_ms,
            max_chars,
        };
        let exec = || {
            self.runtime.block_on(async {
                tokio::select! {
                    res = self.rpc.browser_fetch(req) => res,
                    _ = cancel.cancelled() => Err(ServiceError::new(ErrorCode::Cancelled, "browser_fetch", "cancelled")),
                }
            })
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|s| s.spawn(exec).join())
                .map_err(|_| ServiceError::invalid("browser_fetch worker thread panicked"))?
        } else {
            exec()
        }
    }
    pub fn browser_control(
        &self,
        req: pb::BrowserControlRequest,
        cancel: &crate::agent::CancelToken,
    ) -> Result<pb::BrowserControlResponse> {
        let exec = || {
            self.runtime.block_on(async {
                tokio::select! {
                    res = self.rpc.browser_control(req) => res,
                    _ = cancel.cancelled() => Err(ServiceError::new(ErrorCode::Cancelled, "browser_control", "cancelled")),
                }
            })
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|s| s.spawn(exec).join())
                .map_err(|_| ServiceError::invalid("browser_control worker thread panicked"))?
        } else {
            exec()
        }
    }
}

/// Start an agent with the caller's environment. The environment crosses the
/// local RPC boundary only; the service never persists or returns it.
pub(crate) async fn start_agent(
    rpc: &RpcClient,
    mut request: AgentRequest,
) -> Result<pb::AgentRef> {
    request.environment = Some(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect(),
    );
    rpc.start_agent(codec::agent_request_to(request)).await
}

pub async fn presentation(
    rpc: &RpcClient,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    use base64::Engine;
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    let value = body.cloned().unwrap_or(json!({}));
    let after = form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "after")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let account = |id: &str| pb::AccountRef {
        account_id: id.into(),
    };
    match (method, parts.as_slice()) {
        ("GET", ["health"]) => Ok(json!(rpc.get_info(pb::Empty {}).await?)),
        ("GET", ["api", "status"]) => Ok(codec::status_from(rpc.get_status(pb::Empty {}).await?)),
        ("POST", ["api", "shutdown"]) => {
            rpc.shutdown(pb::Empty {}).await?;
            Ok(json!({"stopping":true}))
        }
        ("GET", ["api", "providers"]) => Ok(json!(rpc
            .list_providers(pb::Empty {})
            .await?
            .providers
            .into_iter()
            .map(codec::provider_info_from)
            .collect::<Vec<_>>())),
        ("GET", ["api", "providers", id]) => Ok(json!(codec::provider_info_from(
            rpc.get_provider(pb::ProviderRef { id: (*id).into() })
                .await?
        ))),
        ("POST", ["api", "providers"]) => {
            let manifest: crate::provider::ProviderManifest =
                serde_json::from_value(value).map_err(ServiceError::storage)?;
            let req = codec::register_provider_request_to(manifest);
            Ok(json!(codec::provider_info_from(
                rpc.register_provider(req).await?
            )))
        }
        ("DELETE", ["api", "providers", id]) => {
            rpc.unregister_provider(pb::ProviderRef { id: (*id).into() })
                .await?;
            Ok(json!({"unregistered": true}))
        }
        ("GET", ["api", "accounts"]) => Ok(json!(rpc.list_accounts(pb::Empty {}).await?.accounts)),
        ("POST", ["api", "accounts"]) => {
            let port = match value.get("port") {
                None | Some(Value::Null) => 0,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n <= u32::from(u16::MAX))
                    .ok_or_else(|| ServiceError::invalid("invalid port"))?,
            };
            Ok(json!(
                rpc.create_account(pb::CreateAccountRequest {
                    id: value["id"]
                        .as_str()
                        .ok_or_else(|| ServiceError::invalid("missing account id"))?
                        .into(),
                    port,
                    provider: value["provider"]
                        .as_str()
                        .unwrap_or(crate::provider::DEFAULT_PROVIDER_ID)
                        .into(),
                })
                .await?
            ))
        }
        ("GET", ["api", "accounts", id, "models"]) => {
            serde_json::to_value(codec::catalog_from(rpc.get_catalog(account(id)).await?))
                .map_err(ServiceError::storage)
        }
        ("GET", ["api", "accounts", id, "browser"]) => {
            Ok(json!(rpc.get_browser_status(account(id)).await?))
        }
        ("GET", ["api", "accounts", id, "usage"]) => {
            Ok(json!(rpc.get_web_usage(account(id)).await?))
        }
        ("POST", ["api", "accounts", id, "clear-history"]) => {
            Ok(json!(rpc.clear_web_history(account(id)).await?))
        }
        ("GET", ["api", "accounts", id, "clear-history"]) => {
            Ok(json!(rpc.get_web_history_cleanup(account(id)).await?))
        }
        ("POST", ["api", "accounts", id, "resolve"]) => {
            serde_json::to_value(codec::catalog_from(rpc.refresh_catalog(account(id)).await?))
                .map_err(ServiceError::storage)
        }
        ("POST", ["api", "accounts", id, "login"]) => {
            rpc.open_login(account(id)).await?;
            Ok(json!({"opened":true}))
        }
        ("POST", ["api", "accounts", id, "browser-mode"]) => Ok(json!(
            rpc.set_browser_mode(pb::SetBrowserModeRequest {
                account_id: (*id).into(),
                headless: value["headless"]
                    .as_bool()
                    .ok_or_else(|| ServiceError::invalid("invalid headless flag"))?
            })
            .await?
        )),
        ("POST", ["api", "accounts", id, "pause"]) => {
            Ok(json!(rpc.pause_account(account(id)).await?))
        }
        ("POST", ["api", "accounts", id, "attach"]) => Ok(json!(
            rpc.attach_browser(pb::AttachBrowserRequest {
                account_id: (*id).into(),
                port: value["port"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(|| ServiceError::invalid("invalid port"))?
            })
            .await?
        )),
        ("POST", ["api", "runs"]) => {
            let request: TurnRequest =
                serde_json::from_value(value).map_err(ServiceError::storage)?;
            Ok(json!(rpc.create_turn(codec::request_to(request)).await?))
        }
        ("GET", ["api", "runs"]) => Ok(json!(
            rpc.list_turns(pb::ListTurnsRequest { limit: 100 })
                .await?
                .turns
        )),
        ("GET", ["api", "runs", id]) => Ok(json!(codec::turn_from(
            rpc.get_turn(pb::TurnRef { id: (*id).into() }).await?
        )?)),
        ("POST", ["api", "runs", id, "cancel"]) => {
            rpc.cancel_turn(pb::TurnRef { id: (*id).into() }).await?;
            Ok(json!({"cancel_requested":true}))
        }
        ("GET", ["api", "events", id]) => {
            let cursor = pb::EventCursor {
                run_id: (*id).into(),
                after,
            };
            let page = if id.starts_with("agent_") {
                rpc.read_agent_events(cursor).await?
            } else {
                rpc.read_events(cursor).await?
            };
            Ok(json!(page
                .events
                .into_iter()
                .map(codec::event_from)
                .collect::<Result<Vec<_>>>()?))
        }
        ("GET", ["api", "sessions"]) => Ok(json!(rpc
            .list_sessions(pb::ListSessionsRequest::default())
            .await?
            .sessions
            .into_iter()
            .map(codec::session_json)
            .collect::<Vec<_>>())),
        ("GET", ["api", "sessions", id]) => Ok(codec::session_json(
            rpc.get_session(pb::SessionRef { id: (*id).into() }).await?,
        )),
        ("POST", ["api", "sessions", id, "close"]) => {
            rpc.close_session(pb::SessionRef { id: (*id).into() })
                .await?;
            Ok(json!({"closed":true}))
        }
        ("POST", ["api", "sessions", id, "media"]) => Ok(
            json!({"artifacts":rpc.recover_media(pb::SessionRef{id:(*id).into()}).await?.artifacts.into_iter().map(|a|a.id).collect::<Vec<_>>()}),
        ),
        ("POST", ["api", "artifacts"]) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(
                    value["data"]
                        .as_str()
                        .ok_or_else(|| ServiceError::invalid("missing artifact data"))?,
                )
                .map_err(ServiceError::storage)?;
            Ok(json!(
                rpc.put_artifact_bytes(
                    value["account_id"].as_str().unwrap_or("default"),
                    value["name"].as_str().unwrap_or("upload.bin"),
                    value["mime"].as_str().unwrap_or("application/octet-stream"),
                    bytes,
                )
                .await?
            ))
        }
        ("POST", ["api", "agents"]) => {
            let request: AgentRequest =
                serde_json::from_value(value).map_err(ServiceError::storage)?;
            Ok(json!(start_agent(rpc, request).await?))
        }
        ("GET", ["api", "agents"]) => Ok(json!(rpc
            .list_agents(pb::Empty {})
            .await?
            .agents
            .into_iter()
            .map(codec::agent_from)
            .collect::<Result<Vec<_>>>()?)),
        ("GET", ["api", "agents", id]) => serde_json::to_value(codec::agent_from(
            rpc.get_agent(pb::AgentRef { id: (*id).into() }).await?,
        )?)
        .map_err(ServiceError::storage),
        ("DELETE", ["api", "agents", id]) => {
            rpc.delete_agent(pb::AgentRef { id: (*id).into() }).await?;
            Ok(json!({"deleted":true}))
        }
        ("POST", ["api", "agents", id, "cancel"]) => {
            rpc.cancel_agent(pb::AgentRef { id: (*id).into() }).await?;
            Ok(json!({"cancel_requested":true}))
        }
        ("POST", ["api", "agents", id, "resume"]) => Ok(json!(
            rpc.resume_agent(pb::AgentRef { id: (*id).into() }).await?
        )),
        ("POST", ["api", "agents", id, "steer"]) => {
            let artifacts: Vec<String> = serde_json::from_value(
                value.get("artifacts").cloned().unwrap_or_else(|| json!([])),
            )
            .map_err(ServiceError::storage)?;
            rpc.steer_agent(pb::SteerRequest {
                id: (*id).into(),
                message: value["message"].as_str().unwrap_or("").into(),
                artifacts,
            })
            .await?;
            Ok(json!({"accepted":true}))
        }
        ("POST", ["api", "knowledge", "query"]) => Ok(json!(
            rpc.query_knowledge(pb::KnowledgeQuery {
                query: value["query"].as_str().unwrap_or("").into(),
                source: value["source"].as_str().map(str::to_owned),
                limit: value["limit"].as_u64().unwrap_or(5).min(50) as u32
            })
            .await?
        )),
        ("GET", ["v1", "models"]) => {
            let catalog = rpc.get_catalog(account("default")).await?;
            let provider = if catalog.provider.is_empty() {
                rpc.list_accounts(pb::Empty {})
                    .await?
                    .accounts
                    .into_iter()
                    .find(|a| a.id == "default")
                    .map(|a| a.provider)
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(default_provider)
            } else {
                catalog.provider.clone()
            };
            let default_model = format!("{provider}-default");
            let mut data =
                vec![json!({"id": default_model, "object": "model", "owned_by": provider})];
            data.extend(
                catalog
                    .models
                    .into_iter()
                    .map(|m| json!({"id": m.mode_id, "object": "model", "owned_by": provider})),
            );
            Ok(json!({"object":"list","data":data}))
        }
        _ => Err(ServiceError::invalid(format!(
            "unknown local presentation operation: {method} {path}"
        ))),
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::{
        rpc::host::Host,
        service::{Service, WebBackend},
        store::Store,
    };
    use std::time::Duration;

    #[test]
    fn explicit_connection_is_scoped_and_restored_even_after_an_error() {
        let remote = Connection::Grpc {
            endpoint: "https://fixture.invalid:50051".into(),
            token: "synthetic-connection-token".into(),
            ca_file: None,
            server_name: None,
            tls_pin: None,
        };
        assert_eq!(selected_connection(), Connection::Local);
        let result = with_connection(remote.clone(), || {
            assert_eq!(selected_connection(), remote);
            assert_eq!(
                std::thread::spawn(selected_connection).join().unwrap(),
                Connection::Local
            );
            with_connection(Connection::Local, || {
                assert_eq!(selected_connection(), Connection::Local);
                Ok(())
            })?;
            assert_eq!(selected_connection(), remote);
            Err(ServiceError::invalid("fixture command failed"))
        });
        assert!(result.is_err());
        assert_eq!(selected_connection(), Connection::Local);
    }

    #[test]
    fn last_owner_joins_workers_and_releases_lease_while_borrowed_clients_remain() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let host = Host::with_service(
            Service::new(store.clone(), Arc::new(WebBackend::new(store))).unwrap(),
            None,
            "workspace",
            Some(Host::lease(directory.path()).unwrap()),
        )
        .unwrap();
        let rpc = runtime
            .block_on(async { RpcClient::embedded(host, runtime.handle().clone()).unwrap() });
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            finished_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let owner = Client::owned(Arc::new(Session {
            runtime: Some(runtime),
            rpc,
            connection: Connection::Local,
            closed: Mutex::new(None),
        }));
        let borrowed = Client::for_workspace(directory.path()).unwrap();
        assert!(borrowed.owner.is_none());
        let (dropped_tx, dropped_rx) = std::sync::mpsc::sync_channel(1);
        let dropping = std::thread::spawn(move || {
            drop(owner);
            dropped_tx.send(()).unwrap();
        });
        assert!(matches!(
            dropped_rx.recv_timeout(Duration::from_millis(25)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        dropping.join().unwrap();
        drop(Host::lease(directory.path()).unwrap());
        drop(borrowed);
    }
}
