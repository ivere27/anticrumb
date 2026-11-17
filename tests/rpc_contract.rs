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
    serde_json::from_value(json!({"messages":[{"role":"user","blocks":[{"type":"text","text":"Use the native attachment"}]}],"keep_session":true})).unwrap()
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
                let value = client.get_agent(job.clone()).await.unwrap();
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
    assert_eq!(count, 36);
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
    let cert = root.path().join("tls/certificate.pem");
    let identity = options.identity().unwrap();
    let first = std::fs::read(&cert).unwrap();
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
    let (endpoint, task) = network(host.clone(), identity).await;
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
    let state = root.path().join("browser");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_anticrumb"))
        .args(["daemon", "--help"])
        .env("ANTICRUMB_HOME", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("anticrumb daemon"), "{help}");
    assert!(!help.contains("--ipc"), "{help}");
    assert!(help.contains("--listen"), "{help}");
    assert!(help.contains("--token"), "{help}");
    assert!(!help.contains("--token-file"), "{help}");
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
    assert!(defaults.listen.is_empty());
    assert!(!defaults.self_signed);
    assert!(defaults.token.is_none());
    assert!(parse(&["--listen", "127.0.0.1:0"]).unwrap().self_signed);
    for args in [
        vec!["--ipc", "--token", TOKEN],
        vec!["--tls-cert", "cert"],
        vec!["--token-file", "file"],
        vec!["--insecure-http", "--listen", "127.0.0.1:0"],
        vec![
            "--token",
            TOKEN,
            "--insecure-http",
            "--listen",
            "0.0.0.0:50051",
        ],
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
    let data = b"private bytes that must never be expanded into a prompt".to_vec();
    let request = pb::PutArtifactRequest {
        account_id: "default".into(),
        name: "sample file.txt".into(),
        mime: "text/plain".into(),
        sha256: digest(&data),
        data: data.clone(),
    };
    let a = client.put_artifact(request.clone()).await.unwrap();
    assert_eq!(client.put_artifact(request.clone()).await.unwrap().id, a.id);
    let downloaded = client
        .get_artifact(pb::ArtifactRef {
            account_id: "default".into(),
            id: a.id,
        })
        .await
        .unwrap();
    assert_eq!(downloaded.data, data);
    for name in ["../secret", "C:\\secret.txt", "..", "a/b", "a\\b"] {
        let mut invalid = request.clone();
        invalid.name = name.into();
        assert_eq!(
            client.put_artifact(invalid).await.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
    let mut invalid = request;
    invalid.sha256 = "bad".into();
    assert_eq!(
        client.put_artifact(invalid).await.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
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
    input.keep_session = false;
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
    assert_eq!(sessions[0].state, "awaiting_human");
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
    request.keep_session = true;
    let run = workspace
        .wait(&workspace.submit(request).unwrap())
        .await
        .unwrap();
    let session = run.result.unwrap().session_id;
    for prompt in ["fail before submit", "fail before submit", "new prompt"] {
        let mut request = TurnRequest::text(prompt);
        request.keep_session = true;
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
        &std::fs::read(browser_dir.path().join("tls/certificate.pem")).unwrap(),
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
    assert_eq!(remote.state, "closed");
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
        "deleted"
    );
    assert_eq!(
        browser
            .service
            .store
            .get::<Session>("local", "session", &remote.id)
            .unwrap()
            .state,
        "deleted"
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
    stale.state = "ready".into();
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
    continuation.keep_session = true;
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
        "deleted"
    );
    assert_eq!(
        echo.requests.lock().unwrap().len(),
        1,
        "a delta must never be replayed as a fresh request"
    );

    // Partial failure invalidates only this workspace's old batch scope.
    let mut old = local.clone();
    old.id = "partial-old".into();
    old.state = "ready".into();
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
    still_ready.state = "ready".into();
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
        "invalidated"
    );
    assert_eq!(
        workspace
            .service
            .store
            .get::<Session>("local", "session", &newer.id)
            .unwrap()
            .state,
        "ready"
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
    seen_deleted.state = "ready".into();
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
            "deleted"
        );
        assert_eq!(
            observer
                .service
                .store
                .get::<Session>("local", "session", &seen_ready.id)
                .unwrap()
                .state,
            "ready"
        );
        assert_eq!(
            observer
                .service
                .store
                .get::<Session>("local", "session", &other_host.id)
                .unwrap()
                .state,
            "ready"
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
        .delete_agent(pb::AgentRef {
            id: "embedded".into(),
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
            .request("DELETE", "/api/agents/presentation", None)
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
            .delete_agent(pb::AgentRef {
                id: "network".into()
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
        .delete_agent(pb::AgentRef {
            id: "network".into(),
        })
        .await
        .unwrap();
    assert!(store.events("network", 0).unwrap().is_empty());
    assert!(authorized
        .delete_agent(pb::AgentRef {
            id: "active".into()
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
                let value = client.get_agent(job.clone()).await.unwrap();
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
    let browser_dir = dir.path().join("browser");
    let providers_dir = dir.path().join("providers");
    let host = Host::browser_with_providers_dir(&browser_dir, Some(&providers_dir)).unwrap();
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
    let reopened = Host::browser_with_providers_dir(&browser_dir, Some(&providers_dir)).unwrap();
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
    let final_host = Host::browser_with_providers_dir(&browser_dir, Some(&providers_dir)).unwrap();
    let final_client =
        RpcClient::embedded(final_host.clone(), tokio::runtime::Handle::current()).unwrap();
    let final_list = final_client.list_providers(pb::Empty {}).await.unwrap();
    assert!(!final_list
        .providers
        .iter()
        .any(|p| p.id == "test-claude-web"));
    final_client.close().await.unwrap();
}
