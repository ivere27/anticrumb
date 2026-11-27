//! A login request owns one bounded, cancellable account sync and catalog refresh per provider.
use super::{persist_probe, sync_discovered_accounts};
use crate::{
    agent::CancelToken,
    domain::*,
    provider::{DiscoveredAccount, Provider, ProviderContext},
    service::BrowserGate,
    store::Store,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct Watch {
    generation: String,
    cancel: CancelToken,
}

#[derive(Default)]
pub(crate) struct Monitors {
    active: Mutex<HashMap<String, Watch>>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stopping: AtomicBool,
}

impl Monitors {
    pub fn recover(store: &Store) -> Result<()> {
        for browser in store.list_browser_profiles()? {
            if super::provider_login_status(store, &browser.provider_id)
                .is_some_and(|status| matches!(status.state.as_str(), "waiting" | "refreshing"))
            {
                store.put(
                    "local",
                    "provider_login",
                    &browser.provider_id,
                    &LoginStatus {
                        state: "failed".into(),
                        error: "Login monitoring stopped when the browser host exited. Open login again or run /model refresh after signing in.".into(),
                    },
                )?;
            }
        }
        Ok(())
    }

    pub fn cancel(&self, store: &Store, provider_id: &str) -> Result<bool> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let previous = active.remove(provider_id);
        if let Some(watch) = &previous {
            watch.cancel.cancel();
        }
        // Serialize clearing with a monitor's final persistence, so an obsolete
        // task cannot overwrite a new login, attachment, or refresh.
        store.put(
            "local",
            "provider_login",
            provider_id,
            &Option::<LoginStatus>::None,
        )?;
        Ok(previous.is_some())
    }

    pub fn cancel_all(&self, store: &Store) {
        self.stopping.store(true, Ordering::SeqCst);
        // Serialize with registration, including concurrent shutdown callers.
        // Every caller returns only after all workers have left module code.
        let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            for (provider_id, watch) in active.drain() {
                watch.cancel.cancel();
                let _ = store.put(
                    "local",
                    "provider_login",
                    &provider_id,
                    &Option::<LoginStatus>::None,
                );
            }
        }
        // Workers take active while completing; never join with that lock held.
        for worker in workers.drain(..) {
            let _ = worker.join();
        }
    }

    fn begin(&self, store: &Store, provider_id: &str) -> Result<Watch> {
        let watch = Watch {
            generation: id("login"),
            cancel: CancelToken::new(),
        };
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopping.load(Ordering::SeqCst) {
            return Err(ServiceError::new(
                ErrorCode::Cancelled,
                "login",
                "browser host is shutting down",
            ));
        }
        if let Some(previous) = active.remove(provider_id) {
            previous.cancel.cancel();
        }
        store.put(
            "local",
            "provider_login",
            provider_id,
            &LoginStatus {
                state: "waiting".into(),
                error: String::new(),
            },
        )?;
        active.insert(provider_id.into(), watch.clone());
        Ok(watch)
    }

    pub fn start(
        self: &Arc<Self>,
        store: Store,
        browser: BrowserProfile,
        provider: Arc<dyn Provider>,
        gate: Arc<BrowserGate>,
    ) -> Result<()> {
        let multi_account = provider.info().capabilities.multi_account;
        let ready_provider = provider.clone();
        let discover_provider = provider.clone();
        let refresh_provider = provider;
        let discover_store = store.clone();
        let refresh_store = store.clone();
        self.start_with(
            store,
            browser,
            multi_account,
            gate,
            move |b, cancel| ready_provider.is_login_ready(b, cancel),
            move |b, cancel| {
                discover_provider.discover_accounts(
                    &ProviderContext::with_store(&discover_store),
                    b,
                    cancel,
                )
            },
            move |b, acct, cancel| {
                refresh_provider.discover_models(
                    &ProviderContext::with_store(&refresh_store),
                    b,
                    acct,
                    cancel,
                )
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with(
        self: &Arc<Self>,
        store: Store,
        browser: BrowserProfile,
        multi_account: bool,
        gate: Arc<BrowserGate>,
        ready: impl FnMut(&BrowserProfile, CancelToken) -> Result<bool> + Send + 'static,
        discover: impl FnMut(&BrowserProfile, CancelToken) -> Result<Vec<DiscoveredAccount>>
            + Send
            + 'static,
        refresh: impl FnMut(&BrowserProfile, &Account, CancelToken) -> Result<ModelCatalogRecord>
            + Send
            + 'static,
    ) -> Result<()> {
        // Keep begin/spawn/registration indivisible with respect to shutdown.
        let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        for index in (0..workers.len()).rev() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            }
        }
        let watch = self.begin(&store, &browser.provider_id)?;
        let monitors = self.clone();
        let failed_store = store.clone();
        let failed_provider = browser.provider_id.clone();
        let failed_watch = watch.clone();
        let worker = std::thread::Builder::new()
            .name("login-models".into())
            .spawn(move || {
                monitors.run_with(
                    &store,
                    &browser,
                    multi_account,
                    &watch,
                    gate,
                    WAIT,
                    POLL,
                    ready,
                    discover,
                    refresh,
                );
            })
            .map_err(|error| {
                self.finish(
                    &failed_store,
                    &failed_provider,
                    &failed_watch,
                    "failed",
                    "Could not start login monitoring. Run /model refresh after signing in.",
                );
                ServiceError::storage(error)
            })?;
        workers.push(worker);
        Ok(())
    }

    fn current(active: &HashMap<String, Watch>, provider_id: &str, watch: &Watch) -> bool {
        !watch.cancel.is_cancelled()
            && active
                .get(provider_id)
                .is_some_and(|value| value.generation == watch.generation)
    }

    fn finish(&self, store: &Store, provider_id: &str, watch: &Watch, state: &str, error: &str) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if Self::current(&active, provider_id, watch) {
            let _ = store.put(
                "local",
                "provider_login",
                provider_id,
                &LoginStatus {
                    state: state.into(),
                    error: error.into(),
                },
            );
            active.remove(provider_id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with(
        &self,
        store: &Store,
        expected: &BrowserProfile,
        multi_account: bool,
        watch: &Watch,
        gate: Arc<BrowserGate>,
        timeout: Duration,
        interval: Duration,
        mut ready: impl FnMut(&BrowserProfile, CancelToken) -> Result<bool>,
        mut discover: impl FnMut(&BrowserProfile, CancelToken) -> Result<Vec<DiscoveredAccount>>,
        mut refresh: impl FnMut(&BrowserProfile, &Account, CancelToken) -> Result<ModelCatalogRecord>,
    ) {
        let deadline = Instant::now() + timeout;
        while !watch.cancel.is_cancelled() && Instant::now() < deadline {
            let Ok(browser) = store.browser_profile(&expected.provider_id) else {
                break;
            };
            if browser.port != expected.port
                || browser.profile != expected.profile
                || browser.subprofile != expected.subprofile
            {
                break;
            }
            if ready(&browser, watch.cancel.clone()).unwrap_or(false) {
                if let Ok(_permit) = gate.clone().try_write_owned() {
                    // Re-verify generation and browser identity under the provider browser gate.
                    let latest = {
                        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                        if !Self::current(&active, &browser.provider_id, watch) {
                            return;
                        }
                        let Ok(latest) = store.browser_profile(&expected.provider_id) else {
                            return;
                        };
                        if latest.port != expected.port
                            || latest.profile != expected.profile
                            || latest.subprofile != expected.subprofile
                        {
                            return;
                        }
                        if store
                            .put(
                                "local",
                                "provider_login",
                                &latest.provider_id,
                                &LoginStatus {
                                    state: "refreshing".into(),
                                    error: String::new(),
                                },
                            )
                            .is_err()
                        {
                            return;
                        }
                        latest
                    };

                    // 1. Discover and sync signed-in accounts from this provider's Chrome profile.
                    let discovered = match discover(&latest, watch.cancel.clone()) {
                        Ok(accounts) => accounts,
                        Err(error) => {
                            self.finish(
                                store,
                                &latest.provider_id,
                                watch,
                                "failed",
                                &refresh_failure(&error),
                            );
                            return;
                        }
                    };

                    let accounts = {
                        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                        if !Self::current(&active, &latest.provider_id, watch) {
                            return;
                        }
                        if !discovered.is_empty() {
                            match sync_discovered_accounts(
                                store,
                                &latest.provider_id,
                                multi_account,
                                discovered,
                            ) {
                                Ok(list) => list,
                                Err(error) => {
                                    drop(active);
                                    self.finish(
                                        store,
                                        &latest.provider_id,
                                        watch,
                                        "failed",
                                        &refresh_failure(&error),
                                    );
                                    return;
                                }
                            }
                        } else {
                            match store.accounts_for_provider(&latest.provider_id) {
                                Ok(list) => list,
                                Err(error) => {
                                    drop(active);
                                    self.finish(
                                        store,
                                        &latest.provider_id,
                                        watch,
                                        "failed",
                                        &refresh_failure(&error),
                                    );
                                    return;
                                }
                            }
                        }
                    };

                    // 2. Refresh model catalogs for non-disabled accounts that don't have cached models yet.
                    let mut first_error: Option<ServiceError> = None;
                    for mut acct in accounts {
                        if acct.state == "disabled" {
                            continue;
                        }
                        if acct.state == "auth_required"
                            && acct.slot.is_none()
                            && acct.identity.is_some()
                        {
                            continue;
                        }
                        if has_cached_models(store, &acct.id) {
                            let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                            if !Self::current(&active, &latest.provider_id, watch) {
                                return;
                            }
                            if acct.state != "ready" {
                                acct.state = "ready".into();
                                if let Err(error) = store.put("local", "account", &acct.id, &acct) {
                                    if first_error.is_none() {
                                        first_error = Some(error);
                                    }
                                }
                            }
                            continue;
                        }
                        let result = refresh(&latest, &acct, watch.cancel.clone());
                        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                        if !Self::current(&active, &latest.provider_id, watch) {
                            return;
                        }
                        let persisted = persist_probe(store, &mut acct, &result);
                        if let Some(error) = result.err().or(persisted.err()) {
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                        }
                    }

                    let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                    if !Self::current(&active, &latest.provider_id, watch) {
                        return;
                    }
                    let status = match first_error {
                        None => LoginStatus {
                            state: "ready".into(),
                            error: String::new(),
                        },
                        Some(error) => LoginStatus {
                            state: "failed".into(),
                            error: refresh_failure(&error),
                        },
                    };
                    let _ = store.put("local", "provider_login", &latest.provider_id, &status);
                    active.remove(&latest.provider_id);
                    return;
                }
            }
            if watch.cancel.sleep_or_cancelled(
                interval.min(deadline.saturating_duration_since(Instant::now())),
            ) {
                return;
            }
        }
        self.finish(
            store,
            &expected.provider_id,
            watch,
            "timed_out",
            "Sign-in was not confirmed within 10 minutes. Open login again or run /model refresh after signing in.",
        );
    }
}

fn refresh_failure(error: &ServiceError) -> String {
    // Keep the actual browser UI diagnostic useful without rendering terminal
    // controls or copying an unbounded error into every status response.
    let detail = if matches!(
        error.code,
        ErrorCode::UiContractChanged | ErrorCode::DeadlineExceeded
    ) {
        let mut characters = error.message.chars().filter(|c| !c.is_control());
        let mut message: String = characters.by_ref().take(512).collect();
        if characters.next().is_some() {
            message.push('…');
        }
        let message = message.trim().trim_end_matches('.');
        if message.is_empty() {
            String::new()
        } else {
            format!(": {message}")
        }
    } else {
        String::new()
    };
    format!(
        "Automatic model refresh failed ({:?} at {}){detail}. Retry with /model refresh.",
        error.code, error.stage
    )
}

fn has_cached_models(store: &Store, account_id: &str) -> bool {
    store
        .model_catalog_record(account_id)
        .map(|catalog| !catalog.models.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (
        tempfile::TempDir,
        Store,
        BrowserProfile,
        Account,
        Arc<Monitors>,
    ) {
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
        store
            .put(
                &account.id,
                "catalog",
                "models",
                &json!({"models":[{"label":"cached"}]}),
            )
            .unwrap();
        (
            directory,
            store,
            browser,
            account,
            Arc::new(Monitors::default()),
        )
    }

    fn empty_catalog(store: &Store, account_id: &str) {
        store
            .put(
                account_id,
                "catalog",
                "models",
                &json!({"models":[],"thinking":[],"model_thinking":{}}),
            )
            .unwrap();
    }

    fn test_catalog(label: &str) -> ModelCatalogRecord {
        ModelCatalogRecord {
            models: vec![ModelOption {
                label: label.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn login_only_refreshes_once_after_authenticated_page_is_ready() {
        let (_dir, store, browser, account, monitors) = fixture();
        empty_catalog(&store, &account.id);
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        let mut checks = 0;
        let mut refreshes = 0;
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_, _| {
                checks += 1;
                Ok(checks == 3)
            },
            |_, _| {
                Ok(vec![DiscoveredAccount {
                    identity: Some("alice@gmail.com".into()),
                    label: Some("alice@gmail.com".into()),
                    slot: Some("0".into()),
                }])
            },
            |_, _, _| {
                refreshes += 1;
                Ok(test_catalog("fresh"))
            },
        );
        assert_eq!((checks, refreshes), (3, 1));
        assert_eq!(
            store.model_catalog_record(&account.id).unwrap().models[0].label,
            "fresh"
        );
        assert_eq!(
            super::super::provider_login_status(&store, &browser.provider_id)
                .unwrap()
                .state,
            "ready"
        );
    }

    #[test]
    fn login_skips_automatic_refresh_when_models_are_already_cached() {
        let (_dir, store, browser, account, monitors) = fixture();
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        let mut refreshes = 0;
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_, _| Ok(true),
            |_, _| Ok(vec![]),
            |_, _, _| {
                refreshes += 1;
                Ok(test_catalog("fresh"))
            },
        );
        assert_eq!(refreshes, 0);
        assert_eq!(
            store
                .get::<Account>("local", "account", &account.id)
                .unwrap()
                .state,
            "ready"
        );
        assert_eq!(
            store.model_catalog_record(&account.id).unwrap().models[0].label,
            "cached"
        );
        assert_eq!(
            super::super::provider_login_status(&store, &browser.provider_id)
                .unwrap()
                .state,
            "ready"
        );
    }

    #[test]
    fn signed_in_without_model_controls_starts_refresh_and_reports_preparation_failure() {
        let (_dir, store, browser, account, monitors) = fixture();
        empty_catalog(&store, &account.id);
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        let page = json!({"url":"https://gemini.google.com/app", "signed_in":true,"editor":true,"models":false,"verification":false});
        let mut refreshes = 0;
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_, _| Ok(crate::provider::gemini::authenticated(&page)),
            |_, _| Ok(vec![]),
            |_, _, _| {
                refreshes += 1;
                assert_eq!(
                    super::super::provider_login_status(&store, &browser.provider_id)
                        .unwrap()
                        .state,
                    "refreshing"
                );
                Err(crate::provider::gemini::model_discovery_error(
                    "deadline exceeded".into(),
                    "prepare",
                    Instant::now(),
                ))
            },
        );
        assert_eq!(refreshes, 1);
        let status = super::super::provider_login_status(&store, &browser.provider_id).unwrap();
        assert_eq!(status.state, "failed");
        assert!(status.error.contains("DeadlineExceeded at prepare"));
        assert!(status
            .error
            .contains("editor/model controls were not ready within 120 seconds"));
        assert!(store
            .model_catalog_record(&account.id)
            .unwrap()
            .models
            .is_empty());
    }

    #[test]
    fn cancelled_or_superseded_refresh_cannot_replace_the_catalog() {
        let (_dir, store, browser, account, monitors) = fixture();
        empty_catalog(&store, &account.id);
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_, _| Ok(true),
            |_, _| Ok(vec![]),
            |_, _, cancel| {
                monitors.begin(&store, &browser.provider_id).unwrap();
                assert!(cancel.is_cancelled());
                Ok(test_catalog("obsolete"))
            },
        );
        assert!(store
            .model_catalog_record(&account.id)
            .unwrap()
            .models
            .is_empty());
        assert_eq!(
            super::super::provider_login_status(&store, &browser.provider_id)
                .unwrap()
                .state,
            "waiting"
        );
        monitors.cancel(&store, &browser.provider_id).unwrap();
        assert!(super::super::provider_login_status(&store, &browser.provider_id).is_none());
    }

    #[test]
    fn refresh_failure_and_login_timeout_keep_the_existing_catalog() {
        let (_dir, store, browser, account, monitors) = fixture();
        empty_catalog(&store, &account.id);
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_, _| Ok(true),
            |_, _| Ok(vec![]),
            |_, _, _| {
                Err(ServiceError::new(
                    ErrorCode::UiContractChanged,
                    "models",
                    "UI contract changed: empty thinking menu.",
                ))
            },
        );
        let status = super::super::provider_login_status(&store, &browser.provider_id).unwrap();
        assert_eq!(status.state, "failed");
        assert_eq!(status.error, "Automatic model refresh failed (UiContractChanged at models): UI contract changed: empty thinking menu. Retry with /model refresh.");
        assert!(store
            .model_catalog_record(&account.id)
            .unwrap()
            .models
            .is_empty());
        store
            .put(&account.id, "catalog", "models", &test_catalog("cached"))
            .unwrap();
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::ZERO,
            Duration::ZERO,
            |_, _| panic!("expired watcher must not inspect Chrome"),
            |_, _| panic!("expired watcher must not discover"),
            |_, _, _| panic!("expired watcher must not refresh"),
        );
        assert_eq!(
            super::super::provider_login_status(&store, &browser.provider_id)
                .unwrap()
                .state,
            "timed_out"
        );
    }

    #[test]
    fn automatic_failure_status_does_not_copy_non_ui_error_details() {
        for (code, message) in [
            (
                ErrorCode::TransportDisconnected,
                "https://person@example.invalid/?token=private-token",
            ),
            (ErrorCode::StorageFailure, "/private/account/path"),
            (
                ErrorCode::AuthRequired,
                "Sign in for account person@example.invalid",
            ),
        ] {
            let error = ServiceError::new(code, "models", message);
            let status = refresh_failure(&error);
            assert!(!status.contains(message));
            assert!(!status.contains("person@"));
            assert!(!status.contains("private"));
            assert!(status.ends_with("Retry with /model refresh."));
        }
    }

    #[test]
    fn automatic_failure_status_preserves_bounded_ui_diagnostics_without_controls() {
        let error = ServiceError::new(
            ErrorCode::UiContractChanged,
            "models",
            "New model menu diagnostic\r\n\u{1b}.",
        );
        let status = refresh_failure(&error);
        assert!(status.contains(": New model menu diagnostic. Retry"));
        assert!(!status.chars().any(char::is_control));
        assert!(!status.contains(".."));

        let long = ServiceError::new(
            ErrorCode::UiContractChanged,
            "models",
            "\u{20ac}".repeat(1024),
        );
        let status = refresh_failure(&long);
        assert_eq!(status.chars().filter(|c| *c == '\u{20ac}').count(), 512);
        assert!(status.contains("…. Retry with /model refresh."));

        let deadline = ServiceError::new(
            ErrorCode::DeadlineExceeded,
            "prepare",
            "Browser editor/model controls were not ready within 120 seconds: deadline exceeded",
        );
        let status = refresh_failure(&deadline);
        assert!(status.contains("DeadlineExceeded at prepare"));
        assert!(status.contains("editor/model controls were not ready within 120 seconds"));
    }

    #[test]
    fn shutdown_cancels_waiting_work_without_inspecting_or_refreshing() {
        let (_dir, store, browser, _account, monitors) = fixture();
        let watch = monitors.begin(&store, &browser.provider_id).unwrap();
        monitors.cancel_all(&store);
        monitors.run_with(
            &store,
            &browser,
            true,
            &watch,
            Arc::new(BrowserGate::default()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_, _| panic!("cancelled watcher must not inspect Chrome"),
            |_, _| panic!("cancelled watcher must not discover"),
            |_, _, _| panic!("cancelled watcher must not refresh"),
        );
        assert!(super::super::provider_login_status(&store, &browser.provider_id).is_none());
        assert!(monitors.begin(&store, &browser.provider_id).is_err());
    }

    #[test]
    fn shutdown_joins_login_and_nested_workers_before_returning() {
        let (_dir, store, browser, _account, monitors) = fixture();
        let worker_exited = Arc::new(AtomicBool::new(false));
        let nested_exited = Arc::new(AtomicBool::new(false));
        let finished = worker_exited.clone();
        let nested_finished = nested_exited.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        monitors
            .start_with(
                store.clone(),
                browser.clone(),
                true,
                Arc::new(BrowserGate::default()),
                move |_, cancel| {
                    let inner_cancel = cancel.clone();
                    let inner_finished = nested_finished.clone();
                    let inner = std::thread::spawn(move || {
                        inner_cancel.sleep_or_cancelled(Duration::from_secs(5));
                        inner_finished.store(true, Ordering::SeqCst);
                    });
                    started_tx.send(()).unwrap();
                    cancel.sleep_or_cancelled(Duration::from_secs(5));
                    inner.join().unwrap();
                    cancelled_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    finished.store(true, Ordering::SeqCst);
                    Ok(false)
                },
                |_, _| panic!("cancelled login must not discover"),
                |_, _, _| panic!("cancelled login must not refresh"),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let shutdown_monitors = monitors.clone();
        let shutdown_store = store.clone();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            shutdown_monitors.cancel_all(&shutdown_store);
            stopped_tx.send(()).unwrap();
        });
        cancelled_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            stopped_rx.recv_timeout(Duration::from_millis(25)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        shutdown.join().unwrap();
        assert!(worker_exited.load(Ordering::SeqCst));
        assert!(nested_exited.load(Ordering::SeqCst));
        assert!(monitors.workers.lock().unwrap().is_empty());
        assert!(monitors
            .start_with(
                store.clone(),
                browser,
                true,
                Arc::new(BrowserGate::default()),
                |_, _| panic!("shutdown must prevent new workers"),
                |_, _| unreachable!(),
                |_, _, _| unreachable!()
            )
            .is_err());
        assert!(monitors.workers.lock().unwrap().is_empty());
    }

    #[test]
    fn a_new_login_reaps_completed_worker_handles() {
        let (_dir, store, browser, _account, monitors) = fixture();
        monitors
            .start_with(
                store.clone(),
                browser.clone(),
                true,
                Arc::new(BrowserGate::default()),
                |_, _| Ok(true),
                |_, _| Ok(vec![]),
                |_, _, _| Ok(ModelCatalogRecord::default()),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !monitors.workers.lock().unwrap()[0].is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        monitors
            .start_with(
                store.clone(),
                browser,
                true,
                Arc::new(BrowserGate::default()),
                |_, _| Ok(false),
                |_, _| unreachable!(),
                |_, _, _| unreachable!(),
            )
            .unwrap();
        assert_eq!(monitors.workers.lock().unwrap().len(), 1);
        monitors.cancel_all(&store);
    }

    #[test]
    fn restart_reports_interrupted_monitor_without_starting_work() {
        let (_dir, store, browser, account, monitors) = fixture();
        monitors.begin(&store, &browser.provider_id).unwrap();
        Monitors::recover(&store).unwrap();
        assert_eq!(
            super::super::provider_login_status(&store, &browser.provider_id)
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            store.model_catalog_record(&account.id).unwrap().models[0].label,
            "cached"
        );
    }
}
