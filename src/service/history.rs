//! Explicit account-wide history removal. Interrupted batches never resume automatically.
use super::Service;
use crate::{domain::*, store::Store};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{atomic::Ordering, Arc},
};

pub(crate) type Status = WebHistoryCleanupState;

pub(crate) fn recover(store: &Store) -> Result<()> {
    for mut status in store.list::<Status>("local", "web_history_cleanup")? {
        if status.is_active() {
            status.state = "interrupted".into();
            status.error = "Host stopped before history cleanup completed; review the deleted count and explicitly run clear-history again to continue.".into();
            status.updated_at = now_ms();
            store.put("local", "web_history_cleanup", &status.account_id, &status)?;
        }
    }
    Ok(())
}

impl Service {
    fn history_status(&self, account: &str) -> Result<Status> {
        let _: Account = self.store.get("local", "account", account)?;
        match self.store.get("local", "web_history_cleanup", account) {
            Ok(status) => Ok(status),
            Err(error) if error.code == ErrorCode::NotFound => Ok(Status::idle(account)),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn web_history_active(&self, account: &str) -> Result<bool> {
        Ok(self.history_status(account)?.is_active())
    }
    pub fn web_history_cleanup(&self, account: &str) -> Result<WebHistoryCleanupState> {
        self.history_status(account)
    }
    pub(crate) fn ensure_no_active_agents(&self, account: &str) -> Result<()> {
        if self
            .store
            .list::<AgentRecord>("local", "agent")?
            .iter()
            .any(|a| a.request.account_id == account && a.is_active())
        {
            return Err(ServiceError::invalid(
                "stop the active agent before clearing this account's web history",
            ));
        }
        Ok(())
    }
    pub fn clear_web_history(self: &Arc<Self>, account_id: &str) -> Result<WebHistoryCleanupState> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(ServiceError::invalid("host is stopping"));
        }
        self.ensure_no_active_agents(account_id)?;
        let permit = self.account_gate(account_id).try_acquire_owned().map_err(|_| ServiceError::new(
            ErrorCode::QueueFull, "web_history", "account is busy; finish its current request or login before clearing web history"))?;
        // Account attachment/login can change its profile under this gate.
        // Read the destructive target only after acquiring it.
        let account: Account = self.store.get("local", "account", account_id)?;
        if account.state != "ready" {
            return Err(ServiceError::invalid(
                "sign in and resolve the account before clearing web history",
            ));
        }
        // Admission uses the same lock: a newly submitted turn cannot slip
        // between the active-run check and recording the destructive batch.
        let controls = self.controls.lock().map_err(ServiceError::storage)?;
        if self.stopping.load(Ordering::SeqCst) {
            return Err(ServiceError::invalid("host is stopping"));
        }
        if self.web_history_active(account_id)? {
            return Err(ServiceError::invalid(
                "web history cleanup is already active for this account",
            ));
        }
        for id in controls.keys() {
            let run: Run = self.store.get("local", "run", id)?;
            if run.request.account_id == account_id {
                return Err(ServiceError::invalid(
                    "stop the active provider turn before clearing web history",
                ));
            }
        }
        let mut status = Status::idle(account_id);
        status.state = "pending".into();
        status.started_at = now_ms().max(self.history_status(account_id)?.started_at + 1);
        status.updated_at = status.started_at;
        self.store
            .put("local", "web_history_cleanup", account_id, &status)?;
        drop(controls);
        let response = status.clone();
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            status.state = "running".into();
            status.updated_at = now_ms();
            let result = catch_unwind(AssertUnwindSafe(|| -> Result<u32> {
                service
                    .store
                    .put("local", "web_history_cleanup", &account.id, &status)?;
                service
                    .cleanup_cancel
                    .check()
                    .map_err(|e| ServiceError::backend(e, false))?;
                service.backend.clear_web_history(
                    &account,
                    service.cleanup_cancel.clone(),
                    &mut |url, deleted| {
                        if deleted < status.deleted {
                            return Err(ServiceError::invalid(
                                "web history progress moved backwards",
                            ));
                        }
                        status.deleted = deleted;
                        status.updated_at = now_ms();
                        // Save the irreversible result before changing local cache metadata.
                        service
                            .store
                            .put("local", "web_history_cleanup", &account.id, &status)?;
                        service.mark_web_history_deleted(&account.id, Some(url), None)
                    },
                )
            }))
            .unwrap_or_else(|_| {
                Err(ServiceError::new(
                    ErrorCode::Internal,
                    "web_history",
                    "web history cleanup worker failed",
                ))
            });
            match result {
                Ok(deleted) if deleted >= status.deleted => {
                    status.deleted = deleted;
                    match service.mark_web_history_deleted(&account.id, None, None) {
                        Ok(()) => {
                            status.state = "completed".into();
                            status.error.clear();
                        }
                        Err(error) => {
                            status.state = "failed".into();
                            status.error = error.to_string();
                        }
                    }
                }
                other => {
                    let error = other.err().unwrap_or_else(|| {
                        ServiceError::invalid("web history final count moved backwards")
                    });
                    status.state = if service.stopping.load(Ordering::SeqCst) {
                        "interrupted"
                    } else {
                        "failed"
                    }
                    .into();
                    status.error = error
                        .to_string()
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(1000)
                        .collect();
                }
            }
            status.updated_at = now_ms();
            if let Err(error) =
                service
                    .store
                    .put("local", "web_history_cleanup", &account.id, &status)
            {
                eprintln!("web history cleanup status could not be saved: {error}");
            }
        });
        Ok(response)
    }

    pub(crate) fn mark_web_history_deleted(
        &self,
        account: &str,
        url: Option<&str>,
        peer: Option<(&str, i64)>,
    ) -> Result<()> {
        self.mark_web_history_sessions(account, url, peer, SessionState::Deleted)
    }

    pub(crate) fn invalidate_web_history_sessions(
        &self,
        account: &str,
        peer: &str,
        cutoff: i64,
    ) -> Result<()> {
        self.mark_web_history_sessions(
            account,
            None,
            Some((peer, cutoff)),
            SessionState::Invalidated,
        )
    }

    fn mark_web_history_sessions(
        &self,
        account: &str,
        url: Option<&str>,
        peer: Option<(&str, i64)>,
        state: SessionState,
    ) -> Result<()> {
        for mut session in self.store.list::<Session>("local", "session")? {
            if session.account_id != account || session.mode == SessionMode::Temporary {
                continue;
            }
            if state == SessionState::Invalidated && session.state == SessionState::Deleted {
                continue;
            }
            if let Some(url) = url {
                if session.remote_url.as_deref() != Some(url) {
                    continue;
                }
            }
            if let Some((peer, started_at)) = peer {
                // Status may first be observed after new chats have already
                // been created. Never invalidate those newer sessions.
                if session.last_used > started_at {
                    continue;
                }
                let record: RemoteTurnRef = match self.store.get(&session.id, "remote", "turn") {
                    Ok(record) => record,
                    Err(error) if error.code == ErrorCode::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if record.server_id != peer {
                    continue;
                }
            }
            session.state = state;
            self.store.put("local", "session", &session.id, &session)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::CancelToken,
        service::{Backend, Emit},
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    #[derive(Default)]
    struct Fixture {
        calls: AtomicUsize,
        fail: AtomicBool,
        hold: AtomicBool,
    }
    impl Backend for Fixture {
        fn generate(
            &self,
            _: &Account,
            _: &mut Session,
            _: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            Err(ServiceError::invalid("unused fixture"))
        }
        fn close(&self, _: &Account, _: &Session) -> Result<()> {
            Ok(())
        }
        fn clear_web_history(
            &self,
            _: &Account,
            cancel: CancelToken,
            progress: &mut dyn FnMut(&str, u32) -> Result<()>,
        ) -> Result<u32> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            progress("https://gemini.google.com/app/owned", 1)?;
            while self.hold.load(Ordering::SeqCst) {
                cancel
                    .check()
                    .map_err(|e| ServiceError::backend(e, false))?;
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(ServiceError::invalid(
                    "fixture failed after one confirmed deletion",
                ));
            }
            progress("https://gemini.google.com/app/other", 2)?;
            Ok(2)
        }
    }
    fn fixture(store: &Store) {
        for account in ["default", "separate"] {
            store.put("local","account",account,&json!({"id":account,"provider":"gemini-web","profile":"fixture/profile","port":0,"headless":true,"state":"ready"})).unwrap();
        }
        for (id, account) in [
            ("owned", "default"),
            ("other", "default"),
            ("unrelated", "separate"),
        ] {
            store.put("local","session",id,&json!({"id":id,"provider":"gemini-web","account_id":account,"mode":"chat","target_id":null,"remote_url":format!("https://gemini.google.com/app/{id}"),"history":[],"state":"ready","model":null,"thinking":null,"tool_digest":""})).unwrap();
        }
        store
            .put(
                "local",
                "agent",
                "saved",
                &json!({"id":"saved","state":"completed","request":{"account_id":"default"}}),
            )
            .unwrap();
    }
    async fn finished(service: &Service) -> Status {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = service.history_status("default").unwrap();
                if !status.is_active() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn partial_bulk_failure_is_durable_and_requires_explicit_retry() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store);
        let backend = Arc::new(Fixture::default());
        backend.fail.store(true, Ordering::SeqCst);
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        let ack = service.clear_web_history("default").unwrap();
        assert_eq!(ack.state, "pending");
        let failed = finished(&service).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.deleted, 1);
        assert!(failed.error.contains("confirmed deletion"));
        assert_eq!(
            store
                .get::<Session>("local", "session", "owned")
                .unwrap()
                .state,
            SessionState::Deleted
        );
        assert_eq!(
            store
                .get::<Session>("local", "session", "other")
                .unwrap()
                .state,
            SessionState::Ready
        );
        let restarted =
            Service::new(Store::open(directory.path()).unwrap(), backend.clone()).unwrap();
        assert_eq!(restarted.history_status("default").unwrap().state, "failed");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let retry = restarted.clear_web_history("default").unwrap();
        assert!(retry.started_at > failed.started_at);
        assert_eq!(finished(&restarted).await.state, "completed");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .get::<Session>("local", "session", "other")
                .unwrap()
                .state,
            SessionState::Deleted
        );
        assert_eq!(
            store
                .get::<Session>("local", "session", "unrelated")
                .unwrap()
                .state,
            SessionState::Ready
        );
        assert!(
            store
                .get::<serde_json::Value>("local", "agent", "saved")
                .is_ok(),
            "bulk removal preserves /resume entries"
        );
    }
    #[tokio::test]
    async fn bulk_holds_account_gate_blocks_new_turns_and_recovers_as_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        fixture(&store);
        let backend = Arc::new(Fixture::default());
        backend.hold.store(true, Ordering::SeqCst);
        let service = Service::new(store.clone(), backend.clone()).unwrap();
        let permit = service
            .account_gate("default")
            .acquire_owned()
            .await
            .unwrap();
        assert!(service
            .clear_web_history("default")
            .unwrap_err()
            .message
            .contains("busy"));
        drop(permit);
        service.clear_web_history("default").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while service.history_status("default").unwrap().deleted == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(service.account_gate("default").try_acquire_owned().is_err());
        assert!(service
            .submit_async(TurnRequest::text("cannot race cleanup"))
            .await
            .unwrap_err()
            .message
            .contains("cleanup is active"));
        assert!(service.clear_web_history("default").is_err());
        service.begin_shutdown();
        let stopped = finished(&service).await;
        assert_eq!(stopped.state, "interrupted");
        assert_eq!(stopped.deleted, 1);
        let mut crash = stopped.clone();
        crash.state = "running".into();
        store
            .put("local", "web_history_cleanup", "default", &crash)
            .unwrap();
        let restarted = Service::new(store.clone(), backend.clone()).unwrap();
        assert_eq!(
            restarted.history_status("default").unwrap().state,
            "interrupted"
        );
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            1,
            "restart must not repeat bulk deletion"
        );
    }
}
