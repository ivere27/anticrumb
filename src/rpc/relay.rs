//! Workspace-side browser relay. Only protobufs and artifact bytes cross hosts.
use super::{client::RpcClient, codec, pb};
use crate::{
    agent::CancelToken,
    domain::*,
    service::{Backend, Emit},
    store::Store,
};
use futures_util::StreamExt;
use serde_json::json;
use std::{sync::Arc, time::Duration};
pub struct Relay {
    pub client: RpcClient,
    pub runtime: tokio::runtime::Handle,
    pub store: Store,
    pub peer: String,
}
impl Relay {
    fn artifact_to_remote(&self, account: &str, id: &str) -> Result<String> {
        let (artifact, file) = self.store.open_artifact(account, id)?;
        let value = self
            .runtime
            .block_on(self.client.put_artifact_verified_file(
                account,
                &artifact.name,
                &artifact.mime,
                &artifact.hash,
                artifact.size,
                file,
            ))?;
        Ok(value.id)
    }
    fn artifact_to_local(&self, account: &str, id: &str) -> Result<String> {
        self.runtime
            .block_on(download_artifact(&self.client, &self.store, account, id))
    }
    fn verify_remote_session(
        &self,
        account: &Account,
        session: &mut Session,
        remote_id: &str,
    ) -> Result<()> {
        let recorded: RemoteTurnRef = self.store.get(&session.id, "remote", "turn")?;
        if recorded.server_id != self.peer {
            return Err(ServiceError::invalid(
                "saved web conversation belongs to a different browser daemon; reconnect to its original host",
            ));
        }
        let remote = match self
            .runtime
            .block_on(self.client.get_session(pb::SessionRef {
                id: remote_id.to_owned(),
            })) {
            Ok(remote) => remote,
            Err(err)
                if err.code == ErrorCode::NotFound && session.mode == SessionMode::Temporary =>
            {
                session.state = SessionState::Invalidated;
                return Err(ServiceError::session_unavailable(
                    "resume",
                    "session no longer available",
                ));
            }
            Err(err) => return Err(err),
        };
        if remote.account_id != account.id {
            return Err(ServiceError::invalid(
                "remote session belongs to a different account",
            ));
        }
        let remote_mode = codec::session_mode_from(remote.mode)?;
        if remote_mode != session.mode {
            return Err(ServiceError::invalid("remote session mode mismatch"));
        }
        match codec::session_state_from(remote.state)? {
            SessionState::Deleted => {
                session.state = SessionState::Deleted;
                Err(ServiceError::invalid(
                    "this web conversation was deleted on the browser daemon; resume the saved conversation or start a new one to send its full context",
                ))
            }
            SessionState::OutcomeUnknown => {
                session.state = SessionState::OutcomeUnknown;
                Err(ServiceError::new(
                    ErrorCode::SubmissionOutcomeUnknown,
                    "resume",
                    "remote session outcome is unknown; reconcile before retrying",
                ))
            }
            state
                if session.mode == SessionMode::Temporary
                    && state.is_unavailable_for_temporary_resume() =>
            {
                session.state = if state == SessionState::Closed {
                    SessionState::Closed
                } else {
                    SessionState::Invalidated
                };
                Err(ServiceError::session_unavailable(
                    "resume",
                    "session no longer available",
                ))
            }
            _ => Ok(()),
        }
    }
}

pub(super) async fn download_artifact(
    client: &RpcClient,
    store: &Store,
    account: &str,
    id: &str,
) -> Result<String> {
    Ok(client
        .download_artifact_to_store(store, account, id, "remote")
        .await?
        .id)
}

