use crate::{
    agent::CancelToken,
    domain::*,
    provider::{ProviderContext, ProviderRegistry},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, OwnedSemaphorePermit, RwLock, Semaphore,
};
pub(crate) mod cleanup;
pub(crate) mod history;

pub type Emit<'a> = &'a mut dyn FnMut(&str, Value) -> Result<()>;
pub trait Backend: Send + Sync + 'static {
    fn generate(
        &self,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult>;
    fn close(&self, account: &Account, session: &Session) -> Result<()>;
    fn delete_conversation(
        &self,
        _account: &Account,
        _session: &Session,
        _cancel: CancelToken,
    ) -> Result<()> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "chat_cleanup",
            "backend cannot delete web conversations",
        ))
    }
    fn media(&self, _account: &Account, _session: &Session) -> Result<Vec<String>> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "download",
            "backend has no media recovery",
        ))
    }
    fn clear_web_history(
        &self,
        _account: &Account,
        _cancel: CancelToken,
        _progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        Err(ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "web_history",
            "backend cannot clear web history",
        ))
    }
    fn refresh_accounts(&self, _provider_id: &str, _cancel: CancelToken) -> Result<Vec<Account>> {
        Ok(Vec::new())
    }
}

pub(crate) use crate::tab_pool::{clear_system_tab_id, record_system_tab_id};
#[cfg(test)]
pub(crate) use crate::tab_pool::{find_reusable_gemini_target, find_system_gemini_target};

