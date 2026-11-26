use anticrumb::{
    agent::CancelToken,
    domain::*,
    rpc::{
        client::{Connection, RpcClient},
        codec,
        daemon::{certificate_fingerprint, Options},
        host::Host,
        pb, relay, server,
    },
    service::{Backend, Emit, Service},
    store::Store,
};
use futures_util::StreamExt;
use prost::Message as _;
use serde_json::json;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Echo {
    store: Store,
    requests: Mutex<Vec<TurnRequest>>,
    replies: Mutex<std::collections::VecDeque<String>>,
    events: std::sync::atomic::AtomicUsize,
    deleted: Mutex<Vec<(String, String)>>,
    cleared: Mutex<Vec<String>>,
}
impl Backend for Echo {
    fn delete_conversation(
        &self,
        account: &Account,
        session: &Session,
        _: CancelToken,
    ) -> Result<()> {
        self.deleted
            .lock()
            .unwrap()
            .push((account.id.clone(), session.id.clone()));
        Ok(())
    }
    fn clear_web_history(
        &self,
        account: &Account,
        _: CancelToken,
        progress: &mut dyn FnMut(&str, u32) -> Result<()>,
    ) -> Result<u32> {
        self.cleared.lock().unwrap().push(account.id.clone());
        progress("https://gemini.google.com/app/fixture", 1)?;
        Ok(1)
    }
    fn generate(
        &self,
        _: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        self.requests.lock().unwrap().push(request.clone());
        if matches!(&request.messages[0].blocks[0], Block::Text { text } if text == "fail before submit")
        {
            return Err(ServiceError::invalid("synthetic preparation failure"));
        }
        emit("submitted", json!({}))?;
        if matches!(&request.messages[0].blocks[0], Block::Text { text } if text == "challenge") {
            return Err(ServiceError::new(
                ErrorCode::VerificationRequired,
                "submit",
                "verification required",
            ));
        }
        if matches!(&request.messages[0].blocks[0], Block::Text { text } if text == "wait for cancellation")
        {
            if cancel.sleep_or_cancelled(Duration::from_secs(5)) {
                return Err(ServiceError::new(
                    ErrorCode::Cancelled,
                    "fixture",
                    "cancelled",
                ));
            }
            return Err(ServiceError::new(
                ErrorCode::DeadlineExceeded,
                "fixture",
                "cancellation did not arrive",
            ));
        }
        for i in 0..self.events.load(std::sync::atomic::Ordering::Relaxed) {
            if cancel.is_cancelled() {
                return Err(ServiceError::new(
                    ErrorCode::Cancelled,
                    "fixture",
                    "cancelled",
                ));
            }
            emit("text_delta", json!({"text":format!("{i} ")}))?;
        }
        let image = self.store.artifact(
            &request.account_id,
            "result.png",
            "image/png",
            b"fixture image",
            "generated",
        )?;
        session.remote_turns += 1;
        Ok(TurnResult {
            session_id: session.id.clone(),
            text: self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "complete".into()),
            tool_calls: vec![],
            artifacts: vec![image.id],
            model: request.model.clone(),
            elapsed_ms: 1,
        })
    }
    fn close(&self, _: &Account, _: &Session) -> Result<()> {
        Ok(())
    }
    fn media(&self, account: &Account, _: &Session) -> Result<Vec<String>> {
        Ok(vec![
            self.store
                .artifact(
                    &account.id,
                    "result.png",
                    "image/png",
                    b"fixture image",
                    "generated",
                )?
                .id,
        ])
    }
}
fn fixture(root: &Path) -> (Arc<Host>, Arc<Echo>) {
    let store = Store::open(root).unwrap();
    store
        .put(
            "local",
            "account",
            "default",
            &Account {
                id: "default".into(),
                provider: default_provider(),
                state: "ready".into(),
                profile: root.join("profile"),
                subprofile: None,
                port: 9222,
                headless: true,
            },
        )
        .unwrap();
    let echo = Arc::new(Echo {
        store: store.clone(),
        requests: Mutex::new(vec![]),
        replies: Mutex::default(),
        events: 1.into(),
        deleted: Mutex::new(Vec::new()),
        cleared: Mutex::new(Vec::new()),
    });
    let host = Host::with_service(
        Service::new(store, echo.clone()).unwrap(),
        None,
        "browser",
        None,
    )
    .unwrap();
    (host, echo)
}
async fn network(
    host: Arc<Host>,
    tls: Option<server::TlsIdentity>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "{}://{}",
        if tls.is_some() { "https" } else { "http" },
        listener.local_addr().unwrap()
    );
    let task = tokio::spawn(async move {
        server::serve(
            host,
            Some(TOKEN.into()),
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            tls,
        )
        .await
        .unwrap();
    });
    (endpoint, task)
}
fn request() -> TurnRequest {
    serde_json::from_value(json!({"messages":[{"role":"user","blocks":[{"type":"text","text":"Use the native attachment"}]}],"retention":"retain"})).unwrap()
}

type RecordedBrowserFetch = (String, String, u32, u32, String);

