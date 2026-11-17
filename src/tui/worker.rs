//! Background worker thread bridging TUI commands to RPC/service calls.
use crate::{
    agent_service::AgentRequest,
    cli::{Client, PromptOptions},
    domain::*,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

pub(super) enum Update {
    Status(Value),
    StatusReport(Option<Value>),
    StatusDisconnected,
    Started(String),
    Event(crate::domain::Event),
    Finished(Value),
    Loaded(Value),
    Conversations(Vec<Value>),
    ConversationDeleted(String),
    ConversationDeleteError(String),
    WebHistoryQueued(Value),
    WebHistoryStatus(Value, bool),
    WebHistoryError(String, bool),
    Models(Value),
    WebUsage(Value),
    WebUsageError(String),
    Cache(crate::rpc::pb::CacheReport, crate::store::cache::Options),
    CacheError(String),
    Browser(Value),
    BrowserControl(String),
    LoginOpened(Value),
    Providers(Value, Option<String>),
    ProviderSetup {
        status: Value,
        account_id: String,
        provider_id: String,
        provider_name: String,
        login_error: Option<String>,
    },
    Accepted(usize),
    Error(String),
}

pub(super) enum Command {
    Status,
    Cache(crate::store::cache::Options),
    Pace(crate::gemini::HumanPace),
    Start(AgentRequest, Vec<PathBuf>, Vec<PathBuf>),
    Steer {
        id: String,
        message: String,
        files: Vec<PathBuf>,
        temp_files: Vec<PathBuf>,
        account: String,
    },
    Cancel(String),
    List(PathBuf, String),
    DeleteConversation(String, PromptOptions),
    ClearWebHistory(String),
    WebHistoryStatus(String, bool),
    Load(String, PromptOptions),
    Models(String, bool),
    Providers(Option<String>),
    SetupProvider {
        provider_id: String,
        provider_name: String,
    },
    WebUsage(String),
    Browser(String, String),
    BrowserControl(crate::rpc::pb::BrowserControlRequest),
}

impl Command {
    pub(super) fn temp_files(&self) -> &[PathBuf] {
        match self {
            Command::Start(_, _, temp_files) => temp_files,
            Command::Steer { temp_files, .. } => temp_files,
            _ => &[],
        }
    }
}

struct WorkerQueueGuard<'a>(&'a mpsc::Receiver<Command>);
impl Drop for WorkerQueueGuard<'_> {
    fn drop(&mut self) {
        for command in self.0.try_iter() {
            crate::input::cleanup_temp_files(command.temp_files());
        }
    }
}

fn runtime_status(client: &Client) -> Option<Value> {
    client.runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(3),
            client.rpc.get_status(crate::rpc::pb::Empty {}),
        )
        .await
        .ok()?
        .ok()
        .map(crate::rpc::codec::status_from)
    })
}

