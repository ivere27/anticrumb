//! PTY fixture with its TUI and isolated gRPC server in one process. No Chrome or tools.
use anticrumb::{
    agent::CancelToken,
    cli::{Client, PromptOptions},
    domain::*,
    rpc::{codec, host::Host, pb, server::GrpcApi},
    service::{Backend, Emit, Service},
    store::Store,
};
use pb::{
    account_service_server::AccountService, artifact_service_server::ArtifactService,
    model_service_server::ModelService, workspace_service_server::WorkspaceService,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tonic::{Request, Response, Status};
type Reply<T> = std::result::Result<Response<T>, Status>;
struct NoBrowser;
impl Backend for NoBrowser {
    fn generate(
        &self,
        _: &Account,
        _: &mut Session,
        _: &TurnRequest,
        _: CancelToken,
        _: Emit<'_>,
    ) -> Result<TurnResult> {
        panic!("fixture must not generate")
    }
    fn close(&self, _: &Account, _: &Session) -> Result<()> {
        Ok(())
    }
}
struct State {
    observations: Value,
    jobs: Vec<pb::Agent>,
    catalog: pb::ModelCatalog,
    browser_headless: Option<bool>,
}
#[derive(Clone)]
struct Fixture {
    api: GrpcApi,
    host: Arc<Host>,
    state: Arc<Mutex<State>>,
    observations_path: PathBuf,
}
impl Fixture {
    fn save(&self, state: &State) {
        let temp = self.observations_path.with_extension("tmp");
        let mut observations = state.observations.clone();
        observations["saved_agents"] = json!(state.jobs);
        std::fs::write(&temp, serde_json::to_vec(&observations).unwrap()).unwrap();
        std::fs::rename(temp, &self.observations_path).unwrap();
    }
    fn browser_status_reply(
        &self,
        account_id: String,
        headless: Option<bool>,
    ) -> pb::BrowserStatus {
        pb::BrowserStatus {
            account_id,
            account_state: "ready".into(),
            connected: true,
            endpoint: "http://127.0.0.1:9222".into(),
            browser: Some("Chrome/fixture".into()),
            profile: "fixture/profile".into(),
            profile_matches: Some(true),
            headless,
            last_verified_sign_in: Some(true),
            checked_at: Some(1),
            login: self
                .host
                .service
                .store
                .get::<Value>("default", "login", "current")
                .ok()
                .and_then(|value| serde_json::from_value(value).ok()),
            ..Default::default()
        }
    }
    fn cache_models(&self, state: &mut State) {
        state.observations["model_probes"] =
            json!(state.observations["model_probes"].as_u64().unwrap() + 1);
        self.host
            .service
            .store
            .put(
                "default",
                "catalog",
                "models",
                &codec::catalog_from(state.catalog.clone()),
            )
            .unwrap();
        self.save(state);
    }
}
#[tonic::async_trait]
impl AccountService for Fixture {
    async fn clear_web_history(
        &self,
        r: Request<pb::AccountRef>,
    ) -> Reply<pb::WebHistoryCleanupStatus> {
        self.api.authorize(r.metadata())?;
        let account_id = r.into_inner().account_id;
        if self
            .observations_path
            .parent()
            .unwrap()
            .join("reject-clear-history")
            .exists()
        {
            return Err(Status::failed_precondition(
                "fixture history cleanup failed",
            ));
        }
        let mut state = self.state.lock().unwrap();
        state.observations["history_clear_requests"]
            .as_array_mut()
            .unwrap()
            .push(json!(&account_id));
        let value = pb::WebHistoryCleanupStatus {
            account_id,
            state: "pending".into(),
            started_at: now_ms(),
            updated_at: now_ms(),
            ..Default::default()
        };
        state.observations["history_cleanup"] = json!(&value);
        self.save(&state);
        Ok(Response::new(value))
    }
    async fn get_web_history_cleanup(
        &self,
        r: Request<pb::AccountRef>,
    ) -> Reply<pb::WebHistoryCleanupStatus> {
        self.api.authorize(r.metadata())?;
        let account_id = r.into_inner().account_id;
        let mut state = self.state.lock().unwrap();
        let mut value: pb::WebHistoryCleanupStatus = serde_json::from_value(
            state.observations["history_cleanup"].clone(),
        )
        .unwrap_or_else(|_| pb::WebHistoryCleanupStatus {
            account_id,
            state: "idle".into(),
            ..Default::default()
        });
        match value.state.as_str() {
            "pending" => {
                value.state = "running".into();
                value.deleted = 1;
            }
            "running" => {
                value.state = "completed".into();
                value.deleted = 2;
            }
            _ => {}
        }
        value.updated_at = now_ms();
        state.observations["history_cleanup"] = json!(&value);
        self.save(&state);
        Ok(Response::new(value))
    }
    async fn browser_fetch(
        &self,
        r: Request<pb::BrowserFetchRequest>,
    ) -> Reply<pb::BrowserFetchResponse> {
        self.api.authorize(r.metadata())?;
        let req = r.into_inner();
        Ok(Response::new(pb::BrowserFetchResponse {
            url: req.url,
            title: "Fixture Page".into(),
            text: "Fixture browser fetch output".into(),
        }))
    }
    async fn browser_control(
        &self,
        r: Request<pb::BrowserControlRequest>,
    ) -> Reply<pb::BrowserControlResponse> {
        self.api.authorize(r.metadata())?;
        let req = r.into_inner();
        Ok(Response::new(pb::BrowserControlResponse {
            session_id: req.session_id,
            active_tab_id: "tab_fixture".into(),
            page: Some(pb::BrowserSnapshot {
                tab_id: "tab_fixture".into(),
                url: "https://example.com".into(),
                title: "Fixture Controlled Page".into(),
                text: "Fixture browser control output".into(),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
    async fn get_web_usage(&self, r: Request<pb::AccountRef>) -> Reply<pb::WebUsage> {
        self.api.authorize(r.metadata())?;
        let account_id = r.into_inner().account_id;
        let mut state = self.state.lock().unwrap();
        state.observations["web_usage_queries"] = json!(
            state.observations["web_usage_queries"]
                .as_u64()
                .unwrap_or(0)
                + 1
        );
        self.save(&state);
        Ok(Response::new(pb::WebUsage {
            account_id,
            checked_at: now_ms(),
            plan: Some("Fixture plan".into()),
            timezone: "Asia/Seoul".into(),
            updated_label: "Updated just now".into(),
            limits: vec![
                pb::WebUsageLimit {
                    label: "Current usage".into(),
                    used: Some("25% used".into()),
                    used_percent: Some(25.0),
                    reset: Some("Resets at 5:41 AM".into()),
                    ..Default::default()
                },
                pb::WebUsageLimit {
                    label: "Weekly limit".into(),
                    used: Some("0% used".into()),
                    used_percent: Some(0.0),
                    reset: Some("Resets Nov 20 at 3:41 PM".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }))
    }
    async fn list_accounts(&self, r: Request<pb::Empty>) -> Reply<pb::AccountList> {
        self.api.list_accounts(r).await
    }
    async fn create_account(&self, r: Request<pb::CreateAccountRequest>) -> Reply<pb::Account> {
        self.api.create_account(r).await
    }
    async fn attach_browser(
        &self,
        _: Request<pb::AttachBrowserRequest>,
    ) -> Reply<pb::BrowserStatus> {
        Err(Status::unimplemented("fixture"))
    }
    async fn open_login(&self, r: Request<pb::AccountRef>) -> Reply<pb::Empty> {
        self.api.authorize(r.metadata())?;
        assert_eq!(r.into_inner().account_id, "default");
        let store = &self.host.service.store;
        let mut account: Account = store.get("local", "account", "default").unwrap();
        account.state = "awaiting_human".into();
        store.put("local", "account", "default", &account).unwrap();
        store
            .put(
                "default",
                "login",
                "current",
                &json!({"state":"waiting","error":""}),
            )
            .unwrap();
        let fixture = self.clone();
        tokio::spawn(async move {
            let completed = fixture
                .observations_path
                .parent()
                .unwrap()
                .join("login-complete");
            for _ in 0..800 {
                if completed.exists() {
                    let store = &fixture.host.service.store;
                    fixture.cache_models(&mut fixture.state.lock().unwrap());
                    account.state = "ready".into();
                    store.put("local", "account", "default", &account).unwrap();
                    store
                        .put(
                            "default",
                            "login",
                            "current",
                            &json!({"state":"ready","error":""}),
                        )
                        .unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });
        Ok(Response::new(pb::Empty {}))
    }
    async fn pause_account(&self, r: Request<pb::AccountRef>) -> Reply<pb::Account> {
        self.api.pause_account(r).await
    }
    async fn set_browser_mode(
        &self,
        r: Request<pb::SetBrowserModeRequest>,
    ) -> Reply<pb::BrowserStatus> {
        self.api.authorize(r.metadata())?;
        let req = r.into_inner();
        let mut state = self.state.lock().unwrap();
        state.browser_headless = Some(req.headless);
        state.observations["browser_mode_requests"]
            .as_array_mut()
            .unwrap()
            .push(json!({"account_id": req.account_id, "headless": req.headless}));
        self.save(&state);
        Ok(Response::new(self.browser_status_reply(
            req.account_id,
            state.browser_headless,
        )))
    }
    async fn get_browser_status(&self, r: Request<pb::AccountRef>) -> Reply<pb::BrowserStatus> {
        self.api.authorize(r.metadata())?;
        let headless = self.state.lock().unwrap().browser_headless;
        Ok(Response::new(
            self.browser_status_reply(r.into_inner().account_id, headless),
        ))
    }
}
#[tonic::async_trait]
impl ModelService for Fixture {
    async fn get_catalog(&self, r: Request<pb::AccountRef>) -> Reply<pb::ModelCatalog> {
        self.api.get_catalog(r).await
    }
    async fn refresh_catalog(&self, r: Request<pb::AccountRef>) -> Reply<pb::ModelCatalog> {
        self.api.authorize(r.metadata())?;
        let mut state = self.state.lock().unwrap();
        self.cache_models(&mut state);
        Ok(Response::new(state.catalog.clone()))
    }
}
#[tonic::async_trait]
impl ArtifactService for Fixture {
    type GetArtifactStream = anticrumb::rpc::server::TypedStream<pb::GetArtifactChunk>;

    async fn put_artifact(
        &self,
        r: Request<tonic::Streaming<pb::PutArtifactChunk>>,
    ) -> Reply<pb::Artifact> {
        let response = self.api.put_artifact(r).await?;
        let artifact = response.get_ref();
        let (_, bytes) = self
            .host
            .service
            .store
            .artifact_bytes(&artifact.scope, &artifact.id)
            .map_err(codec::status)?;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let mut state = self.state.lock().unwrap();
        state.observations["uploads"]
            .as_array_mut()
            .unwrap()
            .push(json!(content));
        state.observations["uploaded_ids"]
            .as_array_mut()
            .unwrap()
            .push(json!(artifact.id));
        self.save(&state);
        Ok(response)
    }
    async fn get_artifact(&self, r: Request<pb::ArtifactRef>) -> Reply<Self::GetArtifactStream> {
        self.api.get_artifact(r).await
    }
}
fn job_conversation_id(job: &pb::Agent) -> &str {
    if !job.conversation_id.is_empty() {
        &job.conversation_id
    } else {
        &job.id
    }
}
fn build_conversation(conv_id: &str, mut turns: Vec<pb::Agent>) -> pb::Conversation {
    turns.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let first = &turns[0];
    let latest = turns.last().unwrap();
    let first_req = first.request.as_ref();
    let latest_req = latest.request.as_ref();
    let created_at = turns.iter().map(|t| t.created_at).min().unwrap_or(0);
    let updated_at = turns
        .iter()
        .map(|t| t.updated_at.max(t.created_at))
        .max()
        .unwrap_or(0);
    let any_temp = turns.iter().any(|t| {
        t.request
            .as_ref()
            .is_some_and(|r| r.session_mode == pb::SessionMode::Temporary as i32)
    });
    let session_mode = if any_temp {
        pb::SessionMode::Temporary as i32
    } else {
        pb::SessionMode::Chat as i32
    };
    let summary = pb::ConversationSummary {
        id: conv_id.to_owned(),
        latest_agent_id: latest.id.clone(),
        workspace: latest_req.map(|r| r.workspace.clone()).unwrap_or_default(),
        account_id: latest_req
            .map(|r| r.account_id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".into()),
        state: latest.state.clone(),
        session_mode,
        title: first_req.map(|r| r.task.clone()).unwrap_or_default(),
        turn_count: turns.len() as u32,
        created_at,
        updated_at,
        model: latest_req.and_then(|r| r.model.clone()),
        thinking: latest_req.and_then(|r| r.thinking.clone()),
        chat_only: latest_req.is_some_and(|r| r.chat_only),
    };
    pb::Conversation {
        summary: Some(summary),
        turns,
    }
}
#[tonic::async_trait]
impl WorkspaceService for Fixture {
    async fn connect_client(&self, r: Request<pb::ConnectClientRequest>) -> Reply<pb::HostInfo> {
        self.api.connect_client(r).await
    }
    async fn start_agent(&self, r: Request<pb::AgentRequest>) -> Reply<pb::StartAgentResponse> {
        self.api.authorize(r.metadata())?;
        let mut request = r.into_inner();
        request.environment.clear();
        let observed_request = codec::agent_request_from(request.clone()).map_err(codec::status)?;
        let mut state = self.state.lock().unwrap();
        state.observations["tasks"]
            .as_array_mut()
            .unwrap()
            .push(json!(&observed_request));
        let n = state.observations["tasks"].as_array().unwrap().len();
        let id = if n == 1 {
            "agent_fixture".to_owned()
        } else {
            format!("agent_fixture_{n}")
        };
        let conversation_id = request
            .conversation_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if n == 1 {
                    "conv_fixture".to_owned()
                } else {
                    format!("conv_fixture_{n}")
                }
            });
        request.conversation_id = Some(conversation_id.clone());
        state.jobs.push(pb::Agent {
            id: id.clone(),
            conversation_id: conversation_id.clone(),
            state: "completed".into(),
            request: Some(request),
            result: Some(pb::AgentResult {
                status: "completed".into(),
                final_response: format!("Attachment turn {n} completed.\n\n**Important result** with *emphasis* and `file_name.rs`.\n```rust\nlet answer = 42;\n```\n\n| Item | State |\n| :--- | ---: |\n| **\u{754c}\u{8a9e}** | `ready` |\n| test | 42 |"),
                ..Default::default()
            }),
            created_at: n as i64,
            updated_at: n as i64,
            ..Default::default()
        });
        self.save(&state);
        Ok(Response::new(pb::StartAgentResponse {
            id,
            conversation_id,
        }))
    }
    async fn get_agent(&self, r: Request<pb::AgentRef>) -> Reply<pb::Agent> {
        self.api.authorize(r.metadata())?;
        let id = r.into_inner().id;
        self.state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .cloned()
            .map(Response::new)
            .ok_or_else(|| Status::not_found("no job"))
    }
    async fn list_agents(&self, r: Request<pb::Empty>) -> Reply<pb::AgentList> {
        self.api.authorize(r.metadata())?;
        Ok(Response::new(pb::AgentList {
            agents: self.state.lock().unwrap().jobs.clone(),
        }))
    }
    async fn get_conversation(&self, r: Request<pb::ConversationRef>) -> Reply<pb::Conversation> {
        self.api.authorize(r.metadata())?;
        let id = r.into_inner().id;
        let turns: Vec<pb::Agent> = self
            .state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|job| job_conversation_id(job) == id)
            .cloned()
            .collect();
        if turns.is_empty() {
            return Err(Status::not_found("no conversation"));
        }
        Ok(Response::new(build_conversation(&id, turns)))
    }
    async fn list_conversations(
        &self,
        r: Request<pb::ListConversationsRequest>,
    ) -> Reply<pb::ConversationList> {
        self.api.authorize(r.metadata())?;
        let req = r.into_inner();
        let workspace = req.workspace.as_deref().filter(|s| !s.is_empty());
        let account_id = req.account_id.as_deref().filter(|s| !s.is_empty());
        let limit = if req.limit == 0 {
            50
        } else {
            (req.limit as usize).clamp(1, 500)
        };
        let state = self.state.lock().unwrap();
        let mut grouped: std::collections::BTreeMap<String, Vec<pb::Agent>> =
            std::collections::BTreeMap::new();
        for job in &state.jobs {
            if let Some(ws) = workspace {
                if job.request.as_ref().map(|r| r.workspace.as_str()) != Some(ws) {
                    continue;
                }
            }
            if let Some(acct) = account_id {
                let job_acct = job
                    .request
                    .as_ref()
                    .map(|r| r.account_id.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("default");
                if job_acct != acct {
                    continue;
                }
            }
            grouped
                .entry(job_conversation_id(job).to_owned())
                .or_default()
                .push(job.clone());
        }
        let mut conversations: Vec<pb::ConversationSummary> = grouped
            .into_iter()
            .filter_map(|(cid, turns)| build_conversation(&cid, turns).summary)
            .collect();
        conversations.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        });
        conversations.truncate(limit);
        Ok(Response::new(pb::ConversationList { conversations }))
    }
    async fn delete_agent_conversation(
        &self,
        r: Request<pb::DeleteConversationRequest>,
    ) -> Reply<pb::Empty> {
        self.api.authorize(r.metadata())?;
        let req = r.into_inner();
        let mut state = self.state.lock().unwrap();
        if self
            .observations_path
            .parent()
            .unwrap()
            .join("reject-delete")
            .exists()
        {
            return Err(Status::failed_precondition("fixture deletion failed"));
        }
        let workspace = req.workspace.as_deref().filter(|s| !s.is_empty());
        let account_id = req.account_id.as_deref().filter(|s| !s.is_empty());
        let matching: Vec<&pb::Agent> = state
            .jobs
            .iter()
            .filter(|job| job_conversation_id(job) == req.id)
            .collect();
        if matching.is_empty() {
            return Err(Status::not_found("no conversation"));
        }
        for job in &matching {
            if matches!(job.state.as_str(), "running" | "queued") {
                return Err(Status::failed_precondition(
                    "interrupt the running task before deleting it",
                ));
            }
            if let Some(ws) = workspace {
                if job.request.as_ref().map(|r| r.workspace.as_str()) != Some(ws) {
                    return Err(Status::invalid_argument(
                        "conversation deletion requires the same workspace and account",
                    ));
                }
            }
            if let Some(acct) = account_id {
                let job_acct = job
                    .request
                    .as_ref()
                    .map(|r| r.account_id.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("default");
                if job_acct != acct {
                    return Err(Status::invalid_argument(
                        "conversation deletion requires the same workspace and account",
                    ));
                }
            }
        }
        state.jobs.retain(|job| job_conversation_id(job) != req.id);
        state.observations["deleted_ids"]
            .as_array_mut()
            .unwrap()
            .push(json!(req.id));
        self.save(&state);
        Ok(Response::new(pb::Empty {}))
    }
    async fn cancel_agent(&self, _: Request<pb::AgentRef>) -> Reply<pb::Empty> {
        Err(Status::unimplemented("fixture"))
    }
    async fn steer_agent(&self, _: Request<pb::SteerRequest>) -> Reply<pb::Empty> {
        Err(Status::unimplemented("fixture"))
    }
    async fn resume_conversation(
        &self,
        _: Request<pb::ConversationRef>,
    ) -> Reply<pb::StartAgentResponse> {
        Err(Status::unimplemented("fixture"))
    }
    async fn read_agent_events(&self, _: Request<pb::EventCursor>) -> Reply<pb::EventPage> {
        Ok(Response::new(pb::EventPage::default()))
    }
    async fn query_knowledge(&self, _: Request<pb::KnowledgeQuery>) -> Reply<pb::KnowledgeResults> {
        Err(Status::unimplemented("fixture"))
    }
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(
        args.first()
            .expect("usage: tui_fixture STATE_DIR [TUI options]"),
    );
    if std::env::var_os("ANTICRUMB_HOME").is_none() {
        std::env::set_var("ANTICRUMB_HOME", root.parent().unwrap_or(&root));
    }
    let options = PromptOptions::parse(&args[1..]).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (host, endpoint, server) = runtime.block_on(start(root));
    let client = Client::at(&endpoint, FIXTURE_TOKEN).unwrap();
    let result = anticrumb::tui::run_with_client(options, client);
    host.shutdown();
    server.abort();
    let _ = runtime.block_on(server);
    drop(host);
    drop(runtime);
    result.unwrap();
}

const FIXTURE_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn start(root: PathBuf) -> (Arc<Host>, String, tokio::task::JoinHandle<()>) {
    let store = Store::open(&root).unwrap();
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
    let host = Host::with_service(
        Service::new(store, Arc::new(NoBrowser)).unwrap(),
        None,
        "workspace",
        None,
    )
    .unwrap();
    let models = json!([
        {"mode_id":"flash","label":"Quick answers","sublabel":"3.5 Flash-Lite"},
        {"mode_id":"hydrogen","label":"Everyday help","sublabel":"4.0 Hydrogen","selected":true},
        {"mode_id":"argon","label":"Code & analysis","sublabel":"4.0 Argon"}
    ]);
    let levels: Vec<Value> = ["Low", "Medium", "High", "Ultra"]
        .iter()
        .enumerate()
        .map(|(i, l)| json!({"level":format!("option:{i}"),"label":l,"selected":i==0}))
        .collect();
    let catalog_record: ModelCatalogRecord = serde_json::from_value(json!({
        "models": models,
        "thinking": levels,
        "model_thinking": {"flash": [], "hydrogen": levels, "argon": levels}
    }))
    .unwrap();
    let catalog = codec::catalog_to("default", &host.server_id, catalog_record);
    let mut observations: Value = std::fs::read(root.join("observations.json"))
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .unwrap_or_else(|| json!({"uploads":[],"uploaded_ids":[],"tasks":[],"model_probes":0}));
    if !observations["deleted_ids"].is_array() {
        observations["deleted_ids"] = json!([]);
    }
    if !observations["history_clear_requests"].is_array() {
        observations["history_clear_requests"] = json!([]);
    }
    if !observations["browser_mode_requests"].is_array() {
        observations["browser_mode_requests"] = json!([]);
    }
    let jobs = serde_json::from_value(observations["saved_agents"].clone()).unwrap_or_default();
    let fixture = Fixture {
        api: GrpcApi::new(host.clone(), Some(FIXTURE_TOKEN.into())),
        host: host.clone(),
        state: Arc::new(Mutex::new(State {
            observations,
            jobs,
            catalog,
            browser_headless: Some(true),
        })),
        observations_path: root.join("observations.json"),
    };
    fixture.save(&fixture.state.lock().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::platform_service_server::PlatformServiceServer::new(
                fixture.api.clone(),
            ))
            .add_service(pb::account_service_server::AccountServiceServer::new(
                fixture.clone(),
            ))
            .add_service(pb::model_service_server::ModelServiceServer::new(
                fixture.clone(),
            ))
            .add_service(pb::artifact_service_server::ArtifactServiceServer::new(
                fixture.clone(),
            ))
            .add_service(pb::workspace_service_server::WorkspaceServiceServer::new(
                fixture,
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (host, endpoint, server)
}