impl Backend for Relay {
    fn delete_conversation(
        &self,
        _account: &Account,
        session: &Session,
        cancel: CancelToken,
    ) -> Result<()> {
        cancel
            .check()
            .map_err(|e| ServiceError::backend(e, false))?;
        let recorded: RemoteTurnRef = self.store.get(&session.id, "remote", "turn")?;
        if recorded.server_id != self.peer {
            return Err(ServiceError::invalid(
                "web cleanup belongs to a different browser daemon; reconnect to its original host",
            ));
        }
        let id = session.remote_session_id.as_ref().ok_or_else(|| {
            ServiceError::invalid("remote web conversation identity is unavailable")
        })?;
        self.runtime.block_on(async {
            tokio::select! {
                result = self.client.delete_conversation(pb::SessionRef { id: id.clone() }) => result,
                _ = cancel.cancelled() => Err(ServiceError::backend("cancelled".into(), false)),
            }
        })?;
        Ok(())
    }
    fn generate(
        &self,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        if let Some(remote_id) = session.remote_session_id.clone() {
            self.verify_remote_session(account, session, &remote_id)?;
        }
        let mut remote = request.clone();
        remote.session_id = session.remote_session_id.clone(); // The remote identity is separate from Chrome URLs and workspace paths.
        remote.retention = SessionRetention::Retain;
        remote.reuse_scope = None;
        // A backend invocation is one logical local turn. Persist its unique key
        // below for reconciliation; a later turn must not reuse a failed turn's key.
        remote.idempotency_key = Some(format!(
            "relay-{}-{}-{}",
            self.peer,
            session.id,
            uuid::Uuid::new_v4().simple()
        ));
        remote.artifacts = request
            .artifacts
            .iter()
            .map(|id| self.artifact_to_remote(&account.id, id))
            .collect::<Result<_>>()?;
        for message in &mut remote.messages {
            for block in &mut message.blocks {
                if let Block::Artifact { artifact_id } = block {
                    *artifact_id = self.artifact_to_remote(&account.id, artifact_id)?;
                }
            }
        }
        let key = remote.idempotency_key.clone().unwrap();
        let mut turn_ref = RemoteTurnRef {
            server_id: self.peer.clone(),
            idempotency_key: key,
            run_id: None,
        };
        // Save reconciliation information before crossing the submission boundary.
        self.store.put(&session.id, "remote", "turn", &turn_ref)?;
        let run = self
            .runtime
            .block_on(self.client.create_turn(codec::request_to(remote)))
            .map_err(uncertain)?;
        turn_ref.run_id = Some(run.id.clone());
        self.store.put(&session.id, "remote", "turn", &turn_ref)?;
        let result = self.runtime.block_on(async {
            let mut stream = self.client.watch_events(pb::EventCursor {
                run_id: run.id.clone(), after: 0
            }).await.map_err(uncertain)?;
            let idle_timeout = Duration::from_secs(request.timeout_secs + 15);
            let hard_cap = tokio::time::Instant::now()
                + Duration::from_secs(
                    crate::domain::max_turn_timeout_secs(request.timeout_secs) + 15,
                );
            let mut deadline = tokio::time::Instant::now() + idle_timeout;
            let mut cancel_deadline = None;
            loop {
                tokio::select! {
                    item = stream.next() => match item {
                        Some(event) => {
                            let event = codec::event_from(event.map_err(uncertain)?)?;
                            if event.kind == "session" {
                                session.remote_session_id = event.data["session_id"].as_str().map(str::to_owned);
                                self.store.put("local", "session", &session.id, session)?;
                            } else if ["text_delta", "model_info", "submitted", "uploading", "prompt_size", "cache_hit", "browser_stage"].contains(&event.kind.as_str()) {
                                let mut data = event.data;
                                if data.get("session_id").is_some() { data["session_id"] = json!(session.id); }
                                emit(&event.kind, data)?;
                                let extended = (tokio::time::Instant::now() + idle_timeout).min(hard_cap);
                                if extended > deadline {
                                    deadline = extended;
                                }
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        let now = tokio::time::Instant::now();
                        if (cancel.is_cancelled() || now >= deadline) && cancel_deadline.is_none() {
                            self.client.cancel_turn(run.clone()).await.map_err(uncertain)?;
                            cancel_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                        }
                        if cancel_deadline.is_some_and(|end| now >= end) {
                            return Err(ServiceError::new(ErrorCode::SubmissionOutcomeUnknown, "relay", "remote cancellation outcome is unknown"));
                        }
                    }
                }
            }
            let turn = codec::turn_from(self.client.get_turn(run).await.map_err(uncertain)?)?;
            if let Some(error) = turn.error { return Err(error); }
            turn.result.ok_or_else(|| ServiceError::new(ErrorCode::SubmissionOutcomeUnknown, "relay", "event stream ended before a terminal outcome was recorded"))
        })?;
        session.remote_session_id = Some(result.session_id.clone());
        session.remote_turns += 1;
        Ok(TurnResult {
            session_id: session.id.clone(),
            artifacts: result
                .artifacts
                .iter()
                .map(|id| self.artifact_to_local(&account.id, id))
                .collect::<Result<_>>()?,
            ..result
        })
    }
    fn close(&self, _account: &Account, session: &Session) -> Result<()> {
        if let Some(id) = &session.remote_session_id {
            self.runtime
                .block_on(self.client.close_session(pb::SessionRef { id: id.clone() }))?;
        }
        Ok(())
    }
    fn media(&self, account: &Account, session: &Session) -> Result<Vec<String>> {
        let id = session
            .remote_session_id
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("session has no remote conversation"))?;
        let value = self
            .runtime
            .block_on(self.client.recover_media(pb::SessionRef { id: id.clone() }))?;
        value
            .artifacts
            .into_iter()
            .map(|a| self.artifact_to_local(&account.id, &a.id))
            .collect()
    }
}

pub async fn workspace(
    root: &std::path::Path,
    connection: super::client::Connection,
) -> Result<Arc<super::host::Host>> {
    workspace_with_browser(root, connection, None).await
}

pub(crate) async fn workspace_with_browser(
    root: &std::path::Path,
    connection: super::client::Connection,
    browser: Option<Arc<super::host::Host>>,
) -> Result<Arc<super::host::Host>> {
    let store = Store::open(root)?;
    let lease = super::host::Host::lease(root)?;
    let client = match &connection {
        super::client::Connection::Local => RpcClient::embedded(
            match browser {
                Some(host) => host,
                None => super::host::Host::browser(&super::server_cmd::state_dir())?,
            },
            tokio::runtime::Handle::current(),
        )?,
        _ => RpcClient::connect(&connection).await?,
    };
    let info = client.get_info(pb::Empty {}).await.map_err(|mut error| {
        error.message = format!(
            "browser daemon handshake failed ({}): {}",
            connection.description(),
            error.message
        );
        if error.code == ErrorCode::AuthRequired {
            error.message.push_str(
                "; check the explicitly selected connection and its token against the running browser daemon configuration",
            );
        }
        error
    })?;
    if info.role != "browser" {
        return Err(ServiceError::new(
            ErrorCode::InvalidRequest,
            "connect",
            format!(
                "endpoint is not a browser server ({}); use the address of `anticrumb server`",
                connection.description()
            ),
        ));
    }
    let (transport, endpoint) = match &connection {
        super::client::Connection::Grpc { endpoint, .. } => {
            ("grpc", super::client::endpoint_description(endpoint))
        }
        super::client::Connection::Local => ("local", String::new()),
    };
    let browser_connection = pb::BrowserConnection {
        transport: transport.into(),
        endpoint,
        server_id: info.server_id.clone(),
        connected: true,
        error: String::new(),
    };
    let previous = store.get::<String>("local", "host", "browser_peer").ok();
    if previous.as_deref() != Some(&info.server_id) {
        for account in store.list::<Account>("local", "account")? {
            store.put(
                &account.id,
                "catalog",
                "models",
                &ModelCatalogRecord::default(),
            )?;
        }
        for mut session in store.list::<Session>("local", "session")? {
            session.state = SessionState::Closed;
            store.put("local", "session", &session.id, &session)?;
        }
        store.put("local", "host", "browser_peer", &info.server_id)?;
    }
    for account in client.list_accounts(pb::Empty {}).await?.accounts {
        let value = codec::account_from(account)?;
        store.put("local", "account", &value.id, &value)?;
    }
    let backend = Arc::new(Relay {
        client: client.clone(),
        runtime: tokio::runtime::Handle::current(),
        store: store.clone(),
        peer: info.server_id,
    });
    let service = crate::service::Service::new(store, backend)?;
    super::host::Host::with_connection(
        service,
        Some(client),
        "workspace",
        Some(lease),
        Some(browser_connection),
        crate::provider::ProviderRegistry::with_builtins(),
    )
}

fn uncertain(error: ServiceError) -> ServiceError {
    if matches!(
        error.code,
        ErrorCode::TransportDisconnected | ErrorCode::DeadlineExceeded
    ) {
        ServiceError::new(ErrorCode::SubmissionOutcomeUnknown, "relay", "remote outcome is unknown; reconcile the recorded run and idempotency key before retrying")
    } else {
        error
    }
}