pub(super) fn worker(client: Client, rx: mpsc::Receiver<Command>, tx: mpsc::SyncSender<Update>) {
    let _queue_guard = WorkerQueueGuard(&rx);
    let mut active = None::<String>;
    let mut after = 0;
    let mut last = Instant::now() - Duration::from_secs(5);
    let mut last_error = String::new();
    let mut history_poll = None::<String>;
    let mut history_error = String::new();
    loop {
        let timeout = if active.is_some() {
            Duration::from_millis(25)
        } else {
            Duration::from_millis(100)
        };
        match rx.recv_timeout(timeout) {
            Ok(command) => {
                let result = (|| -> Result<()> {
                    match command {
                        Command::Status => {
                            let _ = tx.send(Update::StatusReport(runtime_status(&client)));
                        }
                        Command::Cache(options) => {
                            let client = client.clone();
                            let tx = tx.clone();
                            std::thread::spawn(move || {
                                let update = match client.cache(options) {
                                    Ok(report) => Update::Cache(report, options),
                                    Err(error) => Update::CacheError(error.to_string()),
                                };
                                let _ = tx.send(update);
                            });
                        }
                        Command::Pace(pace) => client.set_pace(pace)?,
                        Command::Start(mut request, files, temp_files) => {
                            request.artifacts =
                                client.upload_files(&request.account_id, &files, &temp_files)?;
                            let value =
                                client.request("POST", "/api/agents", Some(&json!(request)))?;
                            let id = value["id"]
                                .as_str()
                                .ok_or_else(|| ServiceError::invalid("missing job ID"))?
                                .to_owned();
                            active = Some(id.clone());
                            after = 0;
                            let _ = tx.send(Update::Accepted(files.len()));
                            let _ = tx.send(Update::Started(id));
                        }
                        Command::Steer {
                            id,
                            message,
                            files,
                            temp_files,
                            account,
                        } => {
                            let artifacts = client.upload_files(&account, &files, &temp_files)?;
                            client.request(
                                "POST",
                                &format!("/api/agents/{id}/steer"),
                                Some(&json!({"message":message,"artifacts":artifacts})),
                            )?;
                            let _ = tx.send(Update::Accepted(files.len()));
                        }
                        Command::Cancel(id) => {
                            client.request(
                                "POST",
                                &format!("/api/agents/{id}/cancel"),
                                Some(&json!({})),
                            )?;
                        }
                        Command::BrowserControl(request) => {
                            let response = client
                                .browser_control(request, &crate::agent::CancelToken::new())?;
                            let _ = tx.send(Update::BrowserControl(
                                crate::browser_control::format_response(&response),
                            ));
                        }
                        Command::Browser(account, action) => {
                            let value = if action == "show" || action == "hide" {
                                client.request(
                                    "POST",
                                    &format!("/api/accounts/{account}/browser-mode"),
                                    Some(&json!({"headless": action == "hide"})),
                                )?
                            } else {
                                if action == "refresh" || action == "login" {
                                    let route = if action == "refresh" {
                                        "resolve"
                                    } else {
                                        "login"
                                    };
                                    client.request(
                                        "POST",
                                        &format!("/api/accounts/{account}/{route}"),
                                        Some(&json!({})),
                                    )?;
                                }
                                client.request(
                                    "GET",
                                    &format!("/api/accounts/{account}/browser"),
                                    None,
                                )?
                            };
                            let update = if action == "login" {
                                Update::LoginOpened(value)
                            } else {
                                Update::Browser(value)
                            };
                            let _ = tx.send(update);
                        }
                        Command::WebUsage(account) => {
                            let client = client.clone();
                            let tx = tx.clone();
                            std::thread::spawn(move || {
                                let result = client.request(
                                    "GET",
                                    &format!("/api/accounts/{account}/usage"),
                                    None,
                                );
                                let update = match result {
                                    Ok(value) => Update::WebUsage(value),
                                    Err(error) => Update::WebUsageError(error.to_string()),
                                };
                                let _ = tx.send(update);
                            });
                        }
                        Command::Models(account, refresh) => {
                            let value = if refresh {
                                client.request(
                                    "POST",
                                    &format!("/api/accounts/{account}/resolve"),
                                    Some(&json!({})),
                                )?
                            } else {
                                client.request(
                                    "GET",
                                    &format!("/api/accounts/{account}/models"),
                                    None,
                                )?
                            };
                            let _ = tx.send(Update::Models(value));
                        }
                        Command::Providers(target) => {
                            let value = client.request("GET", "/api/status", None)?;
                            let _ = tx.send(Update::Providers(value, target));
                        }
                        Command::SetupProvider {
                            provider_id,
                            provider_name,
                        } => {
                            let accounts_val = client.request("GET", "/api/accounts", None)?;
                            let accounts =
                                accounts_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
                            let existing_ids: Vec<&str> =
                                accounts.iter().filter_map(|a| a["id"].as_str()).collect();
                            let desired_id =
                                crate::provider::suggest_account_id(&provider_id, &existing_ids);
                            let created = client.request(
                                "POST",
                                "/api/accounts",
                                Some(&json!({
                                    "id": desired_id,
                                    "port": 0,
                                    "provider": provider_id
                                })),
                            )?;
                            let account_id =
                                created["id"].as_str().unwrap_or(&desired_id).to_owned();
                            let login_error = client
                                .request(
                                    "POST",
                                    &format!("/api/accounts/{account_id}/login"),
                                    Some(&json!({})),
                                )
                                .err()
                                .map(|e| e.to_string());
                            let status = client.request("GET", "/api/status", None)?;
                            let _ = tx.send(Update::ProviderSetup {
                                status,
                                account_id,
                                provider_id,
                                provider_name,
                                login_error,
                            });
                        }
                        Command::List(path, account) => {
                            let _ = tx.send(Update::Conversations(
                                client.conversations(&path, &account)?,
                            ));
                        }
                        Command::DeleteConversation(id, options) => {
                            let update = match client.delete_conversation(
                                &id,
                                &options.workspace,
                                &options.account,
                            ) {
                                Ok(()) => Update::ConversationDeleted(id),
                                Err(error) => Update::ConversationDeleteError(error.to_string()),
                            };
                            let _ = tx.send(update);
                        }
                        Command::ClearWebHistory(account) => {
                            let update = match client.clear_web_history(&account) {
                                Ok(value) => {
                                    history_poll = Some(account);
                                    history_error.clear();
                                    Update::WebHistoryQueued(value)
                                }
                                Err(error) => Update::WebHistoryError(error.to_string(), true),
                            };
                            let _ = tx.send(update);
                        }
                        Command::WebHistoryStatus(account, announce) => {
                            match client.web_history_cleanup(&account) {
                                Ok(value) => {
                                    history_poll = matches!(
                                        value["state"].as_str(),
                                        Some("pending" | "running")
                                    )
                                    .then_some(account);
                                    history_error.clear();
                                    let _ = tx.send(Update::WebHistoryStatus(value, announce));
                                }
                                Err(error) if announce => {
                                    let _ =
                                        tx.send(Update::WebHistoryError(error.to_string(), false));
                                }
                                Err(_) => {}
                            }
                        }
                        Command::Load(id, mut options) => {
                            let job =
                                client.conversation(&id, &options.workspace, &options.account)?;
                            options.inherit_from_job(&job)?;
                            if job["state"] == "running" {
                                active = job["id"].as_str().map(str::to_owned);
                                after = 0;
                            }
                            let _ = tx.send(Update::Loaded(job));
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    if tx.send(Update::Error(e.to_string())).is_err() {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(id) = active.clone() {
            let result = (|| -> Result<()> {
                for event in client.events(&id, after)? {
                    after = event.sequence;
                    if tx.send(Update::Event(event)).is_err() {
                        return Err(ServiceError::invalid("terminal closed"));
                    }
                }
                let job = client.request("GET", &format!("/api/agents/{id}"), None)?;
                if job["state"] != "running" {
                    // Drain events written between the first read and terminal state persistence.
                    for event in client.events(&id, after)? {
                        after = event.sequence;
                        let _ = tx.send(Update::Event(event));
                    }
                    active = None;
                    let _ = tx.send(Update::Finished(job));
                }
                Ok(())
            })();
            if let Err(e) = result {
                let message = e.to_string();
                if message != last_error {
                    let _ = tx.try_send(Update::Error(message.clone()));
                    last_error = message;
                }
            } else {
                last_error.clear();
            }
        }
        if last.elapsed() > Duration::from_secs(1) {
            last = Instant::now();
            let update = match runtime_status(&client) {
                Some(status) => Update::Status(status),
                None => Update::StatusDisconnected,
            };
            let _ = tx.try_send(update);
            if let Some(account) = history_poll.clone() {
                match client.web_history_cleanup(&account) {
                    Ok(value) => {
                        history_error.clear();
                        if !matches!(value["state"].as_str(), Some("pending" | "running")) {
                            history_poll = None;
                        }
                        let _ = tx.try_send(Update::WebHistoryStatus(value, false));
                    }
                    Err(error) => {
                        let error = error.to_string();
                        if error != history_error {
                            let _ = tx.try_send(Update::WebHistoryError(error.clone(), false));
                            history_error = error;
                        }
                    }
                }
            }
        }
    }
}