pub(crate) struct TempDirGuard {
    path: std::path::PathBuf,
    lease: Option<std::fs::File>,
}
impl TempDirGuard {
    pub(crate) fn new(path: std::path::PathBuf) -> Result<Self> {
        use fs2::FileExt;
        std::fs::create_dir_all(&path).map_err(ServiceError::storage)?;
        let lease = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path.join(".lease"))
            .map_err(ServiceError::storage)?;
        lease.lock_exclusive().map_err(ServiceError::storage)?;
        Ok(Self {
            path,
            lease: Some(lease),
        })
    }
}
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        drop(self.lease.take());
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub struct WebBackend {
    pub store: Store,
    pub providers: Arc<ProviderRegistry>,
    input_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl WebBackend {
    pub fn new(store: Store) -> Self {
        Self::with_providers(store, ProviderRegistry::with_builtins())
    }

    pub fn with_providers(store: Store, providers: Arc<ProviderRegistry>) -> Self {
        Self {
            store,
            providers,
            input_locks: Mutex::new(HashMap::new()),
        }
    }

    fn input_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        self.input_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(provider_id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

impl Backend for WebBackend {
    fn clear_web_history(
        &self,
        account: &Account,
        cancel: CancelToken,
        progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        let provider = self.providers.resolve(&account.provider)?;
        let browser = self.store.browser_for_account(account)?;
        let input_lock = self.input_lock(&account.provider);
        provider.clear_web_history(
            &ProviderContext::with_store_and_input_lock(&self.store, &input_lock),
            &browser,
            account,
            cancel,
            progress,
        )
    }

    fn delete_conversation(
        &self,
        account: &Account,
        session: &Session,
        cancel: CancelToken,
    ) -> Result<()> {
        let provider = self.providers.resolve(&account.provider)?;
        let browser = self.store.browser_for_account(account)?;
        let input_lock = self.input_lock(&account.provider);
        provider.delete_conversation(
            &ProviderContext::with_store_and_input_lock(&self.store, &input_lock),
            &browser,
            account,
            session,
            cancel,
        )
    }

    fn generate(
        &self,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        let provider = self.providers.resolve(&account.provider)?;
        let browser = self.store.browser_for_account(account)?;
        let input_lock = self.input_lock(&account.provider);
        provider.generate(
            &ProviderContext::with_store_and_input_lock(&self.store, &input_lock),
            &browser,
            account,
            session,
            request,
            cancel,
            emit,
        )
    }

    fn media(&self, account: &Account, session: &Session) -> Result<Vec<String>> {
        let provider = self.providers.resolve(&account.provider)?;
        let browser = self.store.browser_for_account(account)?;
        provider.recover_media(
            &ProviderContext::with_store(&self.store),
            &browser,
            account,
            session,
        )
    }

    fn close(&self, account: &Account, session: &Session) -> Result<()> {
        let provider = self.providers.resolve(&account.provider)?;
        let browser = self.store.browser_for_account(account)?;
        provider.close_session(
            &ProviderContext::with_store(&self.store),
            &browser,
            account,
            session,
        )
    }

    fn refresh_accounts(&self, provider_id: &str, cancel: CancelToken) -> Result<Vec<Account>> {
        let provider = self.providers.resolve(provider_id)?;
        crate::account::refresh_accounts(&self.store, provider_id, provider.as_ref(), cancel)
    }
}

#[derive(Default)]
struct RunEventBus {
    channels: Mutex<HashMap<String, tokio::sync::watch::Sender<u64>>>,
}

pub(crate) struct RunSubscription {
    run_id: String,
    rx: Option<tokio::sync::watch::Receiver<u64>>,
    bus: Arc<RunEventBus>,
}

impl RunSubscription {
    pub(crate) fn mark_seen(&mut self) {
        if let Some(rx) = &mut self.rx {
            let _ = rx.borrow_and_update();
        }
    }

    pub(crate) async fn wait(&mut self) {
        if let Some(rx) = &mut self.rx {
            // Bounded fallback wait ensures any out-of-band store writes are still noticed.
            let _ = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
        }
    }
}

impl Drop for RunSubscription {
    fn drop(&mut self) {
        drop(self.rx.take());
        self.bus.prune(&self.run_id);
    }
}

impl RunEventBus {
    fn subscribe(self: &Arc<Self>, run_id: &str) -> RunSubscription {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let tx = channels
            .entry(run_id.to_owned())
            .or_insert_with(|| tokio::sync::watch::channel(0u64).0);
        RunSubscription {
            run_id: run_id.to_owned(),
            rx: Some(tx.subscribe()),
            bus: Arc::clone(self),
        }
    }

    fn notify(&self, run_id: &str) {
        let tx = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned();
        if let Some(tx) = tx {
            tx.send_modify(|seq| *seq = seq.wrapping_add(1));
        }
    }

    fn close(&self, run_id: &str) {
        let tx = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        if let Some(tx) = tx {
            tx.send_modify(|seq| *seq = seq.wrapping_add(1));
        }
    }

    fn prune(&self, run_id: &str) {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = channels.get(run_id) {
            if tx.receiver_count() == 0 {
                channels.remove(run_id);
            }
        }
    }
}

/// Provider-level browser coordination gate (1 Chrome profile per provider).
///
/// - `lifecycle` write lock serializes browser-wide operations (`OpenLogin`, `SetBrowserMode`,
///   `AttachBrowser`, `RefreshAccounts`, `shutdown_managed_browsers`) across all accounts
///   sharing this provider's Chrome profile.
/// - `lifecycle` read lock allows turns, usage reads, catalog discovery, and cleanup to run
///   concurrently across different accounts in the same browser.
#[derive(Default)]
pub struct BrowserGate {
    lifecycle: Arc<RwLock<()>>,
}

impl BrowserGate {
    pub async fn read_owned(self: Arc<Self>) -> OwnedRwLockReadGuard<()> {
        self.lifecycle.clone().read_owned().await
    }

    pub fn try_read_owned(
        self: Arc<Self>,
    ) -> std::result::Result<OwnedRwLockReadGuard<()>, tokio::sync::TryLockError> {
        self.lifecycle.clone().try_read_owned()
    }

    pub async fn write_owned(self: Arc<Self>) -> OwnedRwLockWriteGuard<()> {
        self.lifecycle.clone().write_owned().await
    }

    pub fn try_write_owned(
        self: Arc<Self>,
    ) -> std::result::Result<OwnedRwLockWriteGuard<()>, tokio::sync::TryLockError> {
        self.lifecycle.clone().try_write_owned()
    }
}

/// Exclusive account operations (history deletion, media recovery, model refresh) wait
/// for all model turns on that account; independent conversations hold shared turn ownership.
#[derive(Default)]
pub struct AccountGate {
    lock: Arc<RwLock<()>>,
    preparation: Mutex<()>,
}

impl AccountGate {
    pub async fn acquire_owned(
        self: Arc<Self>,
    ) -> std::result::Result<OwnedRwLockWriteGuard<()>, std::convert::Infallible> {
        Ok(self.lock.clone().write_owned().await)
    }

    pub fn try_acquire_owned(
        self: Arc<Self>,
    ) -> std::result::Result<OwnedRwLockWriteGuard<()>, tokio::sync::TryLockError> {
        self.lock.clone().try_write_owned()
    }
}

struct TurnPermits {
    _browser: OwnedRwLockReadGuard<()>,
    _account: OwnedRwLockReadGuard<()>,
    _session: Option<OwnedSemaphorePermit>,
    _worker: Option<OwnedSemaphorePermit>,
}

pub struct Service {
    pub store: Store,
    pub shutdown: tokio::sync::Notify,
    stopping: std::sync::atomic::AtomicBool,
    backend: Arc<dyn Backend>,
    admission: Arc<Semaphore>,
    browsers: Mutex<HashMap<String, Arc<BrowserGate>>>,
    accounts: Mutex<HashMap<String, Arc<AccountGate>>>,
    sessions: Mutex<HashMap<String, std::sync::Weak<Semaphore>>>,
    workers: Mutex<HashMap<String, Arc<Semaphore>>>,
    controls: Mutex<HashMap<String, CancelToken>>,
    persistence_failures: Mutex<HashMap<String, Run>>,
    run_bus: Arc<RunEventBus>,
    cleanup_running: std::sync::atomic::AtomicBool,
    cleanup_notify: Arc<tokio::sync::Notify>,
    cleanup_cancel: CancelToken,
}

impl Drop for Service {
    fn drop(&mut self) {
        // Background loops hold only a Weak<Service> while idle-waiting on
        // `cleanup_notify`; cancelling wakes them so they exit instead of
        // parking forever once the service is gone.
        self.cleanup_cancel.cancel();
    }
}

impl Service {
    pub fn new(store: Store, backend: Arc<dyn Backend>) -> Result<Arc<Self>> {
        store.recover()?;
        history::recover(&store)?;
        Ok(Arc::new(Self {
            store,
            shutdown: tokio::sync::Notify::new(),
            stopping: false.into(),
            backend,
            admission: Arc::new(Semaphore::new(32)),
            browsers: Mutex::new(HashMap::new()),
            accounts: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
            controls: Mutex::new(HashMap::new()),
            persistence_failures: Mutex::new(HashMap::new()),
            run_bus: Arc::new(RunEventBus::default()),
            cleanup_running: false.into(),
            cleanup_notify: Arc::new(tokio::sync::Notify::new()),
            cleanup_cancel: CancelToken::new(),
        }))
    }
    pub(crate) fn subscribe_run(&self, run_id: &str) -> RunSubscription {
        self.run_bus.subscribe(run_id)
    }
    fn record_run_event(&self, run_id: &str, kind: &str, data: Value) -> Result<Event> {
        let event = self.store.event(run_id, kind, data)?;
        self.run_bus.notify(run_id);
        Ok(event)
    }
    pub(crate) async fn snapshot_async(&self, id: &str) -> Result<Run> {
        if let Some(run) = self
            .persistence_failures
            .lock()
            .map_err(ServiceError::storage)?
            .get(id)
            .cloned()
        {
            return Ok(run);
        }
        let key = id.to_owned();
        self.store
            .blocking(move |store| store.get("local", "run", &key))
            .await
    }
    pub(crate) async fn next_event_page(
        &self,
        run_id: &str,
        after: &mut i64,
    ) -> Result<(Vec<Event>, bool)> {
        let key = run_id.to_owned();
        let cursor = *after;
        let events = self
            .store
            .blocking(move |store| store.events(&key, cursor))
            .await?;
        if let Some(last) = events.last() {
            *after = last.sequence;
        }
        let has_more = events.len() == 256;
        Ok((events, has_more))
    }
    pub fn begin_shutdown(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.cleanup_cancel.cancel();
        for token in self
            .controls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            token.cancel();
        }
        self.shutdown.notify_waiters();
        self.shutdown.notify_one();
    }
    pub fn browser_gate(&self, provider_id: &str) -> Arc<BrowserGate> {
        self.browsers
            .lock()
            .unwrap()
            .entry(provider_id.into())
            .or_insert_with(|| Arc::new(BrowserGate::default()))
            .clone()
    }
    pub fn account_gate(&self, id: &str) -> Arc<AccountGate> {
        self.accounts
            .lock()
            .unwrap()
            .entry(id.into())
            .or_insert_with(|| Arc::new(AccountGate::default()))
            .clone()
    }
    pub async fn submit_async(self: &Arc<Self>, request: TurnRequest) -> Result<String> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.submit(request))
            .await
            .map_err(ServiceError::storage)?
    }
    pub fn submit(self: &Arc<Self>, mut request: TurnRequest) -> Result<String> {
        if !self
            .persistence_failures
            .lock()
            .map_err(ServiceError::storage)?
            .is_empty()
        {
            return Err(ServiceError::new(
                ErrorCode::StorageFailure,
                "admission",
                "storage failed; repair storage and restart the host before accepting more work",
            ));
        }
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ServiceError::invalid("host is stopping"));
        }
        if request.account_id.trim().is_empty() {
            request.account_id = self
                .store
                .resolve_active_account_id()
                .unwrap_or_else(|_| default_account());
        }
        crate::protocol::validate(&request)?;
        if let Some(id) = self.store.accepted(&request)? {
            return Ok(id);
        }
        let account: Account = self.store.get("local", "account", &request.account_id)?;
        if account.state != "ready" {
            return Err(ServiceError::new(
                ErrorCode::VerificationRequired,
                "account",
                format!("account {} is {}", account.id, account.state),
            ));
        }
        // An explicit session is the sole continuation authority. Never infer it from a global last request.
        if let Some(id) = &request.session_id {
            let session: Session = self.store.get("local", "session", id)?;
            if session.account_id != account.id
                || session.provider != account.provider
                || session.mode != request.mode
            {
                return Err(ServiceError::invalid(
                    "session ownership, provider, mode, or state mismatch",
                ));
            }
            if !session.is_continuable() {
                return Err(uncontinuable_session_error(&session, "validate"));
            }
        }
        let admission = self.admission.clone().try_acquire_owned().map_err(|_| {
            ServiceError::new(ErrorCode::QueueFull, "queue", "queue capacity reached")
        })?;
        let run = Run {
            id: id("run"),
            provider: account.provider.clone(),
            state: "queued".into(),
            request,
            result: None,
            error: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        };
        // Shutdown sets stopping before taking this lock. Recheck while holding
        // it so every accepted run is either rejected or included in cancellation.
        let mut controls = self.controls.lock().map_err(ServiceError::storage)?;
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ServiceError::invalid("host is stopping"));
        }
        if self.web_history_active(&run.request.account_id)? {
            return Err(ServiceError::invalid(
                "web history cleanup is active; wait for it to finish before starting a turn",
            ));
        }
        if let Some(existing) = self.store.accept(&run)? {
            return Ok(existing);
        }
        let cancel = CancelToken::new();
        controls.insert(run.id.clone(), cancel.clone());
        drop(controls);
        self.record_run_event(&run.id, "queued", json!({}))?;
        let service = self.clone();
        let run_id = run.id.clone();
        tokio::spawn(async move {
            service.execute(run, account, cancel, admission).await;
        });
        Ok(run_id)
    }
    fn session_gate(&self, id: &str) -> Arc<Semaphore> {
        let mut gates = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(id).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Semaphore::new(1));
        gates.insert(id.into(), Arc::downgrade(&gate));
        gate
    }

    async fn acquire_turn_permits(
        &self,
        provider_id: &str,
        account_id: &str,
        request: &TurnRequest,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<TurnPermits> {
        let acquiring = async {
            // Wait for this conversation before consuming a shared Worker slot.
            let session = if let Some(id) = request.session_id.as_deref() {
                Some(
                    self.session_gate(id)
                        .acquire_owned()
                        .await
                        .map_err(ServiceError::storage)?,
                )
            } else {
                None
            };
            let worker = if request.mode == SessionMode::Temporary {
                let gate = {
                    let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
                    workers
                        .entry(account_id.into())
                        .or_insert_with(|| {
                            Arc::new(Semaphore::new(
                                crate::tab_pool::TabPoolConfig::load(Some(&self.store)).worker_tabs,
                            ))
                        })
                        .clone()
                };
                Some(gate.acquire_owned().await.map_err(ServiceError::storage)?)
            } else {
                None
            };
            let browser = self.browser_gate(provider_id).read_owned().await;
            let account = self
                .account_gate(account_id)
                .lock
                .clone()
                .read_owned()
                .await;
            Ok(TurnPermits {
                _browser: browser,
                _account: account,
                _session: session,
                _worker: worker,
            })
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ServiceError::new(
                ErrorCode::Cancelled, "queue", "request cancelled while queued",
            )),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Err(ServiceError::new(
                ErrorCode::DeadlineExceeded, "queue", "queue deadline exceeded",
            )),
            permits = acquiring => permits,
        }
    }
    fn prepare_run_session(
        &self,
        account: &Account,
        worker_run: &mut Run,
        request: &mut TurnRequest,
        session_permit: &mut Option<OwnedSemaphorePermit>,
    ) -> Result<Session> {
        let gate = self.account_gate(&account.id);
        let _preparation = gate.preparation.lock().map_err(ServiceError::storage)?;
        let tools = digest(
            serde_json::to_vec(&json!({
                "tools": worker_run.request.tools,
                "choice": worker_run.request.tool_choice,
                "parallel": worker_run.request.parallel_tool_calls,
                "output_hint": worker_run.request.output_token_hint,
            }))
            .map_err(ServiceError::storage)?
            .as_slice(),
        );
        let mut reuse = request.reuse_scope.is_some()
            && request.session_id.is_none()
            && request.mode == SessionMode::Chat
            && request.artifacts.is_empty()
            && !request.generate_image;
        let mut known = self.store.sessions_for_account(&account.id)?;
        if reuse {
            let candidates: Vec<_> = known
                .iter()
                .filter(|s| {
                    s.account_id == account.id
                        && s.provider == account.provider
                        && s.state == SessionState::Ready
                        && s.reuse_scope == request.reuse_scope
                        && s.model == request.model
                        && s.thinking == request.thinking
                        && s.tool_digest == tools
                        && now_ms() - s.last_used < 30 * 60 * 1000
                        && exact_history_prefix(&s.history, &request.messages)
                })
                .collect();
            if candidates.len() == 1 {
                let session = candidates[0];
                // An explicit request can have reserved this ready session while
                // waiting for the account gate. Never race or evict its turn.
                if let Ok(permit) = self.session_gate(&session.id).try_acquire_owned() {
                    *session_permit = Some(permit);
                    let skipped = session.history.len();
                    request.messages.drain(..skipped);
                    request.session_id = Some(session.id.clone());
                    request.omit_tool_contract = true;
                    self.record_run_event(
                        &worker_run.id,
                        "cache_hit",
                        json!({"session_id": session.id, "skipped_messages": skipped}),
                    )?;
                }
            }
            request.retention = SessionRetention::Retain;
            if request.session_id.is_none() {
                // Keep at most two provider cache tabs per account; explicit user sessions are never evicted.
                let count = known
                    .iter()
                    .filter(|s| {
                        s.account_id == account.id && s.reuse_scope.is_some() && !s.is_inactive()
                    })
                    .count();
                if count >= 2 {
                    let mut idle = known
                        .iter_mut()
                        .filter(|s| {
                            s.account_id == account.id
                                && s.reuse_scope.is_some()
                                && !s.is_inactive()
                                && s.state != SessionState::Running
                        })
                        .collect::<Vec<_>>();
                    idle.sort_by_key(|s| s.last_used);
                    let evictable = idle.into_iter().find_map(|session| {
                        self.session_gate(&session.id)
                            .try_acquire_owned()
                            .ok()
                            .map(|permit| (session, permit))
                    });
                    if let Some((old, _permit)) = evictable {
                        self.backend.close(account, old)?;
                        old.target_id = None;
                        old.state = SessionState::Closed;
                        self.store.put("local", "session", &old.id, old)?;
                    } else {
                        // Both retained cache conversations are in use. Run this
                        // independent request without retaining a third cache entry.
                        reuse = false;
                        request.reuse_scope = None;
                        request.retention = SessionRetention::Ephemeral;
                    }
                }
            }
        }
        let mut session = if let Some(id) = &request.session_id {
            let s: Session = self.store.get("local", "session", id)?;
            if s.account_id != account.id
                || s.provider != account.provider
                || s.mode != request.mode
            {
                return Err(ServiceError::invalid(
                    "session ownership, provider, mode, or state mismatch",
                ));
            }
            if !s.is_continuable() {
                return Err(uncontinuable_session_error(&s, "prepare"));
            }
            s
        } else {
            Session {
                id: id("session"),
                provider: account.provider.clone(),
                account_id: account.id.clone(),
                mode: worker_run.request.mode,
                target_id: None,
                remote_url: None,
                remote_session_id: None,
                history: vec![],
                state: SessionState::Ready,
                model: None,
                thinking: None,
                tool_digest: tools.clone(),
                remote_turns: 0,
                last_used: now_ms(),
                reuse_scope: if reuse {
                    request.reuse_scope.clone()
                } else {
                    None
                },
            }
        };
        // A relay continuation may arrive with an explicit session. Verify the
        // retained contract before omitting it, just as for prefix reuse.
        if request.session_id.is_some()
            && session.tool_digest == tools
            && !session.history.is_empty()
        {
            request.omit_tool_contract = true;
        }
        // Changing a tool contract is explicit in the next prompt, never silently cached.
        session.tool_digest = tools;
        session.state = SessionState::Running;
        worker_run.state = "preparing".into();
        self.store.put("local", "run", &worker_run.id, worker_run)?;
        self.store.put("local", "session", &session.id, &session)?;
        self.record_run_event(&worker_run.id, "session", json!({"session_id": session.id}))?;
        Ok(session)
    }
    fn finalize_run_session(
        &self,
        account: &Account,
        worker_run: &Run,
        request: &TurnRequest,
        session: &mut Session,
        result: &Result<TurnResult>,
    ) -> Result<()> {
        let gate = self.account_gate(&account.id);
        let _preparation = gate.preparation.lock().map_err(ServiceError::storage)?;
        if let Ok(value) = result {
            session.state = SessionState::Ready;
            session.history.extend(request.messages.clone());
            let mut blocks = vec![Block::Text {
                text: value.text.clone(),
            }];
            blocks.extend(
                value
                    .tool_calls
                    .iter()
                    .cloned()
                    .map(|call| Block::ToolCall { call }),
            );
            session.history.push(Message {
                role: Role::Assistant,
                blocks,
            });
            session.model = request.model.clone();
            session.thinking = request.thinking.clone();
        } else if !session.state.is_terminated() {
            if result
                .as_ref()
                .err()
                .is_some_and(|e| e.code == ErrorCode::SessionUnavailable)
            {
                session.state = SessionState::Invalidated;
                session.target_id = None;
            } else {
                let unsubmitted_existing = session.remote_turns > 0
                    && worker_run.state != "submitted"
                    && result
                        .as_ref()
                        .err()
                        .is_none_or(|e| e.code != ErrorCode::SubmissionOutcomeUnknown);
                session.state = if unsubmitted_existing {
                    SessionState::Ready
                } else {
                    SessionState::OutcomeUnknown
                };
            }
        }
        let human_required = result.as_ref().err().is_some_and(|error| {
            matches!(
                error.code,
                ErrorCode::AuthRequired | ErrorCode::VerificationRequired
            )
        });
        if human_required {
            session.state = SessionState::AwaitingHuman;
            self.record_run_event(
                &worker_run.id,
                "human_required",
                json!({
                    "account_id": account.id,
                    "session_id": session.id,
                    "submission_may_have_occurred": worker_run.state == "submitted",
                    "action": "Complete verification on the daemon host, then explicitly refresh and resume."
                }),
            )?;
        }
        session.last_used = now_ms();
        // Keep the actual challenge tab available for the human. Closing it
        // here can destroy the only actionable verification screen.
        // Also release unsubmitted ephemeral sessions if prepare/upload failed
        // before sending anything, so they neither leak Chrome tabs nor fill
        // the retained session cap.
        let keep_alive_session = request.retention.should_retain_turn()
            && (result.is_ok() || session.remote_url.is_some() || worker_run.state == "submitted");
        if !human_required
            && !session.state.is_terminated()
            && !keep_alive_session
            && request.session_id.is_none()
        {
            match self.backend.close(account, session) {
                Ok(()) => {
                    session.target_id = None;
                    session.state = SessionState::Closed;
                }
                Err(e) => {
                    session.state = SessionState::CleanupPending;
                    let _ = self.record_run_event(
                        &worker_run.id,
                        "cleanup_failed",
                        json!({"error": e}),
                    );
                }
            }
        }
        self.store.put("local", "session", &session.id, session)?;
        if let Err(error) = result {
            if matches!(
                error.code,
                ErrorCode::AuthRequired
                    | ErrorCode::VerificationRequired
                    | ErrorCode::RateLimited
                    | ErrorCode::UiContractChanged
            ) {
                let mut account = account.clone();
                account.state = if error.code == ErrorCode::RateLimited {
                    "cooling_down"
                } else if error.code == ErrorCode::UiContractChanged {
                    "adapter_error"
                } else {
                    "awaiting_human"
                }
                .into();
                self.store.put("local", "account", &account.id, &account)?;
            }
        }
        Ok(())
    }
    async fn execute(
        self: Arc<Self>,
        mut run: Run,
        mut account: Account,
        cancel: CancelToken,
        _admission: OwnedSemaphorePermit,
    ) {
        let deadline = Instant::now() + Duration::from_secs(run.request.timeout_secs);
        enum AttemptOutcome {
            Done(Result<TurnResult>),
            IdentityDrift,
        }
        let mut final_result = None;
        for attempt in 0..2 {
            let permit = match self
                .acquire_turn_permits(
                    &account.provider,
                    &account.id,
                    &run.request,
                    &cancel,
                    deadline,
                )
                .await
            {
                Ok(permit) => permit,
                Err(error) => {
                    final_result = Some(Err(error));
                    break;
                }
            };
            let service = self.clone();
            let mut worker_run = run.clone();
            let attempt_account = account.clone();
            let attempt_cancel = cancel.clone();
            let outcome = tokio::task::spawn_blocking(move || -> Result<AttemptOutcome> {
                let mut permits = permit;
                let current: Account =
                    service.store.get("local", "account", &attempt_account.id)?;
                if current.state != "ready" {
                    return Err(ServiceError::new(
                        ErrorCode::VerificationRequired,
                        "account",
                        "account paused while queued",
                    ));
                }
                crate::protocol::validate(&worker_run.request)?;
                let mut request = worker_run.request.clone();
                let mut session = service.prepare_run_session(
                    &current,
                    &mut worker_run,
                    &mut request,
                    &mut permits._session,
                )?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                request.timeout_secs = remaining.as_secs().max(1);
                let mut emit = |kind: &str, data: Value| -> Result<()> {
                    if kind == "submitted" {
                        worker_run.state = "submitted".into();
                        service
                            .store
                            .put("local", "run", &worker_run.id, &worker_run)?;
                    }
                    service.record_run_event(&worker_run.id, kind, data)?;
                    Ok(())
                };
                let result = if attempt_cancel.is_cancelled() {
                    Err(ServiceError::new(
                        ErrorCode::Cancelled,
                        "queue",
                        "request cancelled during preparation",
                    ))
                } else if remaining.is_zero() {
                    Err(ServiceError::new(
                        ErrorCode::DeadlineExceeded,
                        "queue",
                        "deadline exceeded during preparation",
                    ))
                } else {
                    service.backend.generate(
                        &current,
                        &mut session,
                        &request,
                        attempt_cancel.clone(),
                        &mut emit,
                    )
                };
                if attempt == 0
                    && worker_run.state != "submitted"
                    && result
                        .as_ref()
                        .err()
                        .is_some_and(|e| e.stage == "identity_mismatch")
                {
                    session.target_id = None;
                    session.state = if session.remote_turns > 0 {
                        SessionState::Closed
                    } else {
                        SessionState::Invalidated
                    };
                    let _ = service.store.put("local", "session", &session.id, &session);
                    return Ok(AttemptOutcome::IdentityDrift);
                }
                service.finalize_run_session(
                    &current,
                    &worker_run,
                    &request,
                    &mut session,
                    &result,
                )?;
                Ok(AttemptOutcome::Done(result))
            })
            .await
            .unwrap_or_else(|e| {
                Err(ServiceError::new(
                    ErrorCode::Internal,
                    "worker",
                    e.to_string(),
                ))
            });
            match outcome {
                Ok(AttemptOutcome::Done(res)) => {
                    final_result = Some(res);
                    break;
                }
                Err(err) => {
                    final_result = Some(Err(err));
                    break;
                }
                Ok(AttemptOutcome::IdentityDrift) => {
                    // Browser read permit has been dropped; take exclusive browser write lock to rescan.
                    let _browser_write = self.browser_gate(&account.provider).write_owned().await;
                    let refresh_service = self.clone();
                    let provider_id = account.provider.clone();
                    let refresh_cancel = cancel.clone();
                    let refresh_res = tokio::task::spawn_blocking(move || {
                        refresh_service
                            .backend
                            .refresh_accounts(&provider_id, refresh_cancel)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        Err(ServiceError::new(
                            ErrorCode::Internal,
                            "refresh_accounts",
                            e.to_string(),
                        ))
                    });
                    drop(_browser_write);
                    if let Err(err) = refresh_res {
                        final_result = Some(Err(err));
                        break;
                    }
                    match self.store.get::<Account>("local", "account", &account.id) {
                        Ok(updated) if updated.state == "ready" => {
                            account = updated;
                            continue;
                        }
                        Ok(updated) => {
                            final_result = Some(Err(ServiceError::new(
                                ErrorCode::AuthRequired,
                                "identity_mismatch",
                                format!(
                                    "account '{}' is no longer signed in to Chrome; sign in via /browser login or switch with /account",
                                    updated.id
                                ),
                            )));
                            break;
                        }
                        Err(err) => {
                            final_result = Some(Err(err));
                            break;
                        }
                    }
                }
            }
        }
        let result = final_result.unwrap_or_else(|| {
            Err(ServiceError::new(
                ErrorCode::AuthRequired,
                "identity_mismatch",
                "account identity could not be verified after rescan",
            ))
        });
        self.finish(&mut run, result);
    }
    fn finish(&self, run: &mut Run, result: Result<TurnResult>) {
        match result {
            Ok(result) => {
                run.state = "completed".into();
                run.result = Some(result);
            }
            Err(error) => {
                run.state = match error.code {
                    ErrorCode::Cancelled => "cancelled",
                    ErrorCode::SubmissionOutcomeUnknown => "outcome_unknown",
                    ErrorCode::VerificationRequired | ErrorCode::AuthRequired => "awaiting_human",
                    _ => "failed",
                }
                .into();
                run.error = Some(error);
            }
        }
        run.updated_at = now_ms();
        if let Err(e) = self.store.put("local", "run", &run.id, run) {
            eprintln!("persist run {}: {e}", run.id);
            run.state = "failed".into();
            run.error = Some(e);
            run.result = None;
            self.persistence_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(run.id.clone(), run.clone());
        }
        let _ = self.store.event(
            &run.id,
            &run.state,
            json!({"result":run.result,"error":run.error}),
        );
        self.controls.lock().unwrap().remove(&run.id);
        self.run_bus.close(&run.id);
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        let _: Run = self.store.get("local", "run", id)?;
        if let Some(token) = self.controls.lock().unwrap().get(id) {
            token.cancel();
        }
        Ok(())
    }
    pub async fn wait(&self, id: &str) -> Result<Run> {
        let mut sub = self.subscribe_run(id);
        loop {
            sub.mark_seen();
            let run = self.snapshot_async(id).await?;
            if !run.is_active() {
                return Ok(run);
            }
            sub.wait().await;
        }
    }
    pub fn snapshot(&self, id: &str) -> Result<Run> {
        if let Some(run) = self
            .persistence_failures
            .lock()
            .map_err(ServiceError::storage)?
            .get(id)
            .cloned()
        {
            return Ok(run);
        }
        self.store.get("local", "run", id)
    }
    pub async fn recover_media(self: &Arc<Self>, id: &str) -> Result<Vec<String>> {
        let key = id.to_owned();
        let session: Session = self
            .store
            .blocking(move |store| store.get("local", "session", &key))
            .await?;
        let browser_permit = self.browser_gate(&session.provider).read_owned().await;
        let permit = self
            .account_gate(&session.account_id)
            .acquire_owned()
            .await
            .map_err(ServiceError::storage)?;
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let _browser_permit = browser_permit;
            let _permit = permit;
            let session: Session = service.store.get("local", "session", &session.id)?;
            let account: Account = service.store.get("local", "account", &session.account_id)?;
            if account.state != "ready" {
                return Err(ServiceError::invalid(
                    "resolve the account before downloading",
                ));
            }
            let artifacts = service.backend.media(&account, &session)?;
            service.store.event(
                &session.id,
                "artifacts_recovered",
                json!({"artifacts":artifacts}),
            )?;
            Ok(artifacts)
        })
        .await
        .map_err(ServiceError::storage)?
    }
    pub async fn close_session(self: &Arc<Self>, id: &str) -> Result<()> {
        let key = id.to_owned();
        let session: Session = self
            .store
            .blocking(move |store| store.get("local", "session", &key))
            .await?;
        let browser_permit = self.browser_gate(&session.provider).read_owned().await;
        let permit = self
            .account_gate(&session.account_id)
            .acquire_owned()
            .await
            .map_err(ServiceError::storage)?;
        let store = self.store.clone();
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || {
            let _browser_permit = browser_permit;
            let _permit = permit;
            let mut session: Session = store.get("local", "session", &session.id)?;
            let account: Account = store.get("local", "account", &session.account_id)?;
            backend.close(&account, &session)?;
            session.target_id = None;
            if session.state != SessionState::Deleted {
                session.state = SessionState::Closed;
            }
            store.put("local", "session", &session.id, &session)
        })
        .await
        .map_err(ServiceError::storage)?
    }
    pub async fn close_sessions(
        self: &Arc<Self>,
        ids: &[String],
    ) -> (Vec<String>, Vec<String>, Vec<(String, ServiceError)>) {
        let mut seen = std::collections::HashSet::new();
        let mut closed = Vec::new();
        let mut skipped = Vec::new();
        let mut errors = Vec::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            let key = id.clone();
            match self
                .store
                .blocking(move |store| store.get::<Session>("local", "session", &key))
                .await
            {
                Ok(session) if session.is_inactive() => {
                    skipped.push(id.clone());
                    continue;
                }
                Err(err) if err.code == ErrorCode::NotFound => {
                    skipped.push(id.clone());
                    continue;
                }
                Err(err) => {
                    errors.push((id.clone(), err));
                    continue;
                }
                Ok(_) => {}
            }
            match self.close_session(id).await {
                Ok(()) => closed.push(id.clone()),
                Err(err) if err.code == ErrorCode::NotFound => skipped.push(id.clone()),
                Err(err) => errors.push((id.clone(), err)),
            }
        }
        (closed, skipped, errors)
    }
}

