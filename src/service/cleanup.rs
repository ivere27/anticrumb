//! Durable, account-serialized cleanup of web conversations after local deletion.
use super::Service;
use crate::{domain::*, store::Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Job {
    pub id: String,
    pub account_id: String,
    pub sessions: Vec<String>,
    pub runs: Vec<String>,
    #[serde(default)]
    pub completed_sessions: Vec<String>,
    pub state: String,
    pub attempts: u32,
    pub next_attempt: i64,
    pub updated_at: i64,
    pub error: String,
}

/// Ownership references are captured in the same transaction that removes the
/// agent and its journal. Titles, prompt text, and timestamps are never matched.
pub(crate) fn build_job(id: &str, account: &str, ownership_events: Vec<(String, Value)>) -> Job {
    let mut sessions = BTreeSet::new();
    let mut runs = BTreeSet::new();
    for (kind, data) in ownership_events {
        if kind == EVENT_BROWSER_TURN {
            if let Some(id) = data["run_id"].as_str() {
                runs.insert(id.to_owned());
            }
        } else if let Some(id) = data["session_id"].as_str() {
            sessions.insert(id.to_owned());
        }
    }
    let mapped = !sessions.is_empty() || !runs.is_empty();
    Job {
        id: id.into(),
        account_id: account.into(),
        sessions: sessions.into_iter().collect(),
        runs: runs.into_iter().collect(),
        completed_sessions: Vec::new(),
        state: if mapped { "pending" } else { "unmapped" }.into(),
        attempts: 0,
        next_attempt: 0,
        updated_at: now_ms(),
        error: if mapped {
            String::new()
        } else {
            "Saved entry has no recorded web conversation identity (older entries cannot be matched safely); its web chat was not deleted.".into()
        },
    }
}

pub(crate) fn status(store: &Store) -> Result<ChatCleanupSummary> {
    let mut summary = ChatCleanupSummary::default();
    let mut latest = 0;
    for job in store.list::<Job>("local", "chat_cleanup")? {
        match job.state.as_str() {
            "pending" => summary.pending += 1,
            "running" => summary.running += 1,
            "failed" => summary.failed += 1,
            "completed" => summary.completed += 1,
            "unmapped" => summary.unmapped += 1,
            _ => {}
        }
        if !job.error.is_empty() && job.updated_at >= latest {
            latest = job.updated_at;
            summary.last_error = job.error;
        }
    }
    Ok(summary)
}

impl Service {
    pub(crate) fn start_chat_cleanup(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // Only notify when the loop is already running. The first spawn does not
        // need a permit; it starts with a fresh DB query immediately.
        if self.cleanup_running.swap(true, Ordering::SeqCst) {
            self.cleanup_notify.notify_one();
            return;
        }
        enum NextCleanup {
            RunNow,
            WaitUntil(i64),
            Idle,
        }
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            let mut recovered = false;
            loop {
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if service.stopping.load(Ordering::SeqCst) || service.cleanup_cancel.is_cancelled()
                {
                    break;
                }
                let notify = service.cleanup_notify.clone();
                let cancel = service.cleanup_cancel.clone();
                // Register interest in the notification BEFORE the DB query so
                // we never miss a notify_one() that fires while we read the store.
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let result = async {
                    if !recovered {
                        for mut job in service.store.list::<Job>("local", "chat_cleanup")? {
                            if matches!(job.state.as_str(), "running" | "failed") {
                                job.state = "pending".into();
                                job.attempts = 0;
                                job.next_attempt = 0;
                                service.store.put("local", "chat_cleanup", &job.id, &job)?;
                            }
                        }
                        recovered = true;
                    }
                    let now = now_ms();
                    let jobs = service.store.list::<Job>("local", "chat_cleanup")?;
                    if let Some(job) = jobs
                        .iter()
                        .find(|j| j.state == "pending" && j.next_attempt <= now)
                        .cloned()
                    {
                        service.run_chat_cleanup(job).await?;
                        return Ok::<_, ServiceError>(NextCleanup::RunNow);
                    }
                    if let Some(next_attempt) = jobs
                        .iter()
                        .filter(|j| j.state == "pending")
                        .map(|j| j.next_attempt)
                        .min()
                    {
                        return Ok(NextCleanup::WaitUntil(next_attempt));
                    }
                    Ok(NextCleanup::Idle)
                }
                .await;
                drop(service);
                match result {
                    Ok(NextCleanup::RunNow) => {
                        tokio::task::yield_now().await;
                    }
                    Ok(NextCleanup::WaitUntil(next_attempt)) => {
                        let delay_ms = (next_attempt - now_ms()).max(1) as u64;
                        tokio::select! {
                            _ = &mut notified => {}
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                        }
                    }
                    Ok(NextCleanup::Idle) => {
                        tokio::select! {
                            _ = &mut notified => {}
                            _ = cancel.cancelled() => break,
                        }
                    }
                    Err(error) => {
                        eprintln!("web chat cleanup: {error}");
                        tokio::select! {
                            _ = &mut notified => {}
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
                    }
                }
            }
        });
    }

    fn shared_session(&self, session: &str, runs: &[String]) -> Result<bool> {
        self.store.has_shared_browser_session(session, runs)
    }

    async fn run_chat_cleanup(self: &Arc<Self>, mut job: Job) -> Result<()> {
        job.state = "running".into();
        job.attempts += 1;
        job.updated_at = now_ms();
        self.store.put("local", "chat_cleanup", &job.id, &job)?;
        let result = async {
            for id in &job.runs {
                let run: Run = self.store.get("local", "run", id)?;
                if run.request.account_id != job.account_id {
                    return Err(ServiceError::invalid(
                        "web cleanup account ownership mismatch",
                    ));
                }
                if matches!(run.state.as_str(), "queued" | "running" | "preparing" | "submitted") {
                    return Err(ServiceError::invalid(
                        "provider turn is still running; cleanup will retry",
                    ));
                }
                if let Some(result) = run.result {
                    if !job.sessions.contains(&result.session_id) {
                        job.sessions.push(result.session_id);
                    }
                }
                if let Some(session) = run.request.session_id {
                    if !job.sessions.contains(&session) {
                        job.sessions.push(session);
                    }
                }
                let mut after = 0;
                loop {
                    let events = self.store.events(id, after)?;
                    if events.is_empty() {
                        break;
                    }
                    for event in events {
                        after = event.sequence;
                        if event.kind == "session" {
                            if let Some(session) = event.data["session_id"].as_str() {
                                if !job.sessions.iter().any(|s| s == session) {
                                    job.sessions.push(session.into());
                                }
                            }
                        }
                    }
                }
            }
            if job.sessions.is_empty() {
                return Err(ServiceError::new(ErrorCode::NotFound, "chat_cleanup_identity",
                    "Provider turn has no recorded web conversation identity; its web chat was not deleted."));
            }
            self.store.put("local", "chat_cleanup", &job.id, &job)?;
            for id in job.sessions.clone() {
                if job.completed_sessions.contains(&id) {
                    continue;
                }
                let session: Session = self.store.get("local", "session", &id)?;
                if session.account_id != job.account_id {
                    return Err(ServiceError::invalid(
                        "web cleanup session account mismatch",
                    ));
                }
                if session.state != SessionState::Deleted && self.shared_session(&id, &job.runs)? {
                    return Err(ServiceError::invalid(
                        "web chat is referenced by another saved entry; cleanup retained it",
                    ));
                }
                self.delete_conversation(&id).await?;
                job.completed_sessions.push(id);
                self.store.put("local", "chat_cleanup", &job.id, &job)?;
            }
            Ok::<_, ServiceError>(())
        }
        .await;
        job.updated_at = now_ms();
        match result {
            Ok(()) => {
                job.state = "completed".into();
                job.error.clear();
            }
            Err(error) => {
                job.error = error
                    .to_string()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(1000)
                    .collect();
                if error.stage == "chat_cleanup_identity" {
                    job.state = "unmapped".into();
                } else if self.stopping.load(Ordering::SeqCst) || job.attempts < 3 {
                    job.state = "pending".into();
                    job.next_attempt = now_ms() + if job.attempts < 2 { 3_000 } else { 15_000 };
                } else {
                    job.state = "failed".into();
                }
            }
        }
        self.store.put("local", "chat_cleanup", &job.id, &job)
    }

    pub async fn delete_conversation(self: &Arc<Self>, id: &str) -> Result<()> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(ServiceError::invalid("host is stopping"));
        }
        let session: Session = self.store.get("local", "session", id)?;
        for attempt in 0..2 {
            let browser_gate = self.browser_gate(&session.provider);
            let browser_permit = tokio::select! {
                guard = browser_gate.read_owned() => guard,
                _ = self.shutdown.notified() => return Err(ServiceError::invalid("host is stopping")),
            };
            let gate = self.account_gate(&session.account_id);
            let permit = tokio::select! {
                result = gate.acquire_owned() => result.map_err(ServiceError::storage)?,
                _ = self.shutdown.notified() => return Err(ServiceError::invalid("host is stopping")),
            };
            let service = self.clone();
            let session_id = session.id.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _browser_permit = browser_permit;
                let _permit = permit;
                let mut session: Session = service.store.get("local", "session", &session_id)?;
                if session.state == SessionState::Deleted {
                    return Ok(());
                }
                // Recheck after acquiring the account gate: another saved turn
                // may have acquired this session while cleanup was waiting.
                if service.shared_session(&session.id, &[])? {
                    return Err(ServiceError::invalid(
                        "web chat is referenced by another saved entry; cleanup retained it",
                    ));
                }
                if session.state == SessionState::Running {
                    return Err(ServiceError::invalid("web conversation is still running"));
                }
                let account: Account =
                    service.store.get("local", "account", &session.account_id)?;
                if session.mode != SessionMode::Temporary {
                    if account.state != "ready" {
                        return Err(ServiceError::invalid(
                            "sign in and resolve the account before deleting web chats",
                        ));
                    }
                    service.backend.delete_conversation(
                        &account,
                        &session,
                        service.cleanup_cancel.clone(),
                    )?;
                } else if !session.is_inactive() {
                    if let Err(err) = service.backend.close(&account, &session) {
                        if err.code != ErrorCode::NotFound {
                            return Err(err);
                        }
                    }
                    session.target_id = None;
                }
                session.state = SessionState::Deleted;
                service.store.put("local", "session", &session.id, &session)
            })
            .await
            .map_err(ServiceError::storage)?;
            match result {
                Ok(()) => return Ok(()),
                Err(err) if attempt == 0 && err.stage == "identity_mismatch" => {
                    let _write = self.browser_gate(&session.provider).write_owned().await;
                    let refresh_service = self.clone();
                    let provider_id = session.provider.clone();
                    let cancel = self.cleanup_cancel.clone();
                    tokio::task::spawn_blocking(move || {
                        refresh_service
                            .backend
                            .refresh_accounts(&provider_id, cancel)
                    })
                    .await
                    .map_err(ServiceError::storage)??;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(ServiceError::new(
            ErrorCode::AuthRequired,
            "identity_mismatch",
            "account identity could not be verified after rescan",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::CancelToken,
        agent_service::AgentManager,
        service::{Backend, Emit},
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize},
        Mutex,
    };

    #[derive(Default)]
    struct DeleteBackend {
        calls: AtomicUsize,
        fail: AtomicBool,
        identities: Mutex<Vec<(String, String)>>,
        closed: Mutex<Vec<String>>,
    }
    impl Backend for DeleteBackend {
        fn generate(
            &self,
            _: &Account,
            _: &mut Session,
            _: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            Err(ServiceError::invalid("fixture does not generate"))
        }
        fn close(&self, _: &Account, session: &Session) -> Result<()> {
            self.closed.lock().unwrap().push(session.id.clone());
            Ok(())
        }
        fn delete_conversation(
            &self,
            account: &Account,
            session: &Session,
            cancel: CancelToken,
        ) -> Result<()> {
            cancel
                .check()
                .map_err(|e| ServiceError::backend(e, false))?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.identities
                .lock()
                .unwrap()
                .push((account.id.clone(), session.id.clone()));
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(ServiceError::invalid("fixture cleanup failed"));
            }
            Ok(())
        }
    }
    fn fixture(store: &Store, mode: &str) {
        store.put("local", "account", "default", &json!({"id":"default","provider":"gemini-web","profile":"fixture/profile","port":0,"headless":true,"state":"ready"})).unwrap();
        store.put("local", "session", "owned", &json!({"id":"owned","provider":"gemini-web","account_id":"default","mode":mode,"target_id":null,"remote_url":"https://gemini.google.com/app/fixture","history":[],"state":"closed","model":null,"thinking":null,"tool_digest":""})).unwrap();
    }
    fn agent(store: &Store, id: &str) {
        agent_in_conversation(store, id, id, "completed", 1);
    }
    fn agent_in_conversation(
        store: &Store,
        conv_id: &str,
        turn_id: &str,
        state: &str,
        created_at: i64,
    ) {
        store.put(
            "local",
            "agent",
            turn_id,
            &json!({
                "id":turn_id,
                "conversation_id":conv_id,
                "state":state,
                "request":{"task":"fixture task","workspace":"fixture","account_id":"default","conversation_id":conv_id},
                "created_at":created_at,
                "updated_at":created_at + 1
            }),
        ).unwrap();
    }
    fn queue(store: &Store, id: &str) -> Job {
        agent(store, id);
        store
            .event(id, "browser_session", json!({"session_id":"owned"}))
            .unwrap();
        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation(id, None, None)
            .unwrap();
        store.get("local", "chat_cleanup", id).unwrap()
    }
    #[tokio::test]
    async fn local_delete_commits_cleanup_identity_before_removing_journal_and_retries() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store, "chat");
        let backend = Arc::new(DeleteBackend::default());
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        let job = queue(&store, "deleted");
        assert!(store.get::<Value>("local", "agent", "deleted").is_err());
        assert!(store.events("deleted", 0).unwrap().is_empty());
        assert_eq!(job.sessions, ["owned"]);
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            0,
            "local deletion cannot wait for network I/O"
        );
        backend.fail.store(true, Ordering::SeqCst);
        service.run_chat_cleanup(job).await.unwrap();
        let retry: Job = store.get("local", "chat_cleanup", "deleted").unwrap();
        assert_eq!(retry.state, "pending");
        assert!(retry.error.contains("fixture cleanup failed"));
        assert_eq!(
            store
                .get::<Session>("local", "session", "owned")
                .unwrap()
                .state,
            SessionState::Closed
        );
        service.run_chat_cleanup(retry).await.unwrap();
        assert_eq!(status(&store).unwrap().completed, 1);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .get::<Session>("local", "session", "owned")
                .unwrap()
                .state,
            SessionState::Deleted
        );
        service.delete_conversation("owned").await.unwrap();
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            2,
            "repeated cleanup is idempotent"
        );
        assert_eq!(
            backend.identities.lock().unwrap()[0],
            ("default".into(), "owned".into())
        );
    }
    #[tokio::test]
    async fn missing_legacy_links_temporary_sessions_and_shared_chats_never_delete_unrelated_history(
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store, "temporary");
        let backend = Arc::new(DeleteBackend::default());
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        agent(&store, "legacy");
        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation("legacy", None, None)
            .unwrap();
        assert_eq!(status(&store).unwrap().unmapped, 1);
        service
            .run_chat_cleanup(queue(&store, "temporary"))
            .await
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        fixture(&store, "chat");
        let job = queue(&store, "deleted");
        agent(&store, "surviving");
        store
            .event(
                "surviving",
                "browser_session",
                json!({"session_id":"owned"}),
            )
            .unwrap();
        service.run_chat_cleanup(job).await.unwrap();
        let blocked: Job = store.get("local", "chat_cleanup", "deleted").unwrap();
        assert!(blocked.error.contains("another saved entry"));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(store.get::<Value>("local", "agent", "surviving").is_ok());
        // Explicit account-wide cleanup can delete the web chat while keeping
        // local entries. A previously queued job must then complete quietly.
        let mut session: Session = store.get("local", "session", "owned").unwrap();
        session.state = SessionState::Deleted;
        store.put("local", "session", "owned", &session).unwrap();
        service.run_chat_cleanup(blocked).await.unwrap();
        let completed: Job = store.get("local", "chat_cleanup", "deleted").unwrap();
        assert_eq!(completed.state, "completed");
        assert!(completed.error.is_empty());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn run_identity_resolves_interrupted_session_and_cleanup_waits_for_account_gate() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store, "chat");
        let backend = Arc::new(DeleteBackend::default());
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        let mut request = TurnRequest::text("fixture request");
        request.account_id = "default".into();
        store
            .put(
                "local",
                "run",
                "interrupted",
                &Run {
                    id: "interrupted".into(),
                    provider: default_provider(),
                    state: "cancelled".into(),
                    request,
                    result: None,
                    error: None,
                    created_at: 0,
                    updated_at: 0,
                },
            )
            .unwrap();
        store
            .event("interrupted", "session", json!({"session_id":"owned"}))
            .unwrap();
        agent(&store, "deleted");
        store
            .event("deleted", "browser_turn", json!({"run_id":"interrupted"}))
            .unwrap();
        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation("deleted", None, None)
            .unwrap();
        let job: Job = store.get("local", "chat_cleanup", "deleted").unwrap();
        assert!(job.sessions.is_empty());
        let permit = service
            .account_gate("default")
            .acquire_owned()
            .await
            .unwrap();
        let worker = service.clone();
        let task = tokio::spawn(async move { worker.run_chat_cleanup(job).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        drop(permit);
        task.await.unwrap().unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let saved: Job = Store::open(directory.path())
            .unwrap()
            .get("local", "chat_cleanup", "deleted")
            .unwrap();
        assert_eq!(saved.state, "completed");
        assert_eq!(saved.completed_sessions, ["owned"]);
    }
    #[tokio::test]
    async fn terminal_provider_without_session_identity_is_unmapped_not_completed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store, "chat");
        let backend = Arc::new(DeleteBackend::default());
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        store
            .put(
                "local",
                "run",
                "unmapped",
                &Run {
                    id: "unmapped".into(),
                    provider: default_provider(),
                    state: "cancelled".into(),
                    request: {
                        let mut r = TurnRequest::text("fixture");
                        r.account_id = "default".into();
                        r
                    },
                    result: None,
                    error: None,
                    created_at: 0,
                    updated_at: 0,
                },
            )
            .unwrap();
        agent(&store, "deleted");
        store
            .event("deleted", "browser_turn", json!({"run_id":"unmapped"}))
            .unwrap();
        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation("deleted", None, None)
            .unwrap();
        service
            .run_chat_cleanup(store.get("local", "chat_cleanup", "deleted").unwrap())
            .await
            .unwrap();
        let saved: Job = store.get("local", "chat_cleanup", "deleted").unwrap();
        assert_eq!(saved.state, "unmapped");
        assert!(saved.error.contains("was not deleted"));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn deleting_saved_conversation_closes_live_temporary_and_subagent_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .put(
                "local",
                "account",
                "default",
                &json!({"id":"default","provider":"gemini-web","profile":"fixture/profile","port":0,"headless":true,"state":"ready"}),
            )
            .unwrap();
        for (sid, tab) in [("temp-main", "tab-main"), ("temp-sub", "tab-sub")] {
            store
                .put(
                    "local",
                    "session",
                    sid,
                    &json!({"id":sid,"provider":"gemini-web","account_id":"default","mode":"temporary","target_id":tab,"remote_url":"https://gemini.google.com/app","history":[],"state":"ready","model":null,"thinking":null,"tool_digest":""}),
                )
                .unwrap();
        }
        let backend = Arc::new(DeleteBackend::default());
        let service = Service::new(store.clone(), backend.clone()).unwrap();

        // Shared subagent session referenced by a surviving conversation is retained while shared.
        agent(&store, "conv-deleted");
        store
            .event(
                "conv-deleted",
                "browser_session",
                json!({"session_id":"temp-main"}),
            )
            .unwrap();
        store
            .event(
                "conv-deleted",
                "subagent_browser_session",
                json!({"session_id":"temp-sub"}),
            )
            .unwrap();
        agent(&store, "conv-surviving");
        store
            .event(
                "conv-surviving",
                "subagent_browser_session",
                json!({"session_id":"temp-sub"}),
            )
            .unwrap();

        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation("conv-deleted", None, None)
            .unwrap();
        let job: Job = store.get("local", "chat_cleanup", "conv-deleted").unwrap();
        assert_eq!(job.sessions, ["temp-main", "temp-sub"]);
        service.run_chat_cleanup(job).await.unwrap();

        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.closed.lock().unwrap().as_slice(), &["temp-main"]);
        let main_sess: Session = store.get("local", "session", "temp-main").unwrap();
        assert_eq!(main_sess.state, SessionState::Deleted);
        assert!(main_sess.target_id.is_none());
        let sub_sess: Session = store.get("local", "session", "temp-sub").unwrap();
        assert_eq!(sub_sess.state, SessionState::Ready);
        assert_eq!(sub_sess.target_id.as_deref(), Some("tab-sub"));

        // Once the surviving conversation is deleted too, the retained subagent session closes cleanly.
        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation("conv-surviving", None, None)
            .unwrap();
        let job2: Job = store
            .get("local", "chat_cleanup", "conv-surviving")
            .unwrap();
        assert_eq!(job2.sessions, ["temp-sub"]);
        service.run_chat_cleanup(job2).await.unwrap();

        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            backend.closed.lock().unwrap().as_slice(),
            &["temp-main", "temp-sub"]
        );
        let sub_sess: Session = store.get("local", "session", "temp-sub").unwrap();
        assert_eq!(sub_sess.state, SessionState::Deleted);
        assert!(sub_sess.target_id.is_none());
    }
    #[tokio::test]
    async fn deleting_multiturn_conversation_removes_all_turns_and_cleans_shared_web_chat() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store, "chat");
        let backend = Arc::new(DeleteBackend::default());
        let service = Service::new(store.clone(), backend.clone()).unwrap();

        agent_in_conversation(&store, "conv-multi", "turn-1", "completed", 10);
        store
            .event("turn-1", "browser_session", json!({"session_id":"owned"}))
            .unwrap();
        agent_in_conversation(&store, "conv-multi", "turn-2", "completed", 20);
        store
            .event("turn-2", "browser_session", json!({"session_id":"owned"}))
            .unwrap();
        // Leaf turn failed before recording a browser session; deleting the conversation must
        // still collect earlier turns' sessions and delete the web chat without shared_session conflict.
        agent_in_conversation(&store, "conv-multi", "turn-3", "failed", 30);

        AgentManager::new(store.clone())
            .unwrap()
            .delete_conversation("conv-multi", None, None)
            .unwrap();
        for turn in ["turn-1", "turn-2", "turn-3"] {
            assert!(store.get::<Value>("local", "agent", turn).is_err());
            assert!(store.events(turn, 0).unwrap().is_empty());
        }
        let job: Job = store.get("local", "chat_cleanup", "conv-multi").unwrap();
        assert_eq!(job.sessions, ["owned"]);
        service.run_chat_cleanup(job).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(status(&store).unwrap().completed, 1);
    }
}