#[derive(Clone, Default)]
struct UsageFixture {
    accounts: Vec<pb::Account>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    browser_fetches: Arc<Mutex<Vec<RecordedBrowserFetch>>>,
    browser_controls: Arc<Mutex<Vec<pb::BrowserControlRequest>>>,
    screenshots: Arc<Mutex<std::collections::HashMap<String, pb::BrowserScreenshot>>>,
}
#[tonic::async_trait]
impl pb::account_service_server::AccountService for UsageFixture {
    async fn clear_web_history(
        &self,
        _: tonic::Request<pb::AccountRef>,
    ) -> std::result::Result<tonic::Response<pb::WebHistoryCleanupStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn get_web_history_cleanup(
        &self,
        _: tonic::Request<pb::AccountRef>,
    ) -> std::result::Result<tonic::Response<pb::WebHistoryCleanupStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn list_accounts(
        &self,
        _: tonic::Request<pb::Empty>,
    ) -> std::result::Result<tonic::Response<pb::AccountList>, tonic::Status> {
        Ok(tonic::Response::new(pb::AccountList {
            accounts: self.accounts.clone(),
        }))
    }
    async fn create_account(
        &self,
        _: tonic::Request<pb::CreateAccountRequest>,
    ) -> std::result::Result<tonic::Response<pb::Account>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn attach_browser(
        &self,
        _: tonic::Request<pb::AttachBrowserRequest>,
    ) -> std::result::Result<tonic::Response<pb::BrowserStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn open_login(
        &self,
        _: tonic::Request<pb::AccountRef>,
    ) -> std::result::Result<tonic::Response<pb::Empty>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn set_browser_mode(
        &self,
        _: tonic::Request<pb::SetBrowserModeRequest>,
    ) -> std::result::Result<tonic::Response<pb::BrowserStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn pause_account(
        &self,
        _: tonic::Request<pb::AccountRef>,
    ) -> std::result::Result<tonic::Response<pb::Account>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn get_browser_status(
        &self,
        _: tonic::Request<pb::AccountRef>,
    ) -> std::result::Result<tonic::Response<pb::BrowserStatus>, tonic::Status> {
        Err(tonic::Status::unimplemented("usage fixture"))
    }
    async fn get_web_usage(
        &self,
        request: tonic::Request<pb::AccountRef>,
    ) -> std::result::Result<tonic::Response<pb::WebUsage>, tonic::Status> {
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            format!("Bearer {TOKEN}").as_str()
        );
        let timeout = request
            .metadata()
            .get("grpc-timeout")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let id = request.into_inner().account_id;
        self.requests.lock().unwrap().push((id.clone(), timeout));
        Ok(tonic::Response::new(pb::WebUsage {
            account_id: id,
            server_id: "quota-browser-fixture".into(),
            checked_at: 123,
            plan: Some("Fixture plan".into()),
            limits: vec![pb::WebUsageLimit {
                label: "Weekly limit".into(),
                used: Some("0% used".into()),
                used_percent: Some(0.0),
                reset: Some("Resets next week".into()),
                ..Default::default()
            }],
            notice: "Exact message and token counts are not shown.".into(),
            updated_label: "Updated just now".into(),
            timezone: "Etc/UTC".into(),
        }))
    }
    async fn browser_fetch(
        &self,
        request: tonic::Request<pb::BrowserFetchRequest>,
    ) -> std::result::Result<tonic::Response<pb::BrowserFetchResponse>, tonic::Status> {
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            format!("Bearer {TOKEN}").as_str()
        );
        let timeout = request
            .metadata()
            .get("grpc-timeout")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let inner = request.into_inner();
        self.browser_fetches.lock().unwrap().push((
            inner.account_id,
            inner.url.clone(),
            inner.wait_ms,
            inner.max_chars,
            timeout,
        ));
        Ok(tonic::Response::new(pb::BrowserFetchResponse {
            url: inner.url,
            title: "Remote Chrome Page".into(),
            text: "Rendered via remote daemon".into(),
        }))
    }
    async fn browser_control(
        &self,
        request: tonic::Request<pb::BrowserControlRequest>,
    ) -> std::result::Result<tonic::Response<pb::BrowserControlResponse>, tonic::Status> {
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            format!("Bearer {TOKEN}").as_str()
        );
        let inner = request.into_inner();
        self.browser_controls.lock().unwrap().push(inner.clone());
        let (url, screenshot) = match inner.command.unwrap() {
            pb::browser_control_request::Command::Open(open) => (open.url, None),
            pb::browser_control_request::Command::Screenshot(_)
            | pb::browser_control_request::Command::Type(_) => {
                let account = if inner.account_id.is_empty() {
                    "default"
                } else {
                    &inner.account_id
                };
                (
                    "https://example.com/reserve".into(),
                    Some(self.screenshots.lock().unwrap()[account].clone()),
                )
            }
            _ => panic!("unexpected browser command"),
        };
        Ok(tonic::Response::new(pb::BrowserControlResponse {
            session_id: inner.session_id,
            active_tab_id: "tab_remote_1".into(),
            page: Some(pb::BrowserSnapshot {
                tab_id: "tab_remote_1".into(),
                url,
                title: "Remote Controlled Page".into(),
                screenshot,
                elements: vec![pb::BrowserControlElement {
                    ref_id: "fresh-ref".into(),
                    role: "button".into(),
                    label: "Book".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_usage_preserves_optional_values_across_projection_ffi_and_remote_host() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let fixture_api = UsageFixture {
        requests: observed.clone(),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::account_service_server::AccountServiceServer::new(
                fixture_api,
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let remote = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (local, echo) = fixture(directory.path());
    let store = local.service.store.clone();
    let before = store.model_catalog("default").unwrap();
    let workspace =
        Host::with_service(local.service.clone(), Some(remote), "workspace", None).unwrap();
    let client = RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();
    let value = anticrumb::rpc::workspace_client::presentation(
        &client,
        "GET",
        "/api/accounts/default/usage",
        None,
    )
    .await
    .unwrap();
    let usage: pb::WebUsage = serde_json::from_value(value).unwrap();
    assert_eq!(usage.account_id, "default");
    assert_eq!(usage.server_id, "quota-browser-fixture");
    assert_ne!(usage.server_id, workspace.server_id);
    assert_eq!(usage.plan.as_deref(), Some("Fixture plan"));
    assert_eq!(usage.checked_at, 123);
    assert_eq!(usage.limits[0].used_percent, Some(0.0));
    assert!(usage.limits[0].remaining.is_none());
    assert!(usage.limits[0].limit.is_none());
    assert_eq!(usage.limits[0].reset.as_deref(), Some("Resets next week"));
    assert_eq!(usage.timezone, "Etc/UTC");
    assert_eq!(store.model_catalog("default").unwrap(), before);
    assert_eq!(store.usage().unwrap()["submitted_chars"], 0);
    assert!(echo.requests.lock().unwrap().is_empty());
    assert_eq!(
        *observed.lock().unwrap(),
        vec![("default".into(), "150000m".into())]
    );
    drop(client);
    workspace.shutdown();
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_usage_respects_account_gate_without_changing_login_or_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let (host, echo) = fixture(directory.path());
    let store = &host.service.store;
    let login = json!({"state":"refreshing","error":""});
    store.put("default", "login", "current", &login).unwrap();
    let catalog = store.model_catalog("default").unwrap();
    let permit = host
        .service
        .account_gate("default")
        .acquire_owned()
        .await
        .unwrap();
    let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current()).unwrap();
    let error = client
        .get_web_usage(pb::AccountRef {
            account_id: "default".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::QueueFull);
    assert!(error.message.contains("/usage"));
    assert_eq!(store.model_catalog("default").unwrap(), catalog);
    assert_eq!(
        store
            .get::<serde_json::Value>("default", "login", "current")
            .unwrap(),
        login
    );
    assert!(echo.requests.lock().unwrap().is_empty());
    drop(permit);
    let error = client
        .get_web_usage(pb::AccountRef {
            account_id: "missing-account".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    drop(client);
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_usage_runs_concurrently_with_active_turn_while_blocking_login_and_cleanup() {
    use anticrumb::provider::{
        Provider, ProviderCapabilities, ProviderContext, ProviderInfo, ProviderRegistry,
    };

    struct UsageFixtureProvider;
    impl Provider for UsageFixtureProvider {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: "usage-fixture".into(),
                name: "Usage Fixture".into(),
                version: "v1".into(),
                kind: "web_provider".into(),
                entry_url: None,
                default_model: "fixture".into(),
                source: "test".into(),
                inline_prompt_chars: anticrumb::provider::DEFAULT_INLINE_PROMPT_CHARS,
                capabilities: ProviderCapabilities {
                    web_usage: true,
                    ..Default::default()
                },
            }
        }
        fn discover_models(
            &self,
            _: &ProviderContext<'_>,
            _: &Account,
            _: CancelToken,
        ) -> Result<ModelCatalogRecord> {
            Ok(ModelCatalogRecord::default())
        }
        fn generate(
            &self,
            _: &ProviderContext<'_>,
            _: &Account,
            session: &mut Session,
            _: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            Ok(TurnResult {
                session_id: session.id.clone(),
                text: "ok".into(),
                tool_calls: vec![],
                artifacts: vec![],
                model: None,
                elapsed_ms: 1,
            })
        }
        fn close_session(&self, _: &ProviderContext<'_>, _: &Account, _: &Session) -> Result<()> {
            Ok(())
        }
        fn read_usage(
            &self,
            _: &ProviderContext<'_>,
            account: &Account,
            _: CancelToken,
        ) -> Result<WebUsageReport> {
            Ok(WebUsageReport {
                account_id: account.id.clone(),
                checked_at: 42,
                ..Default::default()
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .put(
            "local",
            "account",
            "default",
            &Account {
                id: "default".into(),
                provider: "usage-fixture".into(),
                state: "ready".into(),
                profile: directory.path().join("profile"),
                subprofile: None,
                port: 9222,
                headless: true,
            },
        )
        .unwrap();
    let providers = ProviderRegistry::with_builtins();
    providers.register(Arc::new(UsageFixtureProvider)).unwrap();
    let echo = Arc::new(Echo {
        store: store.clone(),
        requests: Mutex::new(vec![]),
        replies: Mutex::default(),
        events: 1.into(),
        deleted: Mutex::new(Vec::new()),
        cleared: Mutex::new(Vec::new()),
    });
    let host = Host::with_service_and_providers(
        Service::new(store.clone(), echo).unwrap(),
        None,
        "browser",
        None,
        providers,
    )
    .unwrap();
    let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current()).unwrap();

    // 1. Holding account_gate (simulating an ongoing conversation turn) must NOT block GetWebUsage.
    let turn_permit = host
        .service
        .account_gate("default")
        .acquire_owned()
        .await
        .unwrap();
    let report = client
        .get_web_usage(pb::AccountRef {
            account_id: "default".into(),
        })
        .await
        .expect("usage query must run concurrently while a turn holds account_gate");
    assert_eq!(report.account_id, "default");
    assert_eq!(report.checked_at, 42);
    drop(turn_permit);

    // 2. Active bulk history cleanup MUST still reject GetWebUsage with QueueFull.
    store
        .put(
            "local",
            "web_history_cleanup",
            "default",
            &WebHistoryCleanupState {
                account_id: "default".into(),
                state: "running".into(),
                deleted: 1,
                error: String::new(),
                started_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
    let err = client
        .get_web_usage(pb::AccountRef {
            account_id: "default".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::QueueFull);

    drop(client);
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_fetch_relays_across_ffi_and_remote_host_and_enforces_account_gate() {
    let fetches = Arc::new(Mutex::new(Vec::new()));
    let fixture_api = UsageFixture {
        browser_fetches: fetches.clone(),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::account_service_server::AccountServiceServer::new(
                fixture_api,
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let remote = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (local, echo) = fixture(directory.path());
    let workspace =
        Host::with_service(local.service.clone(), Some(remote), "workspace", None).unwrap();
    let client = RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();

    let ws_client =
        anticrumb::rpc::workspace_client::Client::for_workspace(directory.path()).unwrap();
    let cancel = CancelToken::new();
    let resp = ws_client
        .browser_fetch("default", "https://example.com/app", 750, 4096, &cancel)
        .unwrap();
    assert_eq!(resp.url, "https://example.com/app");
    assert_eq!(resp.title, "Remote Chrome Page");
    assert_eq!(resp.text, "Rendered via remote daemon");
    assert!(echo.requests.lock().unwrap().is_empty());
    assert_eq!(
        *fetches.lock().unwrap(),
        vec![(
            "default".into(),
            "https://example.com/app".into(),
            750,
            4096,
            "90000000u".into(),
        )]
    );

    let ctrl_resp = ws_client
        .browser_control(
            anticrumb::browser_control::request(
                &json!({"action":"open","url":"https://example.com/reserve","max_chars":4096}),
                "default",
                "rpc-contract",
            )
            .unwrap(),
            &cancel,
        )
        .unwrap();
    assert_eq!(ctrl_resp.session_id, "rpc-contract");
    assert_eq!(ctrl_resp.active_tab_id, "tab_remote_1");
    let page = ctrl_resp.page.unwrap();
    assert_eq!(page.title, "Remote Controlled Page");
    assert_eq!(page.url, "https://example.com/reserve");
    assert_eq!(page.elements[0].ref_id, "fresh-ref");

    drop(ws_client);
    drop(client);
    workspace.shutdown();
    stop.send(()).unwrap();
    task.await.unwrap();

    // Local browser host path is decoupled from the Gemini account gate while still enforcing
    // missing account and SSRF validation even when a Gemini turn holds the account gate.
    let local_client =
        RpcClient::embedded(local.clone(), tokio::runtime::Handle::current()).unwrap();
    let permit = local
        .service
        .account_gate("default")
        .acquire_owned()
        .await
        .unwrap();
    let err = local_client
        .browser_fetch(pb::BrowserFetchRequest {
            account_id: "default".into(),
            url: "http://127.0.0.1:9222/json/list".into(),
            wait_ms: 0,
            max_chars: 1000,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    drop(permit);

    let err = local_client
        .browser_fetch(pb::BrowserFetchRequest {
            account_id: "missing-account".into(),
            url: "https://example.com/".into(),
            wait_ms: 0,
            max_chars: 1000,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    drop(local_client);
    local.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_screenshots_cross_stores_and_reach_the_next_agent_turn() {
    use base64::Engine as _;

    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let (browser, echo) = fixture(browser_dir.path());
    let store = Store::open(local_dir.path()).unwrap();
    let account: Account = browser
        .service
        .store
        .get("local", "account", "default")
        .unwrap();
    store.put("local", "account", "default", &account).unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=")
        .unwrap();
    let fixture_api = UsageFixture {
        accounts: vec![codec::account_to(account)],
        ..Default::default()
    };
    for account_id in ["default", "work"] {
        let id = format!("shot_{account_id}");
        let artifact = browser
            .service
            .store
            .artifact(
                account_id,
                &format!("{id}.png"),
                "image/png",
                &png,
                "browser",
            )
            .unwrap();
        fixture_api.screenshots.lock().unwrap().insert(
            account_id.into(),
            pb::BrowserScreenshot {
                id,
                artifact_id: artifact.id,
                width: 1,
                height: 1,
                region: Some(pb::BrowserRegion {
                    x: 10.0,
                    y: 20.0,
                    width: 1.0,
                    height: 1.0,
                }),
                png: Vec::new(),
            },
        );
    }
    let screenshots = fixture_api.screenshots.clone();
    let controls = fixture_api.browser_controls.clone();
    let api = server::GrpcApi::new(browser.clone(), Some(TOKEN.into()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::provider_service_server::ProviderServiceServer::new(
                api.clone(),
            ))
            .add_service(pb::account_service_server::AccountServiceServer::new(
                fixture_api,
            ))
            .add_service(pb::artifact_service_server::ArtifactServiceServer::new(
                api.clone(),
            ))
            .add_service(pb::turn_service_server::TurnServiceServer::new(api.clone()))
            .add_service(pb::session_service_server::SessionServiceServer::new(api))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let remote = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    let backend = Arc::new(relay::Relay {
        client: remote.clone(),
        runtime: tokio::runtime::Handle::current(),
        store: store.clone(),
        peer: browser.server_id.clone(),
    });
    let workspace = Host::with_service(
        Service::new(store.clone(), backend).unwrap(),
        Some(remote),
        "workspace",
        None,
    )
    .unwrap();
    let client = RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();

    // An omitted account uses default; explicit accounts keep their own artifact scope.
    for requested in ["", "default", "work"] {
        let account = if requested.is_empty() {
            "default"
        } else {
            requested
        };
        let source = screenshots.lock().unwrap()[account].clone();
        assert!(store.artifact_bytes(account, &source.artifact_id).is_err());
        let response = client
            .browser_control(pb::BrowserControlRequest {
                account_id: requested.into(),
                session_id: "capture-fixture".into(),
                command: Some(pb::browser_control_request::Command::Screenshot(
                    pb::BrowserCapture::default(),
                )),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(response.session_id, "capture-fixture");
        assert_eq!(response.active_tab_id, "tab_remote_1");
        let shot = response.page.unwrap().screenshot.unwrap();
        let (artifact, bytes) = store.artifact_bytes(account, &shot.artifact_id).unwrap();
        assert_eq!(bytes, png);
        assert_eq!(artifact.mime, "image/png");
        assert_eq!(artifact.name, format!("{}.png", source.id));
        assert_ne!(shot.artifact_id, source.artifact_id);
        let mut expected = source;
        expected.artifact_id.clone_from(&shot.artifact_id);
        assert_eq!(
            shot, expected,
            "capture coordinates and screenshot identity must survive relay"
        );
        assert!(store
            .artifact_bytes("unrelated", &shot.artifact_id)
            .is_err());
    }

    // Exercise the actual agent bridge, including screenshots from a different
    // browser account and automatic captures after an action such as typing.
    let project = local_dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    for account_id in ["default", "work"] {
        echo.requests.lock().unwrap().clear();
        *echo.replies.lock().unwrap() = [
            json!({"action":"help"}),
            json!({"action":"open","account_id":account_id,"url":"https://example.com/reserve"}),
            json!({"action":"screenshot","account_id":account_id}),
            json!({"action":"type","account_id":account_id,"text":"fixture"}),
        ]
        .into_iter()
        .map(|args| {
            format!(
                "[TOOL_CALL]\n{}\n[/TOOL_CALL]",
                json!({"tool_calls":[{"tool":"browser_control","args":args}]})
            )
        })
        .chain(["complete".into()])
        .collect();
        let job = client
            .start_agent(pb::AgentRequest {
                task: "Inspect the existing page, type fixture, and inspect the new screenshot"
                    .into(),
                workspace: project.to_string_lossy().into_owned(),
                account_id: "default".into(),
                no_shell: true,
                max_steps: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let value = client
                    .get_agent(pb::AgentRef { id: job.id.clone() })
                    .await
                    .unwrap();
                if value.state != "running" {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(finished.state, "completed", "{finished:?}");
        assert_eq!(finished.result.unwrap().final_response, "complete");
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        for (step, request) in requests.iter().enumerate() {
            let images = request
                .artifacts
                .iter()
                .filter_map(|id| {
                    let file = echo.store.artifact_bytes("default", id).unwrap();
                    (file.0.mime == "image/png").then_some(file)
                })
                .collect::<Vec<_>>();
            assert_eq!(images.len(), usize::from(step >= 3), "step {step}");
            for (image, bytes) in images {
                assert_eq!(image.name, format!("shot_{account_id}.png"));
                assert_eq!(bytes, png);
            }
        }
    }

    // A failed transfer must not return an unusable ID or replay the browser action.
    let source = screenshots.lock().unwrap()["default"].clone();
    let foreign = screenshots.lock().unwrap()["work"].artifact_id.clone();
    for (case, id) in [
        ("missing", "missing-artifact".to_owned()),
        ("wrong-account", foreign),
        ("corrupt", source.artifact_id.clone()),
    ] {
        if case == "corrupt" {
            let (_, path) = echo.store.artifact_path("default", &id).unwrap();
            std::fs::write(path, b"corrupted pixels").unwrap();
        }
        screenshots
            .lock()
            .unwrap()
            .get_mut("default")
            .unwrap()
            .artifact_id = id;
        let before = controls.lock().unwrap().len();
        let saved = store.list::<Artifact>("default", "artifact").unwrap().len();
        let error = client
            .browser_control(pb::BrowserControlRequest {
                account_id: "default".into(),
                session_id: "capture-fixture".into(),
                command: Some(pb::browser_control_request::Command::Type(
                    pb::BrowserTextInput {
                        text: "fixture".into(),
                        target: None,
                    },
                )),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("browser action completed, but screenshot transfer failed"),
            "{case}: {error}"
        );
        assert!(error
            .message
            .contains("observe again before retrying the action"));
        assert_eq!(error.retry, "never");
        assert_eq!(controls.lock().unwrap().len(), before + 1);
        assert_eq!(
            store.list::<Artifact>("default", "artifact").unwrap().len(),
            saved
        );
    }

    client.close().await.unwrap();
    workspace.shutdown();
    browser.shutdown();
    stop.send(()).unwrap();
    task.await.unwrap();
}

async fn connection_status(host: &Arc<Host>) -> pb::BrowserConnection {
    let bytes = host
        .dispatch("/anticrumb.v1.PlatformService/GetStatus", Vec::new())
        .await
        .unwrap();
    pb::HostStatus::decode(bytes.as_slice())
        .unwrap()
        .browser_connection
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_rpc_previews_and_cleans_both_hosts_and_reports_partial_failure() {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let (browser, _) = fixture(browser_dir.path());
    let (local, _) = fixture(local_dir.path());
    let (endpoint, server_task) = network(browser.clone(), None).await;
    let remote = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    let workspace =
        Host::with_service(local.service.clone(), Some(remote), "workspace", None).unwrap();
    let client = RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();
    let mut expired = Vec::new();
    for store in [&browser.service.store, &workspace.service.store] {
        let mut artifact = store
            .artifact(
                "default",
                "expired.png",
                "image/png",
                b"expired pixels",
                "browser",
            )
            .unwrap();
        artifact.created_at -= 8 * 86_400_000;
        store
            .put("default", "artifact", &artifact.id, &artifact)
            .unwrap();
        expired.push(artifact.id);
    }
    let original = client
        .manage_cache(pb::CacheRequest::default())
        .await
        .unwrap();
    assert_eq!(original.stores.len(), 2);
    assert_eq!(original.stores[0].role, "workspace");
    assert_eq!(original.stores[1].role, "browser");
    for report in &original.stores {
        let stats = report.stats.as_ref().unwrap();
        assert_eq!(stats.retention_days, 7);
        assert_eq!(stats.eligible_artifacts, 1);
        assert_eq!(stats.reclaimable_bytes, 14);
        assert_eq!(stats.last_cleanup, 0);
    }
    let preview = client
        .manage_cache(pb::CacheRequest {
            prune: true,
            dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(preview, original);
    for (store, id) in [
        (&browser.service.store, &expired[0]),
        (&workspace.service.store, &expired[1]),
    ] {
        assert_eq!(
            store.artifact_bytes("default", id).unwrap().1,
            b"expired pixels"
        );
    }
    // The CLI and TUI use this same borrowed workspace client.
    let borrowed =
        anticrumb::rpc::workspace_client::Client::for_workspace(local_dir.path()).unwrap();
    let cleaned = borrowed
        .cache(anticrumb::store::cache::Options {
            prune: true,
            ..Default::default()
        })
        .unwrap();
    for report in cleaned.stores {
        let stats = report.stats.unwrap();
        assert!(stats.pruned && stats.last_cleanup > 0);
        assert_eq!(stats.reclaimed_bytes, 14);
        assert_eq!(stats.blob_bytes, 0);
    }
    assert!(browser
        .service
        .store
        .artifact_path("default", &expired[0])
        .is_err());
    assert!(workspace
        .service
        .store
        .artifact_path("default", &expired[1])
        .is_err());
    let unauthorized = RpcClient::network(&endpoint, "incorrect-token", None, None)
        .await
        .unwrap();
    let disconnected =
        Host::with_service(local.service.clone(), Some(unauthorized), "workspace", None).unwrap();
    let bytes = disconnected
        .dispatch(
            "/anticrumb.v1.PlatformService/ManageCache",
            pb::CacheRequest {
                prune: true,
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .await
        .unwrap();
    let partial = pb::CacheReport::decode(bytes.as_slice()).unwrap();
    assert!(partial.stores[0].stats.as_ref().unwrap().pruned);
    assert!(!partial.stores[1].error.is_empty());
    assert!(partial.stores[1].stats.is_none());
    assert!(client
        .manage_cache(pb::CacheRequest {
            days: 3651,
            prune: true,
            ..Default::default()
        })
        .await
        .is_err());
    drop(borrowed);
    client.close().await.unwrap();
    disconnected.shutdown();
    browser.shutdown();
    server_task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owned_host_automatically_cleans_expired_unreferenced_files_on_startup() {
    let directory = tempfile::tempdir().unwrap();
    let (fixture, _) = fixture(directory.path());
    let store = &fixture.service.store;
    let mut image = store
        .artifact("default", "old.png", "image/png", b"old image", "browser")
        .unwrap();
    image.created_at -= 8 * 86_400_000;
    store.put("default", "artifact", &image.id, &image).unwrap();
    let host = Host::with_service(
        fixture.service.clone(),
        None,
        "browser",
        Some(Host::lease(directory.path()).unwrap()),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.artifact_path("default", &image.id).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        store
            .get::<serde_json::Value>("local", "maintenance", "cache")
            .unwrap()["last_cleanup"]
            .as_i64()
            .unwrap()
            > 0
    );
    host.shutdown();
}

#[tokio::test]
async fn every_network_method_authenticates_before_work() {
    let root = tempfile::tempdir().unwrap();
    let (host, _) = fixture(root.path());
    let (endpoint, task) = network(host.clone(), None).await;
    let channel = tonic::transport::Endpoint::from_shared(endpoint.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    // Exercise every declared browser RPC, including the streaming method.
    let mut service = "";
    let mut count = 0;
    for line in include_str!("../proto/anticrumb.proto")
        .lines()
        .map(str::trim)
    {
        if let Some(name) = line.strip_prefix("service ") {
            service = name.split_whitespace().next().unwrap();
        }
        if service == "WorkspaceService" {
            continue;
        }
        if let Some(method) = line.strip_prefix("rpc ") {
            let method = method.split('(').next().unwrap();
            let path: tonic::codegen::http::uri::PathAndQuery =
                format!("/anticrumb.v1.{service}/{method}").parse().unwrap();
            for token in [None, Some("incorrect")] {
                let mut grpc = tonic::client::Grpc::new(channel.clone());
                grpc.ready().await.unwrap();
                let mut request = tonic::Request::new(pb::Empty {});
                if let Some(token) = token {
                    request
                        .metadata_mut()
                        .insert("authorization", format!("Bearer {token}").parse().unwrap());
                }
                let codec = tonic::codec::ProstCodec::<pb::Empty, pb::Empty>::default();
                let error = if method == "WatchEvents" {
                    grpc.server_streaming(request, path.clone(), codec)
                        .await
                        .err()
                        .unwrap()
                } else {
                    grpc.unary(request, path.clone(), codec)
                        .await
                        .err()
                        .unwrap()
                };
                assert_eq!(error.code(), tonic::Code::Unauthenticated, "{path}");
            }
            count += 1;
        }
    }
    assert_eq!(count, 38);
    let client = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    let mode_err = client
        .set_browser_mode(pb::SetBrowserModeRequest {
            account_id: "default".into(),
            headless: true,
        })
        .await
        .unwrap_err();
    assert_eq!(mode_err.code, ErrorCode::InvalidRequest);
    let info = client.get_info(pb::Empty {}).await.unwrap();
    assert_eq!(info.role, "browser");
    assert!(!info
        .services
        .iter()
        .any(|s| s.ends_with("WorkspaceService")));
    assert!(client
        .start_agent(pb::AgentRequest::default())
        .await
        .is_err());
    host.shutdown();
    task.await.unwrap();
}

#[tokio::test]
async fn tls_requires_trust_and_matching_server_name() {
    let root = tempfile::tempdir().unwrap();
    let (host, _) = fixture(root.path());
    let options = Options {
        root: root.path().into(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        names: vec!["daemon.example".into()],
        ..Default::default()
    };
    let cert = root.path().join("tls/ca-certificate.pem");
    let leaf_cert = root.path().join("tls/certificate.pem");
    let identity = options.identity().unwrap();
    let first = std::fs::read(&cert).unwrap();
    let first_leaf = std::fs::read(&leaf_cert).unwrap();
    let der = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        std::str::from_utf8(&first)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("---"))
            .collect::<String>(),
    )
    .unwrap();
    let pin = digest(&der);
    assert_eq!(certificate_fingerprint(&first).unwrap(), pin);
    options.identity().unwrap();
    assert_eq!(first, std::fs::read(&cert).unwrap());
    assert_eq!(first_leaf, std::fs::read(&leaf_cert).unwrap());
    let (endpoint, task) = network(host.clone(), identity.clone()).await;
    assert!(RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .is_err());
    assert!(
        RpcClient::network(&endpoint, TOKEN, Some(&cert), Some("wrong.example"))
            .await
            .is_err()
    );
    let client = RpcClient::network(&endpoint, TOKEN, Some(&cert), Some("daemon.example"))
        .await
        .unwrap();
    assert_eq!(client.get_info(pb::Empty {}).await.unwrap().role, "browser");
    let pinned = RpcClient::network_pinned(&endpoint, TOKEN, None, None, Some(&pin))
        .await
        .unwrap();
    assert_eq!(pinned.get_info(pb::Empty {}).await.unwrap().role, "browser");

    // Hot-reloading a new leaf signed by the same internal CA keeps existing and
    // newly reconnecting CA-pinned clients working without interruption.
    options.rotate_leaf(identity.as_ref().unwrap()).unwrap();
    let rotated_leaf = std::fs::read(&leaf_cert).unwrap();
    assert_ne!(rotated_leaf, first_leaf);
    assert_eq!(first, std::fs::read(&cert).unwrap());
    assert_eq!(pinned.get_info(pb::Empty {}).await.unwrap().role, "browser");
    let pinned_after_rotate = RpcClient::network_pinned(
        &endpoint,
        TOKEN,
        Some(&cert),
        Some("daemon.example"),
        Some(&pin),
    )
    .await
    .unwrap();
    assert_eq!(
        pinned_after_rotate
            .get_info(pb::Empty {})
            .await
            .unwrap()
            .role,
        "browser"
    );
    assert!(
        RpcClient::network_pinned(&endpoint, TOKEN, None, None, Some(&"00".repeat(32)),)
            .await
            .is_err()
    );
    assert!(
        RpcClient::network_pinned(&endpoint, TOKEN, None, Some("wrong.example"), Some(&pin),)
            .await
            .is_err()
    );
    let wrong = RpcClient::network(&endpoint, "wrong-token", Some(&cert), None)
        .await
        .unwrap();
    assert_eq!(
        wrong.get_info(pb::Empty {}).await.unwrap_err().code,
        ErrorCode::AuthRequired
    );
    host.shutdown();
    task.await.unwrap();

    let regenerated = Options {
        regenerate_tls: true,
        ..options
    }
    .identity()
    .unwrap();
    let renewed_pin = certificate_fingerprint(&std::fs::read(&cert).unwrap()).unwrap();
    assert_ne!(renewed_pin, pin);
    let renewed_root = tempfile::tempdir().unwrap();
    let (renewed_host, _) = fixture(renewed_root.path());
    let (endpoint, task) = network(renewed_host.clone(), regenerated).await;
    assert!(
        RpcClient::network_pinned(&endpoint, TOKEN, None, None, Some(&pin))
            .await
            .is_err()
    );
    let client = RpcClient::network_pinned(&endpoint, TOKEN, None, None, Some(&renewed_pin))
        .await
        .unwrap();
    assert_eq!(client.get_info(pb::Empty {}).await.unwrap().role, "browser");
    renewed_host.shutdown();
    task.await.unwrap();
}

#[test]
fn daemon_subcommand_exposes_help_without_starting_a_host() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("server");
    let daemon_output = std::process::Command::new(env!("CARGO_BIN_EXE_anticrumb"))
        .args(["daemon", "--help"])
        .env("ANTICRUMB_HOME", root.path())
        .output()
        .unwrap();
    assert!(
        daemon_output.status.success(),
        "{}",
        String::from_utf8_lossy(&daemon_output.stderr)
    );
    let daemon_help = String::from_utf8(daemon_output.stdout).unwrap();
    assert!(daemon_help.contains("anticrumb daemon"), "{daemon_help}");
    assert!(!daemon_help.contains("--ipc"), "{daemon_help}");
    assert!(!daemon_help.contains("--listen"), "{daemon_help}");
    assert!(!daemon_help.contains("--token"), "{daemon_help}");
    assert!(!state.exists());

    let server_output = std::process::Command::new(env!("CARGO_BIN_EXE_anticrumb"))
        .args(["server", "--help"])
        .env("ANTICRUMB_HOME", root.path())
        .output()
        .unwrap();
    assert!(
        server_output.status.success(),
        "{}",
        String::from_utf8_lossy(&server_output.stderr)
    );
    let server_help = String::from_utf8(server_output.stdout).unwrap();
    assert!(server_help.contains("anticrumb server"), "{server_help}");
    assert!(server_help.contains("--token"), "{server_help}");
    assert!(server_help.contains("--state-dir"), "{server_help}");
    assert!(!server_help.contains("--token-file"), "{server_help}");
    assert!(!state.exists());
}

#[test]
fn daemon_token_and_option_policy() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        root: dir.path().into(),
        ..Default::default()
    };
    let generated = options.network_token().unwrap();
    assert_eq!(generated.len(), 64);
    assert_eq!(options.network_token().unwrap(), generated);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("token")).unwrap(),
        generated
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let explicit = Options {
        root: dir.path().into(),
        token: Some(TOKEN.into()),
        ..Default::default()
    };
    assert_eq!(explicit.network_token().unwrap(), TOKEN);
    assert_eq!(options.network_token().unwrap(), generated);
    for token in ["short".into(), "x".repeat(4097), format!("{TOKEN} ")] {
        let invalid = Options {
            token: Some(token),
            ..Default::default()
        };
        assert!(invalid.network_token().is_err());
    }
    let parse = |args: &[&str]| {
        Options::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    };
    let defaults = parse(&[]).unwrap();
    assert_eq!(defaults.listen, "0.0.0.0:0");
    assert!(defaults.self_signed);
    assert!(defaults.token.is_none());
    assert!(parse(&["127.0.0.1:0"]).unwrap().self_signed);
    for args in [
        vec!["--ipc", "--token", TOKEN],
        vec!["--tls-cert", "cert"],
        vec!["--token-file", "file"],
        vec!["--insecure-http", "127.0.0.1:0"],
        vec!["--token", TOKEN, "--insecure-http", "0.0.0.0:50051"],
        vec!["--ipc", "--insecure-http"],
        vec![
            "--tls-self-signed",
            "--tls-cert",
            "cert",
            "--tls-key",
            "key",
        ],
    ] {
        assert!(parse(&args).is_err());
    }
}

async fn artifact_contract(client: &RpcClient) {
    let info = client.get_info(pb::Empty {}).await.unwrap();
    assert_eq!(info.max_artifact_bytes, MAX_ARTIFACT_BYTES as u32);

    let data = b"private bytes that must never be expanded into a prompt".to_vec();
    let a = client
        .put_artifact_bytes("default", "sample file.txt", "text/plain", data.clone())
        .await
        .unwrap();
    assert_eq!(
        client
            .put_artifact_bytes("default", "sample file.txt", "text/plain", data.clone())
            .await
            .unwrap()
            .id,
        a.id
    );
    let downloaded = client
        .get_artifact_bytes(pb::ArtifactRef {
            account_id: "default".into(),
            id: a.id,
        })
        .await
        .unwrap();
    assert_eq!(downloaded.data, data);

    // Zero-byte and multi-chunk (> 256 KiB) artifacts stream identically.
    let empty = client
        .put_artifact_bytes(
            "default",
            "empty.bin",
            "application/octet-stream",
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(empty.size, 0);
    let downloaded_empty = client
        .get_artifact_bytes(pb::ArtifactRef {
            account_id: "default".into(),
            id: empty.id,
        })
        .await
        .unwrap();
    assert!(downloaded_empty.data.is_empty());

    let multi_chunk: Vec<u8> = (0..(600 * 1024)).map(|i| (i % 251) as u8).collect();
    let chunked = client
        .put_artifact_bytes(
            "default",
            "chunked.bin",
            "application/octet-stream",
            multi_chunk.clone(),
        )
        .await
        .unwrap();
    assert_eq!(chunked.size, multi_chunk.len() as u64);
    let downloaded_chunked = client
        .get_artifact_bytes(pb::ArtifactRef {
            account_id: "default".into(),
            id: chunked.id,
        })
        .await
        .unwrap();
    assert_eq!(downloaded_chunked.data, multi_chunk);

    for name in ["", ".", "../secret", "C:\\secret.txt", "..", "a/b", "a\\b"] {
        let chunk = pb::PutArtifactChunk {
            payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                account_id: "default".into(),
                name: name.into(),
                mime: "text/plain".into(),
                sha256: digest(&data),
                size: data.len() as u64,
            })),
        };
        assert_eq!(
            client
                .put_artifact_stream(futures_util::stream::iter([chunk]))
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    let bad_sha = pb::PutArtifactChunk {
        payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
            account_id: "default".into(),
            name: "sample.txt".into(),
            mime: "text/plain".into(),
            sha256: "bad".into(),
            size: data.len() as u64,
        })),
    };
    assert_eq!(
        client
            .put_artifact_stream(futures_util::stream::iter([bad_sha]))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    let too_large = pb::PutArtifactChunk {
        payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
            account_id: "default".into(),
            name: "too-big.bin".into(),
            mime: "application/octet-stream".into(),
            sha256: "00".repeat(32),
            size: MAX_ARTIFACT_BYTES + 1,
        })),
    };
    assert_eq!(
        client
            .put_artifact_stream(futures_util::stream::iter([too_large]))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    // Malformed chunk sequences are rejected.
    let malformed_cases: Vec<Vec<pb::PutArtifactChunk>> = vec![
        vec![],
        vec![pb::PutArtifactChunk {
            payload: Some(pb::put_artifact_chunk::Payload::Data(b"orphan".to_vec())),
        }],
        vec![
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                    account_id: "default".into(),
                    name: "dup.txt".into(),
                    mime: "text/plain".into(),
                    sha256: digest(b"hi"),
                    size: 2,
                })),
            },
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                    account_id: "default".into(),
                    name: "dup.txt".into(),
                    mime: "text/plain".into(),
                    sha256: digest(b"hi"),
                    size: 2,
                })),
            },
        ],
        vec![
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                    account_id: "default".into(),
                    name: "short.txt".into(),
                    mime: "text/plain".into(),
                    sha256: digest(b"hello"),
                    size: 5,
                })),
            },
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Data(b"he".to_vec())),
            },
        ],
        vec![
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                    account_id: "default".into(),
                    name: "long.txt".into(),
                    mime: "text/plain".into(),
                    sha256: digest(b"he"),
                    size: 2,
                })),
            },
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Data(b"hello".to_vec())),
            },
        ],
        vec![
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                    account_id: "default".into(),
                    name: "wronghash.txt".into(),
                    mime: "text/plain".into(),
                    sha256: "ff".repeat(32),
                    size: 2,
                })),
            },
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Data(b"hi".to_vec())),
            },
        ],
        vec![
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                    account_id: "default".into(),
                    name: "empty-chunk.txt".into(),
                    mime: "text/plain".into(),
                    sha256: digest(b"hi"),
                    size: 2,
                })),
            },
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Data(Vec::new())),
            },
            pb::PutArtifactChunk {
                payload: Some(pb::put_artifact_chunk::Payload::Data(b"hi".to_vec())),
            },
        ],
    ];
    for chunks in malformed_cases {
        assert_eq!(
            client
                .put_artifact_stream(futures_util::stream::iter(chunks))
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    assert_eq!(
        client
            .get_turn(pb::TurnRef {
                id: "missing".into()
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let error = match client
        .watch_events(pb::EventCursor {
            run_id: "missing".into(),
            after: 0,
        })
        .await
    {
        Err(error) => error,
        Ok(mut stream) => stream.next().await.unwrap().unwrap_err(),
    };
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn human_challenge_retains_the_tab_session_and_pauses_only_its_account() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = fixture(dir.path());
    let mut other: Account = host
        .service
        .store
        .get("local", "account", "default")
        .unwrap();
    other.id = "other".into();
    host.service
        .store
        .put("local", "account", "other", &other)
        .unwrap();
    let mut input = request();
    input.messages = vec![Message::user("challenge")];
    input.retention = SessionRetention::Ephemeral;
    let id = host.service.submit_async(input).await.unwrap();
    let run = host.service.wait(&id).await.unwrap();
    assert_eq!(run.state, "awaiting_human");
    assert_eq!(run.error.unwrap().retry, "after_human");
    let sessions = host
        .service
        .store
        .list::<Session>("local", "session")
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].state, SessionState::AwaitingHuman);
    let account: Account = host
        .service
        .store
        .get("local", "account", "default")
        .unwrap();
    assert_eq!(account.state, "awaiting_human");
    let other: Account = host.service.store.get("local", "account", "other").unwrap();
    assert_eq!(other.state, "ready");
    let events = host.service.store.events(&id, 0).unwrap();
    let human = events.iter().find(|e| e.kind == "human_required").unwrap();
    assert_eq!(human.data["submission_may_have_occurred"], true);
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streamed_events_resume_without_cancelling_generation() {
    let dir = tempfile::tempdir().unwrap();
    let (host, echo) = fixture(dir.path());
    echo.events.store(600, std::sync::atomic::Ordering::Relaxed);
    let (endpoint, task) = network(host.clone(), None).await;
    let client = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    artifact_contract(&client).await;
    let run = client
        .create_turn(codec::request_to(request()))
        .await
        .unwrap();
    let cursor = pb::EventCursor {
        run_id: run.id.clone(),
        after: 0,
    };
    let mut stream = client.watch_events(cursor.clone()).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    drop(stream);
    let mut rest = client
        .watch_events(pb::EventCursor {
            after: first.sequence,
            ..cursor
        })
        .await
        .unwrap();
    let mut previous = first.sequence;
    let mut deltas = usize::from(first.kind == "text_delta");
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), rest.next())
        .await
        .unwrap()
    {
        let event = event.unwrap();
        assert!(event.sequence > previous);
        previous = event.sequence;
        deltas += usize::from(event.kind == "text_delta");
    }
    assert_eq!(deltas, 600);
    assert_eq!(client.get_turn(run).await.unwrap().state, "completed");
    host.shutdown();
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_allocates_a_new_key_after_a_known_failed_turn() {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let (browser, echo) = fixture(browser_dir.path());
    let client = RpcClient::embedded(browser.clone(), tokio::runtime::Handle::current()).unwrap();
    let store = Store::open(local_dir.path()).unwrap();
    let account: Account = browser
        .service
        .store
        .get("local", "account", "default")
        .unwrap();
    store.put("local", "account", "default", &account).unwrap();
    let backend = Arc::new(relay::Relay {
        client: client.clone(),
        runtime: tokio::runtime::Handle::current(),
        store: store.clone(),
        peer: browser.server_id.clone(),
    });
    let workspace = Service::new(store, backend).unwrap();
    let mut request = TurnRequest::text("first");
    request.retention = SessionRetention::Retain;
    let run = workspace
        .wait(&workspace.submit(request).unwrap())
        .await
        .unwrap();
    let session = run.result.unwrap().session_id;
    for prompt in ["fail before submit", "fail before submit", "new prompt"] {
        let mut request = TurnRequest::text(prompt);
        request.retention = SessionRetention::Retain;
        request.session_id = Some(session.clone());
        let run = workspace
            .wait(&workspace.submit(request).unwrap())
            .await
            .unwrap();
        if prompt == "fail before submit" {
            assert_eq!(run.error.unwrap().message, "synthetic preparation failure");
        } else {
            assert_eq!(run.state, "completed");
        }
    }
    let keys: std::collections::HashSet<_> = echo
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.idempotency_key.clone().unwrap())
        .collect();
    // Identical retry text must run again too, rather than replaying an old failure.
    assert_eq!(keys.len(), 4);
    browser.shutdown();
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn separate_hosts_transfer_bytes_and_keep_session_identities_distinct() {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let (browser, echo) = fixture(browser_dir.path());
    let options = Options {
        root: browser_dir.path().into(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        ..Default::default()
    };
    let identity = options.identity().unwrap();
    let pin = certificate_fingerprint(
        &std::fs::read(browser_dir.path().join("tls/ca-certificate.pem")).unwrap(),
    )
    .unwrap();
    let (endpoint, task) = network(browser.clone(), identity).await;
    let workspace = relay::workspace(
        local_dir.path(),
        Connection::Grpc {
            endpoint: endpoint.clone(),
            token: TOKEN.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some(pin),
        },
    )
    .await
    .unwrap();
    let connected = connection_status(&workspace).await;
    assert_eq!(connected.transport, "grpc");
    assert_eq!(connected.endpoint, endpoint);
    assert_eq!(connected.server_id, browser.server_id);
    assert!(connected.connected);
    assert!(connected.error.is_empty());

    // Editing saved settings must not misreport the live workspace's peer.
    let unused_token = "synthetic-unused-token-32-characters";
    std::fs::write(
        local_dir.path().join("connection.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "connection": {
                "transport": "grpc",
                "endpoint": "https://other.example:50051",
                "token": unused_token,
                "ca_file": null,
                "server_name": null,
                "tls_pin": "11".repeat(32)
            }
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(connection_status(&workspace).await, connected);
    let artifact = workspace
        .service
        .store
        .artifact(
            "default",
            "native.txt",
            "text/plain",
            b"NATIVE_ONLY",
            "input",
        )
        .unwrap();
    let mut input = request();
    input.artifacts.push(artifact.id);
    let run = workspace.service.submit_async(input).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = workspace.service.snapshot(&run).unwrap();
            if let Some(error) = value.error {
                panic!("{error:?}");
            }
            if let Some(result) = value.result {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result.text, "complete");
    let seen = echo.requests.lock().unwrap()[0].clone();
    assert!(!serde_json::to_string(&seen.messages)
        .unwrap()
        .contains("NATIVE_ONLY"));
    assert_eq!(
        browser
            .service
            .store
            .artifact_bytes("default", &seen.artifacts[0])
            .unwrap()
            .1,
        b"NATIVE_ONLY"
    );
    assert_eq!(
        workspace
            .service
            .store
            .artifact_bytes("default", &result.artifacts[0])
            .unwrap()
            .1,
        b"fixture image"
    );
    let local: Session = workspace
        .service
        .store
        .get("local", "session", &result.session_id)
        .unwrap();
    assert_ne!(local.remote_session_id.as_ref().unwrap(), &local.id);
    let media = workspace.service.recover_media(&local.id).await.unwrap();
    assert_eq!(media.len(), 1);
    workspace.service.close_session(&local.id).await.unwrap();
    let remote: Session = browser
        .service
        .store
        .get(
            "local",
            "session",
            local.remote_session_id.as_ref().unwrap(),
        )
        .unwrap();
    assert_eq!(remote.state, SessionState::Closed);
    let original_peer: serde_json::Value = workspace
        .service
        .store
        .get(&local.id, "remote", "turn")
        .unwrap();
    let mut wrong_peer = original_peer.clone();
    wrong_peer["server_id"] = json!("a-different-browser-daemon");
    workspace
        .service
        .store
        .put(&local.id, "remote", "turn", &wrong_peer)
        .unwrap();
    assert!(workspace
        .service
        .delete_conversation(&local.id)
        .await
        .unwrap_err()
        .message
        .contains("different browser daemon"));
    assert!(echo.deleted.lock().unwrap().is_empty());
    workspace
        .service
        .store
        .put(&local.id, "remote", "turn", &original_peer)
        .unwrap();
    workspace
        .service
        .delete_conversation(&local.id)
        .await
        .unwrap();
    assert_eq!(
        echo.deleted.lock().unwrap().as_slice(),
        &[("default".into(), remote.id.clone())]
    );
    assert_eq!(
        workspace
            .service
            .store
            .get::<Session>("local", "session", &local.id)
            .unwrap()
            .state,
        SessionState::Deleted
    );
    assert_eq!(
        browser
            .service
            .store
            .get::<Session>("local", "session", &remote.id)
            .unwrap()
            .state,
        SessionState::Deleted
    );
    let request = pb::AccountRef {
        account_id: "default".into(),
    }
    .encode_to_vec();
    let queued = pb::WebHistoryCleanupStatus::decode(
        workspace
            .dispatch(
                "/anticrumb.v1.AccountService/ClearWebHistory",
                request.clone(),
            )
            .await
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(queued.account_id, "default");
    let completed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = pb::WebHistoryCleanupStatus::decode(
                workspace
                    .dispatch(
                        "/anticrumb.v1.AccountService/GetWebHistoryCleanup",
                        request.clone(),
                    )
                    .await
                    .unwrap()
                    .as_slice(),
            )
            .unwrap();
            if status.state == "completed" {
                break status;
            }
            assert_ne!(status.state, "failed", "{}", status.error);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed.deleted, 1);
    assert_eq!(echo.cleared.lock().unwrap().as_slice(), &["default"]);
    // A workspace that missed the bulk-status poll must reject a deleted
    // remote continuation before submitting a delta as a fresh conversation.
    let mut stale = local.clone();
    stale.id = "stale-continuation".into();
    stale.state = SessionState::Ready;
    workspace
        .service
        .store
        .put("local", "session", &stale.id, &stale)
        .unwrap();
    workspace
        .service
        .store
        .put(&stale.id, "remote", "turn", &original_peer)
        .unwrap();
    let mut continuation = TurnRequest::text("only the new delta");
    continuation.session_id = Some(stale.id.clone());
    continuation.retention = SessionRetention::Retain;
    let stale_run = workspace.service.submit_async(continuation).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(error) = workspace.service.snapshot(&stale_run).unwrap().error {
                break error;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(error.message.contains("was deleted on the browser daemon"));
    assert_eq!(
        workspace
            .service
            .store
            .get::<Session>("local", "session", &stale.id)
            .unwrap()
            .state,
        SessionState::Deleted
    );
    assert_eq!(
        echo.requests.lock().unwrap().len(),
        1,
        "a delta must never be replayed as a fresh request"
    );

    // Partial failure invalidates only this workspace's old batch scope.
    let mut old = local.clone();
    old.id = "partial-old".into();
    old.state = SessionState::Ready;
    old.last_used = 0;
    old.remote_session_id = Some("remote-still-ready".into());
    let mut newer = old.clone();
    newer.id = "new-after-batch".into();
    newer.last_used = now_ms() + 60_000;
    for session in [&old, &newer] {
        workspace
            .service
            .store
            .put("local", "session", &session.id, session)
            .unwrap();
        workspace
            .service
            .store
            .put(&session.id, "remote", "turn", &original_peer)
            .unwrap();
    }
    let mut still_ready = remote.clone();
    still_ready.id = "remote-still-ready".into();
    still_ready.state = SessionState::Ready;
    browser
        .service
        .store
        .put("local", "session", &still_ready.id, &still_ready)
        .unwrap();
    let mut partial = completed.clone();
    partial.state = "failed".into();
    partial.error = "synthetic partial failure".into();
    browser
        .service
        .store
        .put("local", "web_history_cleanup", "default", &partial)
        .unwrap();
    workspace
        .dispatch(
            "/anticrumb.v1.AccountService/GetWebHistoryCleanup",
            request.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        workspace
            .service
            .store
            .get::<Session>("local", "session", &old.id)
            .unwrap()
            .state,
        SessionState::Invalidated
    );
    assert_eq!(
        workspace
            .service
            .store
            .get::<Session>("local", "session", &newer.id)
            .unwrap()
            .state,
        SessionState::Ready
    );

    // Another workspace has no local cutoff. Synchronize exact remote deleted
    // identities, including on startup with an old completed status.
    let observer_dir = tempfile::tempdir().unwrap();
    let (observer_base, _) = fixture(observer_dir.path());
    let observer = Host::with_service(
        observer_base.service.clone(),
        workspace.remote.clone(),
        "workspace",
        None,
    )
    .unwrap();
    let mut seen_deleted = local.clone();
    seen_deleted.id = "observer-deleted".into();
    seen_deleted.state = SessionState::Ready;
    let mut seen_ready = newer.clone();
    seen_ready.id = "observer-ready".into();
    let mut other_host = seen_deleted.clone();
    other_host.id = "observer-other-host".into();
    for session in [&seen_deleted, &seen_ready, &other_host] {
        observer
            .service
            .store
            .put("local", "session", &session.id, session)
            .unwrap();
        observer
            .service
            .store
            .put(&session.id, "remote", "turn", &original_peer)
            .unwrap();
    }
    observer
        .service
        .store
        .put(&other_host.id, "remote", "turn", &wrong_peer)
        .unwrap();
    for state in ["failed", "interrupted", "completed"] {
        partial.state = state.into();
        browser
            .service
            .store
            .put("local", "web_history_cleanup", "default", &partial)
            .unwrap();
        observer
            .dispatch(
                "/anticrumb.v1.AccountService/GetWebHistoryCleanup",
                request.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            observer
                .service
                .store
                .get::<Session>("local", "session", &seen_deleted.id)
                .unwrap()
                .state,
            SessionState::Deleted
        );
        assert_eq!(
            observer
                .service
                .store
                .get::<Session>("local", "session", &seen_ready.id)
                .unwrap()
                .state,
            SessionState::Ready
        );
        assert_eq!(
            observer
                .service
                .store
                .get::<Session>("local", "session", &other_host.id)
                .unwrap()
                .state,
            SessionState::Ready
        );
    }
    // Conservative invalidation must not suppress later actual deletion.
    workspace
        .service
        .delete_conversation(&old.id)
        .await
        .unwrap();
    assert!(echo
        .deleted
        .lock()
        .unwrap()
        .iter()
        .any(|(_, session)| session == "remote-still-ready"));
    observer.shutdown();
    browser.shutdown();
    task.await.unwrap();
    let disconnected = tokio::time::timeout(Duration::from_secs(3), connection_status(&workspace))
        .await
        .expect("status must bound its probe of a stopped daemon");
    assert!(!disconnected.connected);
    assert_eq!(disconnected.transport, connected.transport);
    assert_eq!(disconnected.endpoint, connected.endpoint);
    assert_eq!(disconnected.server_id, connected.server_id);
    assert!(!disconnected.error.is_empty());
    assert!(!disconnected.error.contains(TOKEN));
    assert!(!disconnected.error.contains(unused_token));
    workspace.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_module_unary_stream_and_cancellation_use_generated_api() {
    let directory = tempfile::tempdir().unwrap();
    let (fixture_host, echo) = fixture(directory.path());
    let host = Host::with_service(
        fixture_host.service.clone(),
        None,
        "browser",
        Some(Host::lease(directory.path()).unwrap()),
    )
    .unwrap();
    drop(fixture_host);
    let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current()).unwrap();
    assert_eq!(client.get_info(pb::Empty {}).await.unwrap().role, "browser");
    assert!(client
        .get_catalog(pb::AccountRef {
            account_id: "default".into()
        })
        .await
        .unwrap()
        .models
        .is_empty());
    artifact_contract(&client).await;
    assert!(client
        .start_agent(pb::AgentRequest::default())
        .await
        .is_err());
    echo.events.store(600, std::sync::atomic::Ordering::Relaxed);
    let run = client
        .create_turn(codec::request_to(request()))
        .await
        .unwrap();
    let mut events = client
        .watch_events(pb::EventCursor {
            run_id: run.id.clone(),
            after: 0,
        })
        .await
        .unwrap();
    let mut deltas = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
    {
        if event.unwrap().kind == "text_delta" {
            deltas += 1;
        }
    }
    assert_eq!(deltas, 600);
    drop(events);
    assert_eq!(client.get_turn(run).await.unwrap().state, "completed");

    let mut waiting = request();
    waiting.messages = vec![Message::user("wait for cancellation")];
    let run = client
        .create_turn(codec::request_to(waiting))
        .await
        .unwrap();
    client.cancel_turn(run.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client.get_turn(run.clone()).await.unwrap().state == "cancelled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(Host::lease(directory.path()).is_err());
    drop(host);
    client.close().await.unwrap();
    // A closed client clone must not keep the embedded runtime's state leased.
    drop(Host::lease(directory.path()).unwrap());
    drop(client);
    // Static module teardown releases the state lease without unloading a file.
    drop(Host::lease(directory.path()).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_agent_uses_ffi_projection_and_authenticated_rpc_without_crossing_host_roles() {
    let root = tempfile::tempdir().unwrap();
    let (browser, _) = fixture(root.path());
    let workspace = Host::with_service(browser.service.clone(), None, "workspace", None).unwrap();
    let store = workspace.service.store.clone();
    for id in ["embedded", "presentation", "network", "keep", "active"] {
        store
            .put(
                "local",
                "agent",
                id,
                &json!({
                    "id":id,"state":if id == "active" {"running"} else {"completed"},
                    "request":{"task":id,"workspace":root.path(),"account_id":"default"},
                    "result":{"final_response":"saved result"}
                }),
            )
            .unwrap();
        store
            .event(id, "tool_finished", json!({"output":"saved observation"}))
            .unwrap();
    }
    let embedded =
        RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();
    embedded
        .delete_agent_conversation(pb::DeleteConversationRequest {
            id: "embedded".into(),
            workspace: None,
            account_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        embedded
            .get_agent(pb::AgentRef {
                id: "embedded".into()
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        embedded
            .read_agent_events(pb::EventCursor {
                run_id: "embedded".into(),
                after: 0
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(store.events("embedded", 0).unwrap().is_empty());
    let presentation_root = root.path().to_path_buf();
    let deleted = tokio::task::spawn_blocking(move || {
        anticrumb::rpc::workspace_client::Client::for_workspace(&presentation_root)
            .unwrap()
            .request("DELETE", "/api/conversations/presentation", None)
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(deleted["deleted"], true);
    assert!(store.events("presentation", 0).unwrap().is_empty());

    let (endpoint, server) = network(workspace.clone(), None).await;
    let unauthorized = RpcClient::network(&endpoint, "incorrect", None, None)
        .await
        .unwrap();
    assert_eq!(
        unauthorized
            .delete_agent_conversation(pb::DeleteConversationRequest {
                id: "network".into(),
                workspace: None,
                account_id: None,
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::AuthRequired
    );
    assert!(store
        .get::<serde_json::Value>("local", "agent", "network")
        .is_ok());
    assert_eq!(store.events("network", 0).unwrap().len(), 1);
    let authorized = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    authorized
        .delete_agent_conversation(pb::DeleteConversationRequest {
            id: "network".into(),
            workspace: None,
            account_id: None,
        })
        .await
        .unwrap();
    assert!(store.events("network", 0).unwrap().is_empty());
    assert!(authorized
        .delete_agent_conversation(pb::DeleteConversationRequest {
            id: "active".into(),
            workspace: None,
            account_id: None,
        })
        .await
        .is_err());
    assert!(store
        .get::<serde_json::Value>("local", "agent", "active")
        .is_ok());
    assert!(browser
        .dispatch(
            "/anticrumb.v1.WorkspaceService/DeleteAgent",
            pb::AgentRef { id: "keep".into() }.encode_to_vec()
        )
        .await
        .is_err());
    assert!(store
        .get::<serde_json::Value>("local", "agent", "keep")
        .is_ok());
    let remaining = embedded.list_agents(pb::Empty {}).await.unwrap().agents;
    assert_eq!(
        remaining
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        ["active", "keep"]
    );
    // The mutation survives a separate database connection, rather than hiding
    // a row only in the TUI or transport adapter.
    let reopened = Store::open(root.path()).unwrap();
    for id in ["embedded", "presentation", "network"] {
        assert_eq!(
            reopened
                .get::<serde_json::Value>("local", "agent", id)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert!(reopened.events(id, 0).unwrap().is_empty());
    }
    workspace.shutdown();
    server.await.unwrap();
    embedded.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_workspace_services_and_borrowed_clients_are_root_scoped() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let mut clients = Vec::new();
    let mut identities = Vec::new();
    for root in [first.path(), second.path()] {
        let (browser, echo) = fixture(root);
        echo.events.store(0, std::sync::atomic::Ordering::Relaxed);
        let workspace =
            Host::with_service(browser.service.clone(), None, "workspace", None).unwrap();
        let id = workspace.server_id.clone();
        let client = RpcClient::embedded(workspace, tokio::runtime::Handle::current()).unwrap();
        let info = client.get_info(pb::Empty {}).await.unwrap();
        assert_eq!(info.role, "workspace");
        assert!(info
            .services
            .iter()
            .any(|service| service.ends_with("WorkspaceService")));
        assert!(client
            .list_agents(pb::Empty {})
            .await
            .unwrap()
            .agents
            .is_empty());
        let borrowed_root = root.to_path_buf();
        let borrowed = tokio::task::spawn_blocking(move || {
            anticrumb::rpc::workspace_client::Client::for_workspace(&borrowed_root)
                .unwrap()
                .request("GET", "/health", None)
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(borrowed["server_id"], id);
        // Exercise the agent's blocking worker too: it must borrow this exact
        // workspace's static module instead of starting a new runtime/daemon.
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let task = format!("Complete the synthetic task for {id}");
        let job = client
            .start_agent(pb::AgentRequest {
                task: task.clone(),
                workspace: project.to_string_lossy().into_owned(),
                account_id: "default".into(),
                no_shell: true,
                max_steps: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let value = client
                    .get_agent(pb::AgentRef { id: job.id.clone() })
                    .await
                    .unwrap();
                if value.state != "running" {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(finished.state, "completed", "{finished:?}");
        assert_eq!(finished.result.unwrap().final_response, "complete");
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].artifacts.len(), 1);
        let (prompt_file, bytes) = echo
            .store
            .artifact_bytes("default", &requests[0].artifacts[0])
            .unwrap();
        let prompt = String::from_utf8(bytes).unwrap();
        assert!(prompt.contains(&task));
        assert!(prompt.contains("[TOOLS]"));
        assert!(
            matches!(&requests[0].messages[0].blocks[0], Block::Text { text }
                if text.contains(&prompt_file.name) && !text.contains(&task) && text.len() < 1_000)
        );
        identities.push(id);
        clients.push(client);
    }
    assert_ne!(identities[0], identities[1]);
    for client in clients {
        client.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_service_registers_persists_binds_accounts_and_unregisters_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let server_dir = dir.path().join("server");
    let providers_dir = dir.path().join("providers");
    let host = Host::browser_with_providers_dir(&server_dir, Some(&providers_dir)).unwrap();
    let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current()).unwrap();

    let initial = client.list_providers(pb::Empty {}).await.unwrap();
    assert_eq!(initial.providers.len(), 3);
    for id in ["gemini-web", "claude-web", "chatgpt-web"] {
        assert!(initial
            .providers
            .iter()
            .any(|p| p.id == id && p.source == "builtin"));
    }

    let registered = client
        .register_provider(pb::RegisterProviderRequest {
            id: "test-claude-web".into(),
            name: "Claude Web".into(),
            version: "v1".into(),
            kind: "web_provider".into(),
            entry_url: Some("https://claude.ai/".into()),
            allowed_origins: vec!["https://claude.ai".into()],
            default_model: Some("claude-web-default".into()),
            capabilities: Some(pb::ProviderCapabilities {
                browser_auth: true,
                temporary_sessions: false,
                web_usage: true,
                ..Default::default()
            }),
            auth_url_substrings: vec!["/login".into(), "auth.anthropic.com".into()],
            chat_url_prefixes: vec!["https://claude.ai/chat/".into()],
            reusable_path_prefixes: vec!["/new".into(), "/chat/".into()],
            usage_url: Some("https://claude.ai/settings/usage".into()),
            observer_binding: Some("anticrumbCdpChanged".into()),
            requires_models_control: false,
            inline_prompt_chars: Some(1234),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(registered.id, "test-claude-web");
    assert_eq!(registered.default_model, "claude-web-default");
    assert_eq!(registered.inline_prompt_chars, Some(1234));

    let fetched = client
        .get_provider(pb::ProviderRef {
            id: "test-claude-web".into(),
        })
        .await
        .unwrap();
    assert_eq!(fetched.name, "Claude Web");
    assert_eq!(fetched.inline_prompt_chars, Some(1234));

    // Verify zero-port CreateAccount auto-allocates a free non-zero CDP port, and explicit port also works.
    let auto_created = client
        .create_account(pb::CreateAccountRequest {
            id: "work-claude".into(),
            port: 0,
            provider: "test-claude-web".into(),
        })
        .await
        .unwrap();
    assert_eq!(auto_created.provider, "test-claude-web");
    assert!(auto_created.port >= 9222);

    let rest_created = anticrumb::rpc::workspace_client::presentation(
        &client,
        "POST",
        "/api/accounts",
        Some(&json!({"id": "work-claude-2", "provider": "test-claude-web"})),
    )
    .await
    .unwrap();
    let rest_port = rest_created["port"].as_u64().unwrap();
    assert!(rest_port >= 9222 && rest_port != u64::from(auto_created.port));

    let status = client.get_status(pb::Empty {}).await.unwrap();
    assert!(status.providers.iter().any(|p| p.id == "test-claude-web"));
    assert!(status.providers.iter().any(|p| p.id == "gemini-web"));
    assert_eq!(
        status
            .accounts
            .iter()
            .find(|a| a.id == "work-claude")
            .unwrap()
            .provider,
        "test-claude-web"
    );

    let catalog = client
        .refresh_catalog(pb::AccountRef {
            account_id: "work-claude".into(),
        })
        .await
        .unwrap();
    assert_eq!(catalog.provider, "test-claude-web");
    assert_eq!(catalog.models[0].mode_id, "claude-web-default");

    let via_rest =
        anticrumb::rpc::workspace_client::presentation(&client, "GET", "/api/providers", None)
            .await
            .unwrap();
    assert!(via_rest
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "test-claude-web"));

    client.close().await.unwrap();
    drop(host);

    // Persisted runtime manifest survives reopening the browser host with all manifest fields intact.
    let reopened = Host::browser_with_providers_dir(&server_dir, Some(&providers_dir)).unwrap();
    let stored_manifest: anticrumb::provider::ProviderManifest = reopened
        .service
        .store
        .get(
            "local",
            anticrumb::provider::registry::PROVIDER_STORE_KIND,
            "test-claude-web",
        )
        .unwrap();
    assert_eq!(stored_manifest.inline_prompt_chars, Some(1234));
    assert_eq!(
        stored_manifest.auth_url_substrings,
        vec!["/login", "auth.anthropic.com"]
    );
    assert_eq!(
        stored_manifest.chat_url_prefixes,
        vec!["https://claude.ai/chat/"]
    );
    assert_eq!(
        stored_manifest.reusable_path_prefixes,
        vec!["/new", "/chat/"]
    );
    assert_eq!(
        stored_manifest.usage_url.as_deref(),
        Some("https://claude.ai/settings/usage")
    );
    assert_eq!(
        stored_manifest.observer_binding.as_deref(),
        Some("anticrumbCdpChanged")
    );
    let reopened_client =
        RpcClient::embedded(reopened.clone(), tokio::runtime::Handle::current()).unwrap();
    let persisted = reopened_client.list_providers(pb::Empty {}).await.unwrap();
    assert!(persisted
        .providers
        .iter()
        .any(|p| p.id == "test-claude-web" && p.inline_prompt_chars == Some(1234)));

    reopened_client
        .unregister_provider(pb::ProviderRef {
            id: "test-claude-web".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        reopened_client
            .get_provider(pb::ProviderRef {
                id: "test-claude-web".into(),
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    reopened_client.close().await.unwrap();
    drop(reopened);

    // Unregistered provider stays removed across reload.
    let final_host = Host::browser_with_providers_dir(&server_dir, Some(&providers_dir)).unwrap();
    let final_client =
        RpcClient::embedded(final_host.clone(), tokio::runtime::Handle::current()).unwrap();
    let final_list = final_client.list_providers(pb::Empty {}).await.unwrap();
    assert!(!final_list
        .providers
        .iter()
        .any(|p| p.id == "test-claude-web"));
    final_client.close().await.unwrap();
}

fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::{fmt::Write as _, io::Read};

    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_streaming_artifacts_exceed_unary_limit_across_transports_and_clean_up_failures() {
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let src_50mib = temp.path().join("payload-50mib.bin");
    let target_len = 50 * 1024 * 1024usize;
    {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&src_50mib).unwrap());
        let block: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
        let mut remaining = target_len;
        while remaining > 0 {
            let chunk = remaining.min(block.len());
            out.write_all(&block[..chunk]).unwrap();
            remaining -= chunk;
        }
        out.flush().unwrap();
    }
    let expected_hash = sha256_file(&src_50mib);

    let net_dir = temp.path().join("net-host");
    std::fs::create_dir_all(&net_dir).unwrap();
    let (host, _) = fixture(&net_dir);
    let options = Options {
        root: net_dir.clone(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        ..Default::default()
    };
    let identity = options.identity().unwrap();
    let pin =
        certificate_fingerprint(&std::fs::read(net_dir.join("tls/ca-certificate.pem")).unwrap())
            .unwrap();
    let (endpoint, task) = network(host.clone(), identity).await;
    let net_client = RpcClient::network_pinned(&endpoint, TOKEN, None, None, Some(&pin))
        .await
        .unwrap();

    // 50 MiB (> 48 MiB unary MAX_MESSAGE) streams cleanly over TLS gRPC.
    let uploaded_net = net_client
        .put_artifact_file(
            "default",
            &src_50mib,
            "payload-50mib.bin",
            "application/octet-stream",
        )
        .await
        .unwrap();
    assert_eq!(uploaded_net.size, target_len as u64);
    assert_eq!(uploaded_net.hash, expected_hash);

    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&download_dir).unwrap();
    let existing = download_dir.join("existing.bin");
    std::fs::write(&existing, b"keep-me").unwrap();
    let overwrite_err = net_client
        .get_artifact_to_file(
            pb::ArtifactRef {
                account_id: "default".into(),
                id: uploaded_net.id.clone(),
            },
            &existing,
        )
        .await
        .unwrap_err();
    assert_eq!(overwrite_err.code, ErrorCode::StorageFailure);
    assert_eq!(std::fs::read(&existing).unwrap(), b"keep-me");

    let downloaded_net = download_dir.join("net-50mib.bin");
    let got_net = net_client
        .get_artifact_to_file(
            pb::ArtifactRef {
                account_id: "default".into(),
                id: uploaded_net.id.clone(),
            },
            &downloaded_net,
        )
        .await
        .unwrap();
    assert_eq!(got_net.size, target_len as u64);
    assert_eq!(sha256_file(&downloaded_net), expected_hash);

    // 100 MiB + 1 rejects before streaming.
    let too_big = temp.path().join("too-big.bin");
    std::fs::File::create(&too_big)
        .unwrap()
        .set_len(MAX_ARTIFACT_BYTES + 1)
        .unwrap();
    assert_eq!(
        net_client
            .put_artifact_file(
                "default",
                &too_big,
                "too-big.bin",
                "application/octet-stream",
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    // Same-length disk corruption on multi-chunk artifact fails download cleanly and leaves no temp file.
    let multi: Vec<u8> = (0..(600 * 1024)).map(|i| (i % 199) as u8).collect();
    let stored_multi = net_client
        .put_artifact_bytes(
            "default",
            "corrupt-multi.bin",
            "application/octet-stream",
            multi.clone(),
        )
        .await
        .unwrap();
    let (_, corrupt_path) = host
        .service
        .store
        .artifact_path("default", &stored_multi.id)
        .unwrap();
    let mut flipped = multi;
    flipped[500 * 1024] ^= 0xff;
    std::fs::write(&corrupt_path, &flipped).unwrap();

    let failed_dest = download_dir.join("corrupted.bin");
    let corrupt_err = net_client
        .get_artifact_to_file(
            pb::ArtifactRef {
                account_id: "default".into(),
                id: stored_multi.id,
            },
            &failed_dest,
        )
        .await
        .unwrap_err();
    assert_eq!(corrupt_err.code, ErrorCode::StorageFailure);
    assert!(!failed_dest.exists());
    let leftover_download_tmps: Vec<_> = std::fs::read_dir(&download_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".anticrumb-tmp-")
        })
        .collect();
    assert!(leftover_download_tmps.is_empty());

    // Zero leftover staging blobs on the server after failed/malformed uploads.
    let leftover_server_tmps: Vec<_> = std::fs::read_dir(net_dir.join("blobs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("tmp_"))
        .collect();
    assert!(leftover_server_tmps.is_empty());
    let leftover_staging: Vec<_> = std::fs::read_dir(net_dir.join("blobs").join(".tmp"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(leftover_staging.is_empty());

    // Nonexistent destination parent directory fails fast before streaming download.
    let missing_parent_dest = download_dir.join("does-not-exist").join("out.bin");
    let missing_parent_err = net_client
        .get_artifact_to_file(
            pb::ArtifactRef {
                account_id: "default".into(),
                id: uploaded_net.id.clone(),
            },
            &missing_parent_dest,
        )
        .await
        .unwrap_err();
    assert_eq!(missing_parent_err.code, ErrorCode::StorageFailure);

    // Mid-stream append to local source file rejects and never commits a partial artifact on server.
    for (label, initial) in [
        ("append-empty.bin", Vec::new()),
        (
            "append-multi.bin",
            (0..(600 * 1024))
                .map(|i| (i % 173) as u8)
                .collect::<Vec<_>>(),
        ),
    ] {
        let src_append = temp.path().join(label);
        std::fs::write(&src_append, &initial).unwrap();
        let initial_hash = sha256_file(&src_append);
        let opened = std::fs::File::open(&src_append).unwrap();
        {
            use std::io::Write as _;
            let mut appender = std::fs::OpenOptions::new()
                .append(true)
                .open(&src_append)
                .unwrap();
            appender.write_all(b"trailing-bytes").unwrap();
        }
        let append_err = net_client
            .put_artifact_verified_file(
                "default",
                label,
                "application/octet-stream",
                &initial_hash,
                initial.len() as u64,
                opened,
            )
            .await
            .unwrap_err();
        assert_eq!(append_err.code, ErrorCode::StorageFailure);
        let committed: Vec<Artifact> = host
            .service
            .store
            .list("default", "artifact")
            .unwrap()
            .into_iter()
            .filter(|a: &Artifact| a.name == label)
            .collect();
        assert!(committed.is_empty(), "{label} must not commit on append");
    }
    let post_append_staging: Vec<_> = std::fs::read_dir(net_dir.join("blobs").join(".tmp"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(post_append_staging.is_empty());

    // Relay multi-chunk streaming transfer & local corruption detection over TLS gRPC.
    let relay_local_dir = temp.path().join("relay-local");
    std::fs::create_dir_all(&relay_local_dir).unwrap();
    let workspace = relay::workspace(
        &relay_local_dir,
        Connection::Grpc {
            endpoint: endpoint.clone(),
            token: TOKEN.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some(pin),
        },
    )
    .await
    .unwrap();
    let relay_payload: Vec<u8> = (0..(700 * 1024)).map(|i| (i % 241) as u8).collect();
    let local_art = workspace
        .service
        .store
        .artifact(
            "default",
            "relay-multi.bin",
            "application/octet-stream",
            &relay_payload,
            "input",
        )
        .unwrap();
    let mut relay_req = request();
    relay_req.artifacts.push(local_art.id.clone());
    let run_id = workspace.service.submit_async(relay_req).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = workspace.service.snapshot(&run_id).unwrap();
            if let Some(error) = value.error {
                panic!("{error:?}");
            }
            if let Some(result) = value.result {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result.text, "complete");
    let (_, relay_corrupt_path) = workspace
        .service
        .store
        .artifact_path("default", &local_art.id)
        .unwrap();
    let mut tampered_local = relay_payload;
    tampered_local[0] ^= 0xff;
    std::fs::write(&relay_corrupt_path, &tampered_local).unwrap();
    let mut bad_relay_req = request();
    bad_relay_req.artifacts.push(local_art.id);
    let bad_run_id = workspace.service.submit_async(bad_relay_req).await.unwrap();
    let failed_turn = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = workspace.service.snapshot(&bad_run_id).unwrap();
            if !value.is_active() {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(failed_turn.state, "failed");
    assert_eq!(failed_turn.error.unwrap().code, ErrorCode::StorageFailure);

    workspace.shutdown();
    drop(workspace);
    drop(net_client);
    host.shutdown();
    task.await.unwrap();

    // 50 MiB streams cleanly over Embedded Synurang FFI as well.
    let ffi_dir = temp.path().join("ffi-host");
    std::fs::create_dir_all(&ffi_dir).unwrap();
    let (ffi_host, _) = fixture(&ffi_dir);
    let ffi_client =
        RpcClient::embedded(ffi_host.clone(), tokio::runtime::Handle::current()).unwrap();
    let uploaded_ffi = ffi_client
        .put_artifact_file(
            "default",
            &src_50mib,
            "payload-50mib.bin",
            "application/octet-stream",
        )
        .await
        .unwrap();
    assert_eq!(uploaded_ffi.size, target_len as u64);
    assert_eq!(uploaded_ffi.hash, expected_hash);
    let downloaded_ffi = download_dir.join("ffi-50mib.bin");
    let got_ffi = ffi_client
        .get_artifact_to_file(
            pb::ArtifactRef {
                account_id: "default".into(),
                id: uploaded_ffi.id,
            },
            &downloaded_ffi,
        )
        .await
        .unwrap();
    assert_eq!(got_ffi.size, target_len as u64);
    assert_eq!(sha256_file(&downloaded_ffi), expected_hash);

    // Aborted/cancelled FFI client stream leaves no staging leftovers under blobs/.tmp.
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
    let stall_stream = futures_util::stream::once(async move {
        pb::PutArtifactChunk {
            payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                account_id: "default".into(),
                name: "stalled.bin".into(),
                mime: "application/octet-stream".into(),
                sha256: digest(&[0xaa; 1024]),
                size: 1024,
            })),
        }
    })
    .chain(futures_util::stream::once(async move {
        let _ = gate_tx.send(());
        futures_util::future::pending::<pb::PutArtifactChunk>().await
    }));
    let mut upload_fut = Box::pin(ffi_client.put_artifact_stream(stall_stream));
    tokio::select! {
        res = &mut upload_fut => panic!("stalled stream finished unexpectedly: {res:?}"),
        _ = gate_rx => {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    drop(upload_fut);
    let mut ffi_staging_cleared = false;
    for _ in 0..40 {
        let empty = std::fs::read_dir(ffi_dir.join("blobs").join(".tmp"))
            .unwrap()
            .filter_map(|e| e.ok())
            .next()
            .is_none();
        if empty {
            ffi_staging_cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        ffi_staging_cleared,
        "aborted FFI upload left staging files under blobs/.tmp"
    );

    ffi_client.close().await.unwrap();
    drop(ffi_host);
}

#[tokio::test]
async fn download_stream_rejects_invalid_name_and_empty_data_chunk() {
    use tokio_stream::wrappers::ReceiverStream;

    enum Mode {
        InvalidName,
        EmptyDataChunk,
    }
    struct MaliciousArtifactServer {
        mode: std::sync::Arc<std::sync::Mutex<Mode>>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[tonic::async_trait]
    impl pb::artifact_service_server::ArtifactService for MaliciousArtifactServer {
        async fn put_artifact(
            &self,
            _request: tonic::Request<tonic::Streaming<pb::PutArtifactChunk>>,
        ) -> std::result::Result<tonic::Response<pb::Artifact>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used"))
        }
        type GetArtifactStream =
            ReceiverStream<std::result::Result<pb::GetArtifactChunk, tonic::Status>>;
        async fn get_artifact(
            &self,
            request: tonic::Request<pb::ArtifactRef>,
        ) -> std::result::Result<tonic::Response<Self::GetArtifactStream>, tonic::Status> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let target = request.into_inner();
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let payload = b"ok".to_vec();
            let name = match *self.mode.lock().unwrap() {
                Mode::InvalidName => "../evil.txt",
                Mode::EmptyDataChunk => "valid.txt",
            };
            let include_empty = matches!(*self.mode.lock().unwrap(), Mode::EmptyDataChunk);
            let art = pb::Artifact {
                id: target.id,
                scope: target.account_id,
                hash: digest(&payload),
                mime: "text/plain".into(),
                name: name.into(),
                size: payload.len() as u64,
                origin: "generated".into(),
                created_at: now_ms(),
            };
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(pb::GetArtifactChunk {
                        payload: Some(pb::get_artifact_chunk::Payload::Artifact(art)),
                    }))
                    .await;
                if include_empty {
                    let _ = tx
                        .send(Ok(pb::GetArtifactChunk {
                            payload: Some(pb::get_artifact_chunk::Payload::Data(Vec::new())),
                        }))
                        .await;
                }
                let _ = tx
                    .send(Ok(pb::GetArtifactChunk {
                        payload: Some(pb::get_artifact_chunk::Payload::Data(payload)),
                    }))
                    .await;
            });
            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let mode = Arc::new(Mutex::new(Mode::InvalidName));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server_mode = mode.clone();
    let server_calls = calls.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::artifact_service_server::ArtifactServiceServer::new(
                MaliciousArtifactServer {
                    mode: server_mode,
                    calls: server_calls,
                },
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let client = RpcClient::network(&endpoint, TOKEN, None, None)
        .await
        .unwrap();
    let local_store = Store::open(&temp.path().join("local-store")).unwrap();
    let dest_dir = temp.path().join("dest");
    std::fs::create_dir_all(&dest_dir).unwrap();
    let target = pb::ArtifactRef {
        account_id: "default".into(),
        id: "art_1".into(),
    };

    let trailing_sep = format!("{}/", dest_dir.display());
    for invalid_dest in ["", ".", "..", "/", &trailing_sep] {
        assert_eq!(
            client
                .get_artifact_to_file(target.clone(), std::path::Path::new(invalid_dest))
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest,
            "expected InvalidRequest for {invalid_dest:?}"
        );
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "invalid destination path must fail before opening RPC stream"
    );

    for expected_mode in [Mode::InvalidName, Mode::EmptyDataChunk] {
        *mode.lock().unwrap() = expected_mode;
        assert_eq!(
            client
                .get_artifact_bytes(target.clone())
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        let dest_file = dest_dir.join("out.txt");
        assert_eq!(
            client
                .get_artifact_to_file(target.clone(), &dest_file)
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert!(!dest_file.exists());
        assert_eq!(
            client
                .download_artifact_to_store(&local_store, "default", "art_1", "remote")
                .await
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    let tmps: Vec<_> = std::fs::read_dir(&dest_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(tmps.is_empty());

    drop(client);
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temporary_conversations_keep_browser_session_until_new_or_clear_and_recover_when_invalidated(
) {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let project = local_dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let (browser, echo) = fixture(browser_dir.path());
    let options = Options {
        root: browser_dir.path().into(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        ..Default::default()
    };
    let identity = options.identity().unwrap();
    let pin = certificate_fingerprint(
        &std::fs::read(browser_dir.path().join("tls/ca-certificate.pem")).unwrap(),
    )
    .unwrap();
    let (endpoint, task) = network(browser.clone(), identity).await;
    let workspace = relay::workspace(
        local_dir.path(),
        Connection::Grpc {
            endpoint,
            token: TOKEN.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some(pin),
        },
    )
    .await
    .unwrap();
    let client = RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();

    let run_turn = |client: RpcClient,
                    project: std::path::PathBuf,
                    task_text: String,
                    conversation_id: Option<String>,
                    temporary: bool| async move {
        let job = client
            .start_agent(pb::AgentRequest {
                task: task_text,
                workspace: project.to_string_lossy().into_owned(),
                account_id: "default".into(),
                session_mode: codec::session_mode_to(SessionMode::from_temporary(temporary)),
                no_shell: true,
                conversation_id,
                ..Default::default()
            })
            .await
            .unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let value = client
                    .get_agent(pb::AgentRef { id: job.id.clone() })
                    .await
                    .unwrap();
                if value.state != "running" {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(finished.state, "completed", "{finished:?}");
        finished.conversation_id
    };

    let read_full_prompt = |req: &TurnRequest| -> String {
        let mut combined = anticrumb::protocol::prompt(req).unwrap();
        for art_id in &req.artifacts {
            if let Ok((meta, bytes)) = echo.store.artifact_bytes(&req.account_id, art_id) {
                if meta.name.starts_with("anticrumb-prompt-") {
                    combined.push('\n');
                    combined.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
        combined
    };

    // Turn 1 (/temp on, new conversation): creates browser temporary session S1 and retains it.
    let turn1 = run_turn(
        client.clone(),
        project.clone(),
        "remember secret ALPHA".into(),
        None,
        true,
    )
    .await;
    let s1_id = {
        let sessions = browser
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].mode, SessionMode::Temporary);
        assert_eq!(sessions[0].state, SessionState::Ready);
        assert_eq!(sessions[0].remote_turns, 1);
        sessions[0].id.clone()
    };

    // Turn 2 (follow-up without /new or /clear): continues S1 in-place without previous-context dump.
    let _turn2 = run_turn(
        client.clone(),
        project.clone(),
        "what was the secret?".into(),
        Some(turn1),
        true,
    )
    .await;
    {
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].session_id.is_none());
        assert_eq!(requests[0].mode, SessionMode::Temporary);
        assert_eq!(requests[1].session_id.as_deref(), Some(s1_id.as_str()));
        assert_eq!(requests[1].mode, SessionMode::Temporary);
        let turn2_prompt = read_full_prompt(&requests[1]);
        assert!(turn2_prompt.contains("what was the secret?"));
        assert!(!turn2_prompt.contains("[PREVIOUS CONVERSATION"));

        let s1: Session = browser
            .service
            .store
            .get("local", "session", &s1_id)
            .unwrap();
        assert_eq!(s1.state, SessionState::Ready);
        assert_eq!(s1.remote_turns, 2);
    }

    // Turn 3 (after /new or /clear -> conversation_id is None): closes S1 and creates a fresh temp session S2.
    let turn3 = run_turn(
        client.clone(),
        project.clone(),
        "fresh temporary topic".into(),
        None,
        true,
    )
    .await;
    let s2_id = {
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].session_id.is_none());
        assert_eq!(requests[2].mode, SessionMode::Temporary);

        let s1: Session = browser
            .service
            .store
            .get("local", "session", &s1_id)
            .unwrap();
        assert_eq!(s1.state, SessionState::Closed);

        let sessions = browser
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap();
        let s2 = sessions
            .into_iter()
            .find(|s| s.id != s1_id)
            .expect("second temporary session created");
        assert_eq!(s2.mode, SessionMode::Temporary);
        assert_eq!(s2.state, SessionState::Ready);
        assert_eq!(s2.remote_turns, 1);
        s2.id
    };

    // Simulate Chrome restart invalidating S2 on the browser daemon: Turn 4 falls back cleanly
    // to a new temporary session S3 with full local previous-conversation context.
    let mut invalidated_s2: Session = browser
        .service
        .store
        .get("local", "session", &s2_id)
        .unwrap();
    invalidated_s2.state = SessionState::Invalidated;
    browser
        .service
        .store
        .put("local", "session", &s2_id, &invalidated_s2)
        .unwrap();
    let turn4 = run_turn(
        client.clone(),
        project.clone(),
        "continue after chrome restart".into(),
        Some(turn3),
        true,
    )
    .await;
    let s3_id = {
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[3].session_id.is_none());
        assert_eq!(requests[3].mode, SessionMode::Temporary);
        let turn4_prompt = read_full_prompt(&requests[3]);
        assert!(turn4_prompt.contains("[PREVIOUS CONVERSATION"));
        assert!(turn4_prompt.contains("fresh temporary topic"));

        let sessions = browser
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap();
        let s3 = sessions
            .into_iter()
            .find(|s| s.id != s1_id && s.id != s2_id)
            .expect("fallback temporary session created");
        assert_eq!(s3.mode, SessionMode::Temporary);
        assert_eq!(s3.state, SessionState::Ready);
        // Local workspace session mapped to invalidated S2 must have synced its state.
        let local_s2 = workspace
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap()
            .into_iter()
            .find(|s| s.remote_session_id.as_deref() == Some(s2_id.as_str()))
            .expect("local session mapping for S2");
        assert_eq!(local_s2.state, SessionState::Invalidated);
        s3.id
    };

    // Simulate S3 completely vanishing from browser daemon store (`GetSession` -> `NotFound`):
    // Relay must mark the local session invalidated and fall back cleanly to S3b.
    assert!(browser
        .service
        .store
        .delete("local", "session", &s3_id)
        .unwrap());
    let turn4b = run_turn(
        client.clone(),
        project.clone(),
        "continue after missing remote session".into(),
        Some(turn4),
        true,
    )
    .await;
    let s3b_id = {
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[4].session_id.is_none());
        assert_eq!(requests[4].mode, SessionMode::Temporary);
        let turn4b_prompt = read_full_prompt(&requests[4]);
        assert!(turn4b_prompt.contains("[PREVIOUS CONVERSATION"));
        assert!(turn4b_prompt.contains("continue after chrome restart"));

        let local_s3 = workspace
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap()
            .into_iter()
            .find(|s| s.remote_session_id.as_deref() == Some(s3_id.as_str()))
            .expect("local session mapping for deleted S3");
        assert_eq!(local_s3.state, SessionState::Invalidated);

        let sessions = browser
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap();
        let s3b = sessions
            .into_iter()
            .find(|s| s.id != s1_id && s.id != s2_id && s.id != s3_id)
            .expect("fallback session S3b after missing remote session");
        assert_eq!(s3b.mode, SessionMode::Temporary);
        assert_eq!(s3b.state, SessionState::Ready);
        s3b.id
    };

    // Remote state `SessionState::OutcomeUnknown` must NOT trigger silent session_fallback.
    let mut unknown_s3b: Session = browser
        .service
        .store
        .get("local", "session", &s3b_id)
        .unwrap();
    unknown_s3b.state = SessionState::OutcomeUnknown;
    browser
        .service
        .store
        .put("local", "session", &s3b_id, &unknown_s3b)
        .unwrap();
    let failed_job = client
        .start_agent(pb::AgentRequest {
            task: "must not silently fork outcome_unknown".into(),
            workspace: project.to_string_lossy().into_owned(),
            account_id: "default".into(),
            session_mode: pb::SessionMode::Temporary as i32,
            no_shell: true,
            conversation_id: Some(turn4b.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let value = client
                .get_agent(pb::AgentRef {
                    id: failed_job.id.clone(),
                })
                .await
                .unwrap();
            if value.state != "running" {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(failed.state, "failed", "{failed:?}");
    assert_eq!(
        echo.requests.lock().unwrap().len(),
        5,
        "must not submit a new fallback turn while remote session is outcome_unknown"
    );
    // Restore S3b (and its synced local mapping) to ready for the mode-switch test.
    unknown_s3b.state = SessionState::Ready;
    browser
        .service
        .store
        .put("local", "session", &s3b_id, &unknown_s3b)
        .unwrap();
    let mut local_s3b = workspace
        .service
        .store
        .list::<Session>("local", "session")
        .unwrap()
        .into_iter()
        .find(|s| s.remote_session_id.as_deref() == Some(s3b_id.as_str()))
        .expect("local session mapping for S3b");
    assert_eq!(local_s3b.state, SessionState::OutcomeUnknown);
    local_s3b.state = SessionState::Ready;
    workspace
        .service
        .store
        .put("local", "session", &local_s3b.id, &local_s3b)
        .unwrap();

    // Attempting to continue a temporary conversation in normal mode is rejected; switching
    // `/temp off` starts a new normal conversation (`conversation_id = None`), closing S3b
    // and starting a normal Chat session S4.
    let mode_err = client
        .start_agent(pb::AgentRequest {
            task: "cannot continue temporary conversation in normal mode".into(),
            workspace: project.to_string_lossy().into_owned(),
            account_id: "default".into(),
            session_mode: pb::SessionMode::Chat as i32,
            no_shell: true,
            conversation_id: Some(turn4b),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(mode_err.code, ErrorCode::InvalidRequest);
    let turn5 = run_turn(
        client.clone(),
        project.clone(),
        "switch to normal server chat".into(),
        None,
        false,
    )
    .await;
    {
        let requests = echo.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests[5].session_id.is_none());
        assert_eq!(requests[5].mode, SessionMode::Chat);

        let s3b: Session = browser
            .service
            .store
            .get("local", "session", &s3b_id)
            .unwrap();
        assert_eq!(s3b.state, SessionState::Closed);
    }

    // Deleting a conversation closes any retained `subagent_browser_session` on the browser host.
    let sub_turn = run_turn(
        client.clone(),
        project.clone(),
        "retained subagent temporary session".into(),
        None,
        true,
    )
    .await;
    let (local_sub_id, remote_sub_id) = {
        let local_sub = workspace
            .service
            .store
            .list::<Session>("local", "session")
            .unwrap()
            .into_iter()
            .find(|s| s.mode == SessionMode::Temporary && s.state == SessionState::Ready)
            .expect("live relayed temporary session for subagent");
        let remote_id = local_sub
            .remote_session_id
            .clone()
            .expect("remote session id");
        (local_sub.id, remote_id)
    };
    let sub_agent_id = client
        .get_conversation(pb::ConversationRef { id: sub_turn })
        .await
        .unwrap()
        .summary
        .unwrap()
        .latest_agent_id;
    // Detach synthetic helper agent row so only `turn5` owns `local_sub_id` via `subagent_browser_session`.
    assert!(workspace
        .service
        .store
        .delete("local", "agent", &sub_agent_id)
        .unwrap());
    workspace
        .service
        .store
        .event(
            &client
                .get_conversation(pb::ConversationRef { id: turn5.clone() })
                .await
                .unwrap()
                .summary
                .unwrap()
                .latest_agent_id,
            "subagent_browser_session",
            serde_json::json!({"session_id": local_sub_id}),
        )
        .unwrap();
    client
        .delete_agent_conversation(pb::DeleteConversationRequest {
            id: turn5,
            workspace: None,
            account_id: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let remote_sub: Session = browser
                .service
                .store
                .get("local", "session", &remote_sub_id)
                .unwrap();
            let local_sub: Session = workspace
                .service
                .store
                .get("local", "session", &local_sub_id)
                .unwrap();
            if remote_sub.state == SessionState::Closed && local_sub.state == SessionState::Deleted
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("deleting conversation should close retained subagent session on browser daemon");

    drop(client);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_service_proto_api_and_retention_work_across_synurang_ffi_and_grpc() {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let project = local_dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let (browser, _echo) = fixture(browser_dir.path());
    let browser_ffi =
        RpcClient::embedded(browser.clone(), tokio::runtime::Handle::current()).unwrap();

    // Create one retained temporary turn and one retained chat turn over Synurang FFI.
    let mut temp_req = TurnRequest::text("ffi temp");
    temp_req.mode = SessionMode::Temporary;
    temp_req.retention = SessionRetention::Retain;
    let temp_run = browser_ffi
        .create_turn(codec::request_to(temp_req))
        .await
        .unwrap();
    let temp_done = browser.service.wait(&temp_run.id).await.unwrap();
    let temp_sid = temp_done.result.unwrap().session_id;

    let mut chat_req = TurnRequest::text("ffi chat");
    chat_req.mode = SessionMode::Chat;
    chat_req.retention = SessionRetention::Retain;
    let chat_run = browser_ffi
        .create_turn(codec::request_to(chat_req))
        .await
        .unwrap();
    let chat_done = browser.service.wait(&chat_run.id).await.unwrap();
    let chat_sid = chat_done.result.unwrap().session_id;

    // GetSession over Synurang FFI returns typed proto enums and continuable flag.
    let fetched = browser_ffi
        .get_session(pb::SessionRef {
            id: temp_sid.clone(),
        })
        .await
        .unwrap();
    assert_eq!(fetched.id, temp_sid);
    assert_eq!(fetched.mode, pb::SessionMode::Temporary as i32);
    assert_eq!(fetched.state, pb::SessionState::Ready as i32);
    assert_eq!(fetched.provider, default_provider());
    assert!(fetched.continuable);

    let missing_err = browser_ffi
        .get_session(pb::SessionRef {
            id: "missing-session".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(missing_err.code, ErrorCode::NotFound);

    // Filtered ListSessions over Synurang FFI matches mode/state/account enums.
    let only_temp = browser_ffi
        .list_sessions(pb::ListSessionsRequest {
            account_id: Some("default".into()),
            mode: pb::SessionMode::Temporary as i32,
            state: pb::SessionState::Ready as i32,
        })
        .await
        .unwrap();
    assert_eq!(only_temp.sessions.len(), 1);
    assert_eq!(only_temp.sessions[0].id, temp_sid);

    let none_closed = browser_ffi
        .list_sessions(pb::ListSessionsRequest {
            account_id: None,
            mode: pb::SessionMode::Unspecified as i32,
            state: pb::SessionState::Closed as i32,
        })
        .await
        .unwrap();
    assert!(none_closed.sessions.is_empty());

    // Presentation JSON exposes readable string mode/state for both list and single show.
    let single_json = anticrumb::rpc::workspace_client::presentation(
        &browser_ffi,
        "GET",
        &format!("/api/sessions/{temp_sid}"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(single_json["mode"], "temporary");
    assert_eq!(single_json["state"], "ready");
    assert_eq!(single_json["continuable"], true);

    // Batch CloseSessions closes live sessions, skips missing/duplicate IDs, and updates state.
    let batch = browser_ffi
        .close_sessions(pb::CloseSessionsRequest {
            session_ids: vec![temp_sid.clone(), temp_sid.clone(), "does-not-exist".into()],
        })
        .await
        .unwrap();
    assert_eq!(batch.closed, vec![temp_sid.clone()]);
    assert_eq!(batch.skipped, vec!["does-not-exist".to_string()]);
    assert!(batch.errors.is_empty());

    let after_close = browser_ffi
        .get_session(pb::SessionRef {
            id: temp_sid.clone(),
        })
        .await
        .unwrap();
    assert_eq!(after_close.state, pb::SessionState::Closed as i32);
    assert!(!after_close.continuable);

    // Continuing a closed temporary session returns typed ErrorCode::SessionUnavailable over FFI.
    let mut resume_closed = TurnRequest::text("cannot continue closed temp");
    resume_closed.mode = SessionMode::Temporary;
    resume_closed.session_id = Some(temp_sid.clone());
    let unavailable = browser_ffi
        .create_turn(codec::request_to(resume_closed))
        .await
        .unwrap_err();
    assert_eq!(unavailable.code, ErrorCode::SessionUnavailable);

    // Close remaining chat session cleanly via CloseSessions; already-closed temp_sid is skipped.
    let batch2 = browser_ffi
        .close_sessions(pb::CloseSessionsRequest {
            session_ids: vec![temp_sid, chat_sid.clone()],
        })
        .await
        .unwrap();
    assert_eq!(batch2.closed, vec![chat_sid]);
    assert_eq!(batch2.skipped.len(), 1);
    assert!(batch2.errors.is_empty());

    // Workspace AgentRequest over gRPC relay + Synurang FFI with SessionRetention::Ephemeral
    // closes its temporary session on finish and preserves typed request fields without bit-packing.
    let options = Options {
        root: browser_dir.path().into(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        ..Default::default()
    };
    let identity = options.identity().unwrap();
    let pin = certificate_fingerprint(
        &std::fs::read(browser_dir.path().join("tls/ca-certificate.pem")).unwrap(),
    )
    .unwrap();
    let (endpoint, grpc_task) = network(browser.clone(), identity).await;
    let workspace = relay::workspace(
        local_dir.path(),
        Connection::Grpc {
            endpoint,
            token: TOKEN.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some(pin),
        },
    )
    .await
    .unwrap();
    let ws_ffi = RpcClient::embedded(workspace.clone(), tokio::runtime::Handle::current()).unwrap();
    let job = ws_ffi
        .start_agent(pb::AgentRequest {
            task: "ephemeral temp agent".into(),
            workspace: project.to_string_lossy().into_owned(),
            account_id: "default".into(),
            session_mode: pb::SessionMode::Temporary as i32,
            no_shell: true,
            max_steps: 7,
            session_retention: pb::SessionRetention::Ephemeral as i32,
            chat_only: true,
            permissions: Some(pb::ToolPermissions {
                read: true,
                edit: false,
                shell: false,
                browser: true,
                subagent: false,
                ast: true,
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let value = ws_ffi
                .get_agent(pb::AgentRef { id: job.id.clone() })
                .await
                .unwrap();
            if value.state != "running" {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.state, "completed", "{finished:?}");
    let finished_req = finished.request.as_ref().expect("agent request preserved");
    assert_eq!(finished_req.session_mode, pb::SessionMode::Temporary as i32);
    assert_eq!(
        finished_req.session_retention,
        pb::SessionRetention::Ephemeral as i32
    );
    assert_eq!(finished_req.max_steps, 7);
    assert!(finished_req.chat_only);
    assert_eq!(
        finished_req.permissions,
        Some(pb::ToolPermissions {
            read: true,
            edit: false,
            shell: false,
            browser: true,
            subagent: false,
            ast: true,
        })
    );
    let ready_temp = browser_ffi
        .list_sessions(pb::ListSessionsRequest {
            account_id: Some("default".into()),
            mode: pb::SessionMode::Temporary as i32,
            state: pb::SessionState::Ready as i32,
        })
        .await
        .unwrap();
    assert!(
        ready_temp.sessions.is_empty(),
        "SESSION_RETENTION_EPHEMERAL must close the browser temporary session on completion"
    );
    drop(ws_ffi);
    drop(browser_ffi);
    grpc_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_tool_approval_ask_mode_works_across_synurang_ffi_and_grpc() {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let project = local_dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("sample.txt"), "before").unwrap();

    let (browser, echo) = fixture(browser_dir.path());
    *echo.replies.lock().unwrap() = [
        format!(
            "[TOOL_CALL]\n{}\n[/TOOL_CALL]",
            json!({
                "tool_calls": [
                    {"tool": "write_file", "args": {"path": "a.txt", "content": "first"}},
                    {"tool": "edit_file", "args": {"path": "sample.txt", "old_text": "before", "new_text": "after"}}
                ]
            })
        ),
        format!(
            "[TOOL_CALL]\n{}\n[/TOOL_CALL]",
            json!({
                "tool_calls": [
                    {"tool": "write_file", "args": {"path": "b.txt", "content": "second"}}
                ]
            })
        ),
        "all done".to_string(),
    ]
    .into();

    let options = Options {
        root: browser_dir.path().into(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        ..Default::default()
    };
    let identity = options.identity().unwrap();
    let pin = certificate_fingerprint(
        &std::fs::read(browser_dir.path().join("tls/ca-certificate.pem")).unwrap(),
    )
    .unwrap();
    let (endpoint, grpc_task) = network(browser, identity).await;
    let workspace = relay::workspace(
        local_dir.path(),
        Connection::Grpc {
            endpoint,
            token: TOKEN.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some(pin),
        },
    )
    .await
    .unwrap();
    let ws_ffi = RpcClient::embedded(workspace, tokio::runtime::Handle::current()).unwrap();
    let job = ws_ffi
        .start_agent(pb::AgentRequest {
            task: "Apply first batch and reject second batch".into(),
            workspace: project.to_string_lossy().into_owned(),
            account_id: "default".into(),
            chat_only: false,
            no_shell: true,
            permissions: Some(pb::ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: false,
                subagent: false,
                ast: true,
            }),
            tool_approval: pb::ToolApprovalMode::Ask as i32,
            ..Default::default()
        })
        .await
        .unwrap();

    let mut after = 0;
    let mut approval_batches = 0usize;
    let finished = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let page = ws_ffi
                .read_agent_events(pb::EventCursor {
                    run_id: job.id.clone(),
                    after,
                })
                .await
                .unwrap();
            for ev in page.events {
                after = ev.sequence;
                if ev.kind == "tool_approval_requested" {
                    let data: serde_json::Value = serde_json::from_str(&ev.data_json).unwrap();
                    let approval_id = data["id"].as_str().unwrap().to_owned();
                    let calls = data["calls"].as_array().unwrap();
                    let approve = match approval_batches {
                        0 => {
                            assert_eq!(calls.len(), 2);
                            assert_eq!(calls[0]["tool"], "write_file");
                            assert_eq!(calls[1]["tool"], "edit_file");
                            true
                        }
                        1 => {
                            assert_eq!(calls.len(), 1);
                            assert_eq!(calls[0]["tool"], "write_file");
                            false
                        }
                        other => panic!("unexpected approval batch #{other}"),
                    };
                    approval_batches += 1;
                    ws_ffi
                        .approve_tool(pb::ApproveToolRequest {
                            id: job.id.clone(),
                            approval_id,
                            approved: approve,
                        })
                        .await
                        .unwrap();
                }
            }
            let value = ws_ffi
                .get_agent(pb::AgentRef { id: job.id.clone() })
                .await
                .unwrap();
            if value.state != "running" {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!(approval_batches, 2);
    assert_eq!(finished.state, "completed", "{finished:?}");
    assert_eq!(finished.result.unwrap().final_response, "all done");
    assert_eq!(
        finished.request.unwrap().tool_approval,
        pb::ToolApprovalMode::Ask as i32
    );
    assert_eq!(
        std::fs::read_to_string(project.join("a.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("sample.txt")).unwrap(),
        "after"
    );
    assert!(!project.join("b.txt").exists());

    {
        let requests = echo.requests.lock().unwrap();
        let last_req = requests.last().unwrap();
        let mut full_prompt = anticrumb::protocol::prompt(last_req).unwrap();
        for art_id in &last_req.artifacts {
            if let Ok((meta, bytes)) = echo.store.artifact_bytes(&last_req.account_id, art_id) {
                if meta.name.starts_with("anticrumb-prompt-") {
                    full_prompt.push('\n');
                    full_prompt.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
        assert!(full_prompt.contains("Tool execution rejected by user."));
    }

    let http_err = anticrumb::rpc::workspace_client::presentation(
        &ws_ffi,
        "POST",
        &format!("/api/agents/{}/approve-tool", job.id),
        Some(&json!({"approval_id": "missing", "approved": true})),
    )
    .await
    .unwrap_err();
    assert_eq!(http_err.code, ErrorCode::InvalidRequest);

    drop(ws_ffi);
    grpc_task.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_agent_events_streams_ephemeral_tool_output_with_zero_sqlite_inserts() {
    let browser_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let project = local_dir.path().join("project");
    std::fs::create_dir(&project).unwrap();

    let (browser, echo) = fixture(browser_dir.path());
    *echo.replies.lock().unwrap() = [
        format!(
            "[TOOL_CALL]\n{}\n[/TOOL_CALL]",
            json!({
                "tool_calls": [
                    {
                        "tool": "run_shell",
                        "args": {"command": "printf 'stream-alpha\\n'; sleep 0.12; printf 'stream-beta\\n'"}
                    }
                ]
            })
        ),
        "stream turn complete".to_string(),
    ]
    .into();

    let options = Options {
        root: browser_dir.path().into(),
        listen: "127.0.0.1:0".into(),
        self_signed: true,
        ..Default::default()
    };
    let identity = options.identity().unwrap();
    let pin = certificate_fingerprint(
        &std::fs::read(browser_dir.path().join("tls/ca-certificate.pem")).unwrap(),
    )
    .unwrap();
    let (endpoint, grpc_task) = network(browser, identity).await;
    let workspace = relay::workspace(
        local_dir.path(),
        Connection::Grpc {
            endpoint,
            token: TOKEN.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some(pin),
        },
    )
    .await
    .unwrap();
    let ws_store = workspace.service.store.clone();
    let ws_ffi = RpcClient::embedded(workspace, tokio::runtime::Handle::current()).unwrap();

    let job = ws_ffi
        .start_agent(pb::AgentRequest {
            task: "Run shell command and stream live output".into(),
            workspace: project.to_string_lossy().into_owned(),
            account_id: "default".into(),
            chat_only: false,
            no_shell: false,
            permissions: Some(pb::ToolPermissions {
                read: true,
                edit: false,
                shell: true,
                browser: false,
                subagent: false,
                ast: false,
            }),
            tool_approval: pb::ToolApprovalMode::Auto as i32,
            ..Default::default()
        })
        .await
        .unwrap();

    let mut stream = ws_ffi
        .watch_agent_events(pb::EventCursor {
            run_id: job.id.clone(),
            after: 0,
        })
        .await
        .unwrap();

    let mut streamed_kinds = Vec::new();
    let mut streamed_stdout = String::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(ev) = stream.next().await {
            let ev = ev.unwrap();
            if ev.kind == "tool_output_chunk" {
                let data: serde_json::Value = serde_json::from_str(&ev.data_json).unwrap();
                if let Some(t) = data["text"].as_str() {
                    streamed_stdout.push_str(t);
                }
            }
            streamed_kinds.push(ev.kind);
        }
    })
    .await
    .expect("watch_agent_events stream must terminate after agent_terminal");

    assert!(
        streamed_kinds.iter().any(|k| k == "tool_output_chunk"),
        "expected live tool_output_chunk over WatchAgentEvents: {streamed_kinds:?}"
    );
    assert!(
        streamed_stdout.contains("stream-alpha") && streamed_stdout.contains("stream-beta"),
        "unexpected streamed shell output: {streamed_stdout:?}"
    );
    assert!(streamed_kinds.iter().any(|k| k == "tool_started"));
    assert!(streamed_kinds.iter().any(|k| k == "tool_finished"));
    assert!(streamed_kinds.iter().any(|k| k == "agent_terminal"));

    // Verify SQLite events table has 0 INSERTs for any ephemeral kind!
    let db_events = ws_store.events(&job.id, 0).unwrap();
    let db_kinds: Vec<&str> = db_events.iter().map(|e| e.kind.as_str()).collect();
    for ephemeral in [
        "tool_output_chunk",
        "model_chunk",
        "browser_stage",
        "subagent_progress",
        "request_payload",
        "model_input",
    ] {
        assert!(
            !db_kinds.contains(&ephemeral),
            "ephemeral event {ephemeral} must NOT be persisted in SQLite: {db_kinds:?}"
        );
    }

    drop(ws_ffi);
    grpc_task.abort();
}