fn uncontinuable_session_error(session: &Session, stage: &str) -> ServiceError {
    match session.state {
        SessionState::Closed | SessionState::Invalidated | SessionState::CleanupPending => {
            ServiceError::session_unavailable(stage, "session no longer available")
        }
        SessionState::OutcomeUnknown => ServiceError::new(
            ErrorCode::SubmissionOutcomeUnknown,
            stage,
            "session outcome is unknown; reconcile before retrying",
        ),
        _ => ServiceError::invalid("session ownership, provider, mode, or state mismatch"),
    }
}

/// Exact complete messages only. Empty text blocks are normalized because clients omit them on tool calls.
fn exact_history_prefix(history: &[Message], input: &[Message]) -> bool {
    if history.is_empty() || history.len() >= input.len() {
        return false;
    }
    fn normalized(messages: &[Message]) -> Vec<Value> {
        messages.iter().map(|m|json!({"role":m.role,"blocks":m.blocks.iter().filter(|b|!matches!(b,Block::Text{text} if text.is_empty())).collect::<Vec<_>>()})).collect()
    }
    normalized(history) == normalized(&input[..history.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct UnexpectedBackend(AtomicUsize);
    impl Backend for UnexpectedBackend {
        fn generate(
            &self,
            _: &Account,
            _: &mut Session,
            _: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ServiceError::invalid("unexpected backend call"))
        }
        fn close(&self, _: &Account, _: &Session) -> Result<()> {
            Ok(())
        }
    }

    fn wait_until(predicate: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        predicate()
    }

    #[test]
    fn cache_reuse_and_eviction_respect_explicit_session_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let account = Account {
            id: default_account(),
            provider: default_provider(),
            state: "ready".into(),
            identity: None,
            label: None,
            slot: Some("0".into()),
        };
        let service = Service::new(
            store.clone(),
            Arc::new(UnexpectedBackend(AtomicUsize::new(0))),
        )
        .unwrap();
        let prepare = |mut request: TurnRequest| {
            let mut run = Run {
                id: id("test-run"),
                provider: default_provider(),
                state: "queued".into(),
                request: request.clone(),
                result: None,
                error: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            };
            let mut permit = None;
            let session = service
                .prepare_run_session(&account, &mut run, &mut request, &mut permit)
                .unwrap();
            (session, request, permit)
        };
        let mut seed = TurnRequest::text("seed");
        seed.reuse_scope = Some("cache-scope".into());
        let (mut cached, _, _) = prepare(seed.clone());
        cached.state = SessionState::Ready;
        cached.history = seed.messages.clone();
        cached.history.push(Message {
            role: Role::Assistant,
            blocks: vec![Block::Text {
                text: "answer".into(),
            }],
        });
        store.put("local", "session", &cached.id, &cached).unwrap();
        let explicit = service
            .session_gate(&cached.id)
            .try_acquire_owned()
            .unwrap();
        let mut followup = seed.clone();
        followup.messages = cached.history.clone();
        followup
            .messages
            .extend(TurnRequest::text("followup").messages);
        let (mut fresh, sent, _) = prepare(followup.clone());
        assert_ne!(
            fresh.id, cached.id,
            "do not steal the explicitly reserved session"
        );
        assert_eq!(
            sent.messages.len(),
            3,
            "fallback must retain the complete history"
        );
        assert!(sent.session_id.is_none());
        fresh.state = SessionState::Ready;
        store.put("local", "session", &fresh.id, &fresh).unwrap();
        let second_explicit = service.session_gate(&fresh.id).try_acquire_owned().unwrap();
        let (uncached, sent, _) = prepare(TurnRequest {
            reuse_scope: Some("different-scope".into()),
            ..TurnRequest::text("independent")
        });
        assert!(uncached.reuse_scope.is_none());
        assert!(!sent.retention.should_retain_turn());
        for sid in [&cached.id, &fresh.id] {
            assert_eq!(
                store.get::<Session>("local", "session", sid).unwrap().state,
                SessionState::Ready
            );
        }
        drop(explicit);
        let (reused, _, implicit) = prepare(followup);
        assert_eq!(reused.id, cached.id);
        assert!(implicit.is_some());
        assert!(
            service
                .session_gate(&cached.id)
                .try_acquire_owned()
                .is_err(),
            "cache turn must hold session ownership until finalization"
        );
        drop(implicit);
        drop(second_explicit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preparation_deadline_finalizes_the_session_without_generating() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let account = Account {
            id: default_account(),
            provider: default_provider(),
            state: "ready".into(),
            identity: None,
            label: None,
            slot: Some("0".into()),
        };
        store
            .put("local", "account", &account.id, &account)
            .unwrap();
        let backend = Arc::new(UnexpectedBackend(AtomicUsize::new(0)));
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        let gate = service.account_gate(&account.id);
        let blocked_gate = gate.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _preparing = blocked_gate.preparation.lock().unwrap();
            started.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut request = TurnRequest::text("expired during preparation");
        request.timeout_secs = 1;
        let id = service.submit(request).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while gate.lock.clone().try_write_owned().is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        release.send(()).unwrap();
        blocker.join().unwrap();
        let run = service.wait(&id).await.unwrap();
        assert_eq!(run.error.unwrap().code, ErrorCode::DeadlineExceeded);
        assert_eq!(backend.0.load(Ordering::SeqCst), 0);
        let sessions = store.sessions_for_account(&account.id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Closed);
    }

    #[tokio::test]
    async fn shutdown_rejects_submission_waiting_to_register_without_persisting_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .put(
                "local",
                "account",
                DEFAULT_ACCOUNT_ID,
                &Account {
                    id: default_account(),
                    provider: default_provider(),
                    state: "ready".into(),
                    identity: None,
                    label: None,
                    slot: Some("0".into()),
                },
            )
            .unwrap();
        let backend = Arc::new(UnexpectedBackend(AtomicUsize::new(0)));
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        let mut request = TurnRequest::text("shutdown admission fixture");
        request.idempotency_key = Some("shutdown-admission".into());
        let (submission, reached_registration, shutdown_started) = {
            let controls = service.controls.lock().unwrap();
            let submitter = service.clone();
            let submitted = request.clone();
            let submission = tokio::task::spawn_blocking(move || submitter.submit(submitted));
            // A reserved permit proves submit passed its initial stopping check.
            // Holding controls prevents it from registering its cancellation token.
            let reached_registration = wait_until(|| service.admission.available_permits() == 31);
            let stopping = service.clone();
            let shutdown = std::thread::spawn(move || stopping.begin_shutdown());
            let shutdown_started = wait_until(|| service.stopping.load(Ordering::SeqCst));
            drop(controls);
            shutdown.join().unwrap();
            (submission, reached_registration, shutdown_started)
        };
        let result = submission.await.unwrap();
        assert!(reached_registration);
        assert!(shutdown_started);
        assert_eq!(result.unwrap_err().message, "host is stopping");
        assert!(store.list::<Run>("local", "run").unwrap().is_empty());
        assert!(store.accepted(&request).unwrap().is_none());
        assert!(service.controls.lock().unwrap().is_empty());
        assert_eq!(service.admission.available_permits(), 32);
        assert_eq!(backend.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn system_tab_never_steals_chat_tabs_and_reusable_chat_never_steals_system_tab() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .put(
                "local",
                "session",
                "closed-chat",
                &Session {
                    id: "closed-chat".into(),
                    provider: default_provider(),
                    account_id: "default".into(),
                    mode: SessionMode::Chat,
                    target_id: Some("tab-chat".into()),
                    remote_url: Some("https://gemini.google.com/app/chat_abc".into()),
                    remote_session_id: None,
                    history: vec![],
                    state: SessionState::Closed,
                    model: None,
                    thinking: None,
                    tool_digest: String::new(),
                    remote_turns: 1,
                    last_used: 1,
                    reuse_scope: None,
                },
            )
            .unwrap();

        let targets_json = std::sync::Arc::new(std::sync::Mutex::new(
            json!([
                {"id":"tab-chat","type":"page","title":"Chat","url":"https://gemini.google.com/app/chat_abc","webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/tab-chat"}
            ])
            .to_string(),
        ));
        let body_ref = targets_json.clone();
        let server = crate::browser_host::MockHttpServer::spawn(move |stream, _req, _port| {
            let body = body_ref.lock().unwrap().clone();
            crate::browser_host::MockHttpServer::write_json(stream, &body);
        });
        let port = server.port;

        // 1. Only a conversation tab (/app/chat_abc) exists (even after session closed):
        // find_system_gemini_target MUST return None instead of hijacking it.
        assert!(find_system_gemini_target(&store, "default", port).is_none());
        assert_eq!(
            find_reusable_gemini_target(&store, "default", port)
                .map(|t| t.id)
                .as_deref(),
            Some("tab-chat")
        );

        // 2. Register a dedicated internal system tab at /usage:
        // find_system_gemini_target picks tab-sys while find_reusable_gemini_target excludes it.
        *targets_json.lock().unwrap() = json!([
            {"id":"tab-chat","type":"page","title":"Chat","url":"https://gemini.google.com/app/chat_abc","webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/tab-chat"},
            {"id":"tab-sys","type":"page","title":"Usage","url":"https://gemini.google.com/usage","webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/tab-sys"}
        ])
        .to_string();
        record_system_tab_id(&store, "gemini", port, "tab-sys");
        assert_eq!(
            find_system_gemini_target(&store, "gemini", port)
                .map(|t| t.id)
                .as_deref(),
            Some("tab-sys")
        );
        assert_eq!(
            find_reusable_gemini_target(&store, "gemini", port)
                .map(|t| t.id)
                .as_deref(),
            Some("tab-chat")
        );

        // If no chat tab is left (only system tab on /app), chat reuse STILL skips tab-sys.
        *targets_json.lock().unwrap() = json!([
            {"id":"tab-sys","type":"page","title":"Gemini","url":"https://gemini.google.com/app","webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/tab-sys"}
        ])
        .to_string();
        assert!(find_reusable_gemini_target(&store, "gemini", port).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chat_session_with_remote_url_survives_browser_mode_invalidation_and_reopens() {
        struct ResumableChatBackend;
        impl Backend for ResumableChatBackend {
            fn generate(
                &self,
                _: &Account,
                session: &mut Session,
                _: &TurnRequest,
                _: CancelToken,
                _: Emit<'_>,
            ) -> Result<TurnResult> {
                session.target_id = Some(format!("tab-turn-{}", session.remote_turns + 1));
                session.remote_url = Some("https://claude.ai/chat/persisted-123".into());
                session.remote_turns += 1;
                Ok(TurnResult {
                    session_id: session.id.clone(),
                    text: format!("turn {}", session.remote_turns),
                    tool_calls: vec![],
                    artifacts: vec![],
                    model: None,
                    elapsed_ms: 1,
                })
            }
            fn close(&self, _: &Account, _: &Session) -> Result<()> {
                Ok(())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .put(
                "local",
                "account",
                "claude",
                &Account {
                    id: "claude".into(),
                    provider: "claude-web".into(),
                    state: "ready".into(),
                    identity: None,
                    label: None,
                    slot: None,
                },
            )
            .unwrap();
        let service = Service::new(store.clone(), Arc::new(ResumableChatBackend)).unwrap();

        let mut turn1 = TurnRequest::text("hi");
        turn1.account_id = "claude".into();
        turn1.retention = SessionRetention::Retain;
        let run1_id = service.submit(turn1).unwrap();
        assert!(wait_until(|| {
            matches!(
                store
                    .get::<Run>("local", "run", &run1_id)
                    .unwrap()
                    .state
                    .as_str(),
                "completed" | "failed"
            )
        }));
        let run1: Run = store.get("local", "run", &run1_id).unwrap();
        assert_eq!(run1.state, "completed", "{:?}", run1.error);
        let session_id = run1.result.unwrap().session_id;

        // Simulate `/browser hide` restarting Chrome and clearing live tabs.
        crate::account::invalidate_account_sessions(&store, "claude").unwrap();
        let after_hide: Session = store.get("local", "session", &session_id).unwrap();
        assert_eq!(after_hide.state, SessionState::Closed);
        assert!(after_hide.target_id.is_none());
        assert_eq!(
            after_hide.remote_url.as_deref(),
            Some("https://claude.ai/chat/persisted-123")
        );

        let mut turn2 = TurnRequest::text("follow up");
        turn2.account_id = "claude".into();
        turn2.session_id = Some(session_id.clone());
        turn2.retention = SessionRetention::Retain;
        let run2_id = service.submit(turn2).unwrap();
        assert!(wait_until(|| {
            matches!(
                store
                    .get::<Run>("local", "run", &run2_id)
                    .unwrap()
                    .state
                    .as_str(),
                "completed" | "failed"
            )
        }));
        let run2: Run = store.get("local", "run", &run2_id).unwrap();
        assert_eq!(run2.state, "completed", "{:?}", run2.error);
        let res2 = run2.result.unwrap();
        assert_eq!(res2.session_id, session_id);
        assert_eq!(res2.text, "turn 2");

        let reopened: Session = store.get("local", "session", &session_id).unwrap();
        assert_eq!(reopened.state, SessionState::Ready);
        assert_eq!(reopened.remote_turns, 2);
        assert_eq!(reopened.target_id.as_deref(), Some("tab-turn-2"));
    }

    #[test]
    fn submit_rejects_explicit_session_with_mismatched_temporary_mode() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .put(
                "local",
                "account",
                DEFAULT_ACCOUNT_ID,
                &Account {
                    id: default_account(),
                    provider: default_provider(),
                    state: "ready".into(),
                    identity: None,
                    label: None,
                    slot: Some("0".into()),
                },
            )
            .unwrap();
        store
            .put(
                "local",
                "session",
                "chat-sess",
                &Session {
                    id: "chat-sess".into(),
                    provider: default_provider(),
                    account_id: default_account(),
                    mode: SessionMode::Chat,
                    target_id: Some("tab-1".into()),
                    remote_url: Some("https://gemini.google.com/app/c1".into()),
                    remote_session_id: None,
                    history: vec![],
                    state: SessionState::Ready,
                    model: None,
                    thinking: None,
                    tool_digest: String::new(),
                    remote_turns: 1,
                    last_used: 1,
                    reuse_scope: None,
                },
            )
            .unwrap();
        let service =
            Service::new(store, Arc::new(UnexpectedBackend(AtomicUsize::new(0)))).unwrap();
        let mut req = TurnRequest::text("follow up with wrong mode");
        req.session_id = Some("chat-sess".into());
        req.mode = SessionMode::Temporary;
        let err = service.submit(req).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err
            .message
            .contains("session ownership, provider, mode, or state mismatch"));
    }
}
