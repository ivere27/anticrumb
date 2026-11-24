use anticrumb::{
    agent::CancelToken,
    domain::*,
    service::{Backend, Emit, Service},
    store::Store,
};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tower::ServiceExt;

struct Fake {
    active: AtomicUsize,
    max: AtomicUsize,
    calls: AtomicUsize,
    wait_for_peer: AtomicBool,
}
impl Backend for Fake {
    fn generate(
        &self,
        _account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        if self.wait_for_peer.load(Ordering::SeqCst) {
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while self.max.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        emit("submitted", json!({}))?;
        for _ in 0..10 {
            if cancel.is_cancelled() {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return Err(ServiceError::new(ErrorCode::Cancelled, "fake", "cancelled"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        let prompt = match &request.messages[0].blocks[0] {
            Block::Text { text } => text.clone(),
            _ => "".into(),
        };
        if prompt == "disconnect" {
            return Err(ServiceError::new(
                ErrorCode::SubmissionOutcomeUnknown,
                "fake",
                "connection lost",
            ));
        }
        let (text, tool_calls) = if request.tools.is_empty() {
            (prompt, vec![])
        } else {
            (
                "".into(),
                vec![ToolCall {
                    kind: ToolKind::Function,
                    id: "call_1".into(),
                    name: request.tools[0].name.clone(),
                    arguments: json!({"path":"x"}),
                }],
            )
        };
        if !text.is_empty() {
            emit("text_delta", json!({"text":text}))?;
        }
        Ok(TurnResult {
            session_id: session.id.clone(),
            text,
            tool_calls,
            artifacts: vec![],
            model: Some("fake".into()),
            elapsed_ms: 50,
        })
    }
    fn close(&self, _: &Account, _: &Session) -> Result<()> {
        Ok(())
    }
}
fn fixture() -> (tempfile::TempDir, Arc<Service>, Arc<Fake>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .put(
            "local",
            "account",
            "default",
            &Account {
                id: "default".into(),
                provider: default_provider(),
                profile: dir.path().join("profile"),
                subprofile: None,
                port: 9222,
                headless: true,
                state: "ready".into(),
            },
        )
        .unwrap();
    let backend = Arc::new(Fake {
        active: 0.into(),
        max: 0.into(),
        calls: 0.into(),
        wait_for_peer: false.into(),
    });
    let service = Service::new(store, backend.clone()).unwrap();
    (dir, service, backend)
}

#[test]
fn media_recovery_uses_latest_session_and_holds_gate_after_caller_cancellation() {
    use std::{future::Future, sync::Mutex, task::Wake};
    struct MediaBackend {
        entered: tokio::sync::Notify,
        observed_turns: AtomicUsize,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Backend for MediaBackend {
        fn generate(
            &self,
            _: &Account,
            _: &mut Session,
            _: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            panic!("media recovery must not generate a turn")
        }
        fn close(&self, _: &Account, _: &Session) -> Result<()> {
            Ok(())
        }
        fn media(&self, _: &Account, session: &Session) -> Result<Vec<String>> {
            self.observed_turns
                .store(session.remote_turns as usize, Ordering::SeqCst);
            self.entered.notify_one();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .map_err(ServiceError::storage)?;
            Ok(vec![])
        }
    }
    struct WakeOnReady(tokio::sync::Notify);
    impl Wake for WakeOnReady {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (_dir, original, _) = fixture();
            let store = original.store.clone();
            let (release, released) = std::sync::mpsc::channel();
            let backend = Arc::new(MediaBackend {
                entered: tokio::sync::Notify::new(),
                observed_turns: 0.into(),
                release: Mutex::new(released),
            });
            let service = Service::new(store.clone(), backend.clone()).unwrap();
            let mut session = Session {
                id: "media-session".into(),
                provider: default_provider(),
                account_id: "default".into(),
                mode: SessionMode::Chat,
                target_id: Some("owned-target".into()),
                remote_url: None,
                remote_session_id: None,
                history: vec![],
                state: SessionState::Ready,
                model: None,
                thinking: None,
                tool_digest: String::new(),
                remote_turns: 1,
                last_used: now_ms(),
                reuse_scope: None,
            };
            store
                .put("local", "session", &session.id, &session)
                .unwrap();
            let gate = service.account_gate("default");
            let turn_permit = gate.clone().acquire_owned().await.unwrap();

            // Hold the sole blocking worker so the first poll deterministically
            // suspends at the initial session read, then wait for that read.
            let (unblock, blocked) = std::sync::mpsc::channel();
            let (started, started_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            started_rx.await.unwrap();
            let recovering_service = service.clone();
            let id = session.id.clone();
            let mut recovery = Box::pin(async move { recovering_service.recover_media(&id).await });
            let ready = Arc::new(WakeOnReady(tokio::sync::Notify::new()));
            let waker = std::task::Waker::from(ready.clone());
            assert!(recovery
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending());
            unblock.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), ready.0.notified())
                .await
                .unwrap();
            // This poll consumes the completed read and parks at the account
            // gate. A turn then commits its newer response before unlocking.
            assert!(recovery
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending());
            session.remote_turns = 2;
            store
                .put("local", "session", &session.id, &session)
                .unwrap();
            drop(turn_permit);
            let caller = tokio::spawn(recovery);
            tokio::time::timeout(Duration::from_secs(3), backend.entered.notified())
                .await
                .unwrap();

            // Cancelling the async caller cannot stop an already running
            // blocking download; it must retain exclusive browser ownership.
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            let still_locked = gate.clone().try_acquire_owned().is_err();
            release.send(()).unwrap();
            let _finished = tokio::time::timeout(Duration::from_secs(3), gate.acquire_owned())
                .await
                .unwrap()
                .unwrap();
            assert!(still_locked);
            assert_eq!(backend.observed_turns.load(Ordering::SeqCst), 2);
            assert_eq!(
                store.events(&session.id, 0).unwrap().last().unwrap().kind,
                "artifacts_recovered"
            );
        });
}

#[tokio::test]
async fn parallel_account_sessions_preserve_isolation_and_idempotency() {
    let (_dir, service, backend) = fixture();
    backend.wait_for_peer.store(true, Ordering::SeqCst);
    let mut a = TurnRequest::text("A");
    a.idempotency_key = Some("A-key".into());
    let first = service.submit(a.clone()).unwrap();
    assert_eq!(service.submit(a).unwrap(), first);
    let second = service.submit(TurnRequest::text("B")).unwrap();
    let ar = service.wait(&first).await.unwrap().result.unwrap();
    let br = service.wait(&second).await.unwrap().result.unwrap();
    assert_eq!(ar.text, "A");
    assert_eq!(br.text, "B");
    assert_ne!(ar.session_id, br.session_id);
    assert_eq!(backend.max.load(Ordering::SeqCst), 2);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn temporary_turns_obey_worker_limit_and_same_session_turns_remain_serial() {
    let (_dir, service, backend) = fixture();
    service
        .store
        .put(
            "local",
            "runtime",
            "tab_pool_config",
            &json!({"worker_tabs":2}),
        )
        .unwrap();
    backend.wait_for_peer.store(true, Ordering::SeqCst);
    let mut runs = Vec::new();
    for n in 0..6 {
        let mut request = TurnRequest::text(format!("worker {n}"));
        request.mode = SessionMode::Temporary;
        runs.push(service.submit(request).unwrap());
    }
    for id in runs {
        assert_eq!(service.wait(&id).await.unwrap().state, "completed");
    }
    assert_eq!(backend.max.load(Ordering::SeqCst), 2);

    backend.wait_for_peer.store(false, Ordering::SeqCst);
    let mut seed = TurnRequest::text("seed");
    seed.retention = SessionRetention::Retain;
    let seed_id = service.submit(seed).unwrap();
    let session = service
        .wait(&seed_id)
        .await
        .unwrap()
        .result
        .unwrap()
        .session_id;
    backend.max.store(0, Ordering::SeqCst);
    // Queue both requests before either one changes the session state.
    let exclusive = service
        .account_gate("default")
        .acquire_owned()
        .await
        .unwrap();
    let mut first = TurnRequest::text("first continuation");
    first.session_id = Some(session.clone());
    first.retention = SessionRetention::Retain;
    let mut second = first.clone();
    second.messages = TurnRequest::text("second continuation").messages;
    let a = service.submit(first).unwrap();
    let b = service.submit(second).unwrap();
    drop(exclusive);
    assert_eq!(service.wait(&a).await.unwrap().state, "completed");
    assert_eq!(service.wait(&b).await.unwrap().state, "completed");
    assert_eq!(backend.max.load(Ordering::SeqCst), 1);
    let saved: Session = service.store.get("local", "session", &session).unwrap();
    assert_eq!(saved.history.len(), 6);
}

#[tokio::test]
async fn queued_cancellation_and_unknown_outcome_do_not_replay() {
    let (_dir, service, backend) = fixture();
    let first = service.submit(TurnRequest::text("disconnect")).unwrap();
    let second = service.submit(TurnRequest::text("never submit")).unwrap();
    service.cancel(&second).unwrap();
    assert_eq!(service.wait(&first).await.unwrap().state, "outcome_unknown");
    assert_eq!(service.wait(&second).await.unwrap().state, "cancelled");
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

async fn http_call(service: Arc<Service>, path: &str, body: Value) -> (u16, String) {
    let app = anticrumb::http::router(service, "test-secret".into()).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-secret")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn chat_stream_and_responses_tool_stream_have_terminal_events() {
    let (_dir, service, _) = fixture();
    let (status, body) = http_call(
        service.clone(),
        "/v1/chat/completions",
        json!({"messages":[{"role":"user","content":"hello"}],"stream":true}),
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("hello"));
    assert!(body.contains("[DONE]"));
    let (status,body)=http_call(service,"/v1/responses",json!({"input":"read x","stream":true,"tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]})).await;
    assert_eq!(status, 200);
    assert!(body.contains("response.created"));
    assert!(body.contains("response.function_call_arguments.done"));
    assert!(body.contains("response.completed"));
    assert!(body.contains("call_1"));
}

#[tokio::test]
async fn api_auth_diagnostics_do_not_expose_secrets_or_submit_requests() {
    let (dir, service, backend) = fixture();
    let secret = "synthetic-gateway-token";
    let wrong = "synthetic-invalid-token";
    let app = anticrumb::http::router(service.clone(), secret.into()).unwrap();
    let body = json!({"messages":[{"role":"user","content":"private-body-marker"}]});
    for credentials in [
        None,
        Some(("authorization", format!("Bearer {wrong}"))),
        Some(("authorization", "Bearer short".into())),
        Some(("authorization", format!("Basic {secret}"))),
        Some(("x-api-key", wrong.into())),
        Some(("x-goog-api-key", wrong.into())),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri(format!(
                "/v1/chat/completions?token={secret}&debug=private-query-marker"
            ))
            .header("content-type", "application/json")
            .header("x-debug", "private-header-marker");
        if let Some((header, value)) = credentials {
            request = request.header(header, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["error"]["message"], "invalid daemon token");
        assert_eq!(value["error"]["code"], "invalid_daemon_token");
        assert_eq!(value["error"]["stage"], "daemon_auth");
        let hint = value["error"]["hint"].as_str().unwrap();
        for guidance in [
            "API key",
            "anticrumb server",
            "--token",
            "server address",
            "Chrome",
        ] {
            assert!(hint.contains(guidance));
        }
        for private in [
            secret,
            wrong,
            "private-query-marker",
            "private-header-marker",
            "private-body-marker",
            dir.path().to_str().unwrap(),
        ] {
            assert!(!text.contains(private));
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(service
            .store
            .list::<Run>("local", "run")
            .unwrap()
            .is_empty());
    }
    for (header, value) in [
        ("authorization", format!("Bearer {secret}")),
        ("x-api-key", secret.into()),
        ("x-goog-api-key", secret.into()),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header(header, value)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["choices"][0]["message"]["content"],
            "private-body-marker"
        );
    }
    assert_eq!(backend.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn api_rejects_unsupported_content() {
    let (_dir, service, _) = fixture();
    let (status,_)=http_call(service,"/v1/chat/completions",json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://127.0.0.1/private"}}]}]})).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn independent_accounts_execute_concurrently_and_sessions_cannot_cross_accounts() {
    let (_dir, service, backend) = fixture();
    backend.wait_for_peer.store(true, Ordering::SeqCst);
    let mut second: Account = service.store.get("local", "account", "default").unwrap();
    second.id = "other".into();
    second.port = 9223;
    service
        .store
        .put("local", "account", "other", &second)
        .unwrap();
    let mut a = TurnRequest::text("one");
    a.retention = SessionRetention::Retain;
    let mut b = TurnRequest::text("two");
    b.account_id = "other".into();
    let first = service.submit(a).unwrap();
    let other = service.submit(b).unwrap();
    let first = service.wait(&first).await.unwrap();
    service.wait(&other).await.unwrap();
    assert_eq!(backend.max.load(Ordering::SeqCst), 2);
    let mut bad = TurnRequest::text("cross account");
    bad.account_id = "other".into();
    bad.session_id = Some(first.result.unwrap().session_id);
    assert!(service.submit(bad).is_err());
}

#[tokio::test]
async fn idempotent_result_survives_account_pause() {
    let (_dir, service, backend) = fixture();
    let mut request = TurnRequest::text("once");
    request.idempotency_key = Some("same".into());
    let id = service.submit(request.clone()).unwrap();
    service.wait(&id).await.unwrap();
    let mut account: Account = service.store.get("local", "account", "default").unwrap();
    account.state = "awaiting_human".into();
    service
        .store
        .put("local", "account", "default", &account)
        .unwrap();
    assert_eq!(service.submit(request).unwrap(), id);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert!(service.submit(TurnRequest::text("new")).is_err());
}

#[tokio::test]
async fn sdk_mcp_negotiates_lists_and_calls_the_same_service() {
    use base64::Engine as _;
    use rmcp::{
        model::{CallToolRequestParams, ReadResourceRequestParams, ResourceContents},
        ServiceExt,
    };
    let (_dir, service, _) = fixture();
    let payload: Vec<u8> = (0..600 * 1024).map(|i| ((i * 29) % 251) as u8).collect();
    let stored = service
        .store
        .artifact(
            "default",
            "mcp-multi.bin",
            "application/octet-stream",
            &payload,
            "generated",
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http = tokio::spawn(async move {
        let host =
            anticrumb::rpc::host::Host::with_service(service, None, "workspace", None).unwrap();
        anticrumb::rpc::server::serve(
            host,
            Some("secret".into()),
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            None,
        )
        .await
        .unwrap();
    });
    let (server_io, client_io) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        anticrumb::mcp_sdk::Server::new(format!("http://{address}"), "secret".into())
            .await
            .unwrap()
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(client_io).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "gemini_ask"));
    assert!(tools.iter().any(|t| t.name == "gemini_session_get"));
    let templates = client.list_all_resource_templates().await.unwrap();
    assert!(templates.iter().any(|t| {
        t.uri_template == "gemini://artifacts/{account}/{id}"
            && t.description
                .as_deref()
                .is_some_and(|d| d.contains("100 MiB"))
    }));
    let resource = client
        .read_resource(ReadResourceRequestParams::new(format!(
            "gemini://artifacts/default/{}",
            stored.id
        )))
        .await
        .unwrap();
    assert_eq!(resource.contents.len(), 1);
    match &resource.contents[0] {
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => {
            assert_eq!(uri, &format!("gemini://artifacts/default/{}", stored.id));
            assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .unwrap();
            assert_eq!(decoded, payload);
        }
        other => panic!("expected blob resource contents, got {other:?}"),
    }
    let result = client
        .call_tool(
            CallToolRequestParams::new("gemini_ask").with_arguments(
                json!({
                    "prompt": "mcp roundtrip",
                    "session_mode": "temporary",
                    "session_retention": "retain"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let payload_str = serde_json::to_string(&result).unwrap();
    assert!(payload_str.contains("mcp roundtrip"));
    let sid = result
        .structured_content
        .as_ref()
        .and_then(|v| v["session_id"].as_str())
        .expect("mcp gemini_ask returns session_id")
        .to_owned();
    let inspected = client
        .call_tool(
            CallToolRequestParams::new("gemini_session_get")
                .with_arguments(json!({"session_id": sid}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(inspected.is_error, Some(true));
    let inspected_json = inspected
        .structured_content
        .as_ref()
        .expect("gemini_session_get returns structured session");
    assert_eq!(inspected_json["mode"], "temporary");
    assert_eq!(inspected_json["state"], "ready");
    assert_eq!(inspected_json["continuable"], true);
    client.cancel().await.unwrap();
    server.await.unwrap();
    http.abort();
}

#[tokio::test]
async fn final_storage_failure_is_terminal_and_stops_new_work() {
    let (dir, service, _) = fixture();
    let database = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    database.execute_batch("CREATE TRIGGER fail_finish BEFORE UPDATE ON objects WHEN NEW.kind='run' AND json_extract(NEW.payload,'$.state')='completed' BEGIN SELECT RAISE(FAIL,'injected disk full'); END;").unwrap();
    let id = service.submit(TurnRequest::text("storage test")).unwrap();
    let run = tokio::time::timeout(Duration::from_secs(5), service.wait(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.state, "failed");
    assert_eq!(run.error.unwrap().code, ErrorCode::StorageFailure);
    assert_eq!(
        service
            .submit(TurnRequest::text("must not submit"))
            .unwrap_err()
            .code,
        ErrorCode::StorageFailure
    );
}

#[tokio::test]
async fn shutdown_cancels_queued_and_active_runs_and_rejects_new_work() {
    let (_dir, service, _) = fixture();
    let first = service.submit(TurnRequest::text("first")).unwrap();
    let second = service.submit(TurnRequest::text("queued")).unwrap();
    service.begin_shutdown();
    assert!(service.submit(TurnRequest::text("new")).is_err());
    for id in [first, second] {
        let run = service.wait(&id).await.unwrap();
        assert_eq!(run.state, "cancelled");
    }
}

#[tokio::test]
async fn provider_cache_requires_exact_scope_contract_and_complete_prefix() {
    let (_dir, service, _) = fixture();
    let mut request = TurnRequest::text("original");
    request.reuse_scope = Some("client_a".into());
    let id = service.submit(request.clone()).unwrap();
    let first = service.wait(&id).await.unwrap().result.unwrap();
    request.messages.push(Message {
        role: Role::Assistant,
        blocks: vec![Block::Text { text: first.text }],
    });
    request.messages.push(Message::user("followup"));
    let fork = request.clone();
    let id = service.submit(request).unwrap();
    let second = service.wait(&id).await.unwrap().result.unwrap();
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(second.text, "followup");
    assert!(service
        .store
        .events(&id, 0)
        .unwrap()
        .iter()
        .any(|e| e.kind == "cache_hit"));
    // The old prefix now names a branch, not the current head: create a distinct conversation.
    let id = service.submit(fork.clone()).unwrap();
    let branch = service.wait(&id).await.unwrap().result.unwrap();
    assert_ne!(branch.session_id, first.session_id);
    let mut isolated = fork;
    isolated.reuse_scope = Some("client_b".into());
    let id = service.submit(isolated).unwrap();
    let other = service.wait(&id).await.unwrap().result.unwrap();
    assert_ne!(other.session_id, branch.session_id);
}

#[tokio::test]
async fn outcome_unknown_sessions_do_not_exhaust_retained_session_cap() {
    let (_dir, service, _) = fixture();
    for i in 0..8 {
        let mut session = Session {
            id: format!("session_unknown_{i}"),
            provider: default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Chat,
            target_id: None,
            remote_url: None,
            remote_session_id: None,
            history: vec![],
            state: SessionState::OutcomeUnknown,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 0,
            last_used: now_ms(),
            reuse_scope: None,
        };
        if i == 0 {
            session.mode = SessionMode::Temporary;
        }
        service
            .store
            .put("local", "session", &session.id, &session)
            .unwrap();
    }
    let id = service.submit(TurnRequest::text("still works")).unwrap();
    let run = service.wait(&id).await.unwrap();
    assert_eq!(run.state, "completed");

    for i in 0..8 {
        let session = Session {
            id: format!("session_ready_{i}"),
            provider: default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Chat,
            target_id: None,
            remote_url: None,
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: now_ms(),
            reuse_scope: None,
        };
        service
            .store
            .put("local", "session", &session.id, &session)
            .unwrap();
    }
    // Multiple workspaces can each retain their own Master session without hitting an artificial cap.
    let id = service
        .submit(TurnRequest::text("multi-workspace works"))
        .unwrap();
    let run = service.wait(&id).await.unwrap();
    assert_eq!(run.state, "completed");
}

#[tokio::test]
async fn web_backend_routes_turns_by_account_provider_and_rejects_unregistered_providers() {
    use anticrumb::provider::{
        Provider, ProviderCapabilities, ProviderContext, ProviderInfo, ProviderRegistry,
    };

    struct StubProvider {
        id: &'static str,
    }
    impl Provider for StubProvider {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: self.id.into(),
                name: self.id.into(),
                version: "v1".into(),
                kind: "stub".into(),
                entry_url: None,
                default_model: format!("{}-default", self.id),
                source: "runtime".into(),
                capabilities: ProviderCapabilities::default(),
                inline_prompt_chars: anticrumb::provider::DEFAULT_INLINE_PROMPT_CHARS,
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
            request: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            let prompt = match &request.messages[0].blocks[0] {
                Block::Text { text } => text.as_str(),
                _ => "",
            };
            Ok(TurnResult {
                session_id: session.id.clone(),
                text: format!("{}:{}", self.id, prompt),
                tool_calls: vec![],
                artifacts: vec![],
                model: Some(format!("{}-default", self.id)),
                elapsed_ms: 1,
            })
        }
        fn close_session(&self, _: &ProviderContext<'_>, _: &Account, _: &Session) -> Result<()> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let providers = ProviderRegistry::new();
    providers
        .register(Arc::new(StubProvider { id: "alpha" }))
        .unwrap();
    providers
        .register(Arc::new(StubProvider { id: "beta" }))
        .unwrap();

    for (acct_id, prov_id, port) in [("acct_a", "alpha", 9222), ("acct_b", "beta", 9223)] {
        store
            .put(
                "local",
                "account",
                acct_id,
                &Account {
                    id: acct_id.into(),
                    provider: prov_id.into(),
                    profile: dir.path().join(acct_id),
                    subprofile: None,
                    port,
                    headless: true,
                    state: "ready".into(),
                },
            )
            .unwrap();
    }

    let service = Service::new(
        store.clone(),
        Arc::new(anticrumb::service::WebBackend::with_providers(
            store,
            providers.clone(),
        )),
    )
    .unwrap();

    let mut req_a = TurnRequest::text("ping");
    req_a.account_id = "acct_a".into();
    let run_a = service.wait(&service.submit(req_a).unwrap()).await.unwrap();
    assert_eq!(run_a.result.unwrap().text, "alpha:ping");

    let mut req_b = TurnRequest::text("pong");
    req_b.account_id = "acct_b".into();
    let run_b = service.wait(&service.submit(req_b).unwrap()).await.unwrap();
    assert_eq!(run_b.result.unwrap().text, "beta:pong");

    providers.unregister("beta").unwrap();
    let mut req_b2 = TurnRequest::text("after-unregister");
    req_b2.account_id = "acct_b".into();
    let run_b2 = service
        .wait(&service.submit(req_b2).unwrap())
        .await
        .unwrap();
    assert_eq!(run_b2.state, "failed");
    assert_eq!(run_b2.error.unwrap().code, ErrorCode::NotFound);
}
