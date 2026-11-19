//! Async background worker bridging TUI commands to RPC/service calls.
use crate::{
    agent_service::AgentRequest,
    cli::{Client, PromptOptions},
    domain::*,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::{
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
    MentionMatches {
        id: u64,
        query: String,
        matches: Vec<super::mentions::MentionMatch>,
        total: usize,
    },
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
    SearchMentions {
        id: u64,
        workspace: PathBuf,
        query: String,
    },
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

/// Owns the command queue; on exit (or abort) it releases temp files of
/// commands that were queued but never processed.
struct WorkerQueue(mpsc::Receiver<Command>);
impl Drop for WorkerQueue {
    fn drop(&mut self) {
        self.0.close();
        while let Ok(command) = self.0.try_recv() {
            crate::input::cleanup_temp_files(command.temp_files());
        }
    }
}

/// Event polling backs off while a turn is quiet (e.g. waiting on the model)
/// and snaps back to the fast interval as soon as events flow again.
const ACTIVE_POLL_MIN: Duration = Duration::from_millis(25);
const ACTIVE_POLL_MAX: Duration = Duration::from_millis(250);

fn next_active_poll(current: Duration, saw_events: bool) -> Duration {
    if saw_events {
        ACTIVE_POLL_MIN
    } else {
        (current * 2).min(ACTIVE_POLL_MAX)
    }
}

async fn runtime_status(client: &Client) -> Option<Value> {
    tokio::time::timeout(
        Duration::from_secs(3),
        client.rpc.get_status(crate::rpc::pb::Empty {}),
    )
    .await
    .ok()?
    .ok()
    .map(crate::rpc::codec::status_from)
}

fn status_needs_fast_poll(status: &Value) -> bool {
    let account_waiting = status["accounts"].as_array().is_some_and(|accounts| {
        accounts.iter().any(|account| {
            matches!(
                account["state"].as_str(),
                Some("awaiting_human" | "auth_required")
            ) || matches!(
                account["login"]["state"].as_str(),
                Some("waiting" | "refreshing")
            )
        })
    });
    let cleanup_active = status["chat_cleanup"]["pending"].as_u64().unwrap_or(0) > 0
        || status["chat_cleanup"]["running"].as_u64().unwrap_or(0) > 0;
    account_waiting || cleanup_active
}

fn status_poll_interval(active: bool, status_fast: bool, history_active: bool) -> Duration {
    if active || status_fast || history_active {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(3)
    }
}

pub(super) async fn worker(client: Client, rx: mpsc::Receiver<Command>, tx: mpsc::Sender<Update>) {
    let mut queue = WorkerQueue(rx);
    let mut active = None::<String>;
    let mut after = 0;
    let mut last_error = String::new();
    let mut history_poll = None::<String>;
    let mut history_error = String::new();
    let mut status_fast = true;
    let mut next_status_at = Instant::now();
    let mut next_active_at = Instant::now();
    let mut active_interval = ACTIVE_POLL_MIN;

    loop {
        let active_deadline = active.as_ref().map(|_| next_active_at);
        tokio::select! {
            maybe_cmd = queue.0.recv() => {
                let Some(command) = maybe_cmd else {
                    break;
                };
                let result: Result<()> = async {
                    match command {
                        Command::Status => {
                            let status = runtime_status(&client).await;
                            status_fast = status
                                .as_ref()
                                .is_none_or(status_needs_fast_poll);
                            next_status_at = Instant::now()
                                + status_poll_interval(active.is_some(), status_fast, history_poll.is_some());
                            let _ = tx.send(Update::StatusReport(status)).await;
                        }
                        Command::Cache(options) => {
                            if let Err(error) = options.validate() {
                                let _ = tx.send(Update::CacheError(error.to_string())).await;
                            } else {
                                let rpc = client.rpc.clone();
                                let tx = tx.clone();
                                tokio::spawn(async move {
                                    let update = match rpc
                                        .manage_cache(crate::rpc::pb::CacheRequest {
                                            days: options.days,
                                            prune: options.prune,
                                            dry_run: options.dry_run,
                                        })
                                        .await
                                    {
                                        Ok(report) => Update::Cache(report, options),
                                        Err(error) => Update::CacheError(error.to_string()),
                                    };
                                    let _ = tx.send(update).await;
                                });
                            }
                        }
                        Command::Pace(pace) => client.rpc.set_pace(pace).await?,
                        Command::Start(mut request, files, temp_files) => {
                            request.artifacts = client
                                .upload_files_async(&request.account_id, &files, &temp_files)
                                .await?;
                            let id = client.start_agent_async(request).await?;
                            active = Some(id.clone());
                            after = 0;
                            active_interval = ACTIVE_POLL_MIN;
                            next_active_at = Instant::now();
                            let _ = tx.send(Update::Accepted(files.len())).await;
                            let _ = tx.send(Update::Started(id)).await;
                        }
                        Command::Steer {
                            id,
                            message,
                            files,
                            temp_files,
                            account,
                        } => {
                            let artifacts = client
                                .upload_files_async(&account, &files, &temp_files)
                                .await?;
                            client
                                .request_async(
                                    "POST",
                                    &format!("/api/agents/{id}/steer"),
                                    Some(&json!({"message":message,"artifacts":artifacts})),
                                )
                                .await?;
                            let _ = tx.send(Update::Accepted(files.len())).await;
                        }
                        Command::Cancel(id) => {
                            client
                                .request_async(
                                    "POST",
                                    &format!("/api/agents/{id}/cancel"),
                                    Some(&json!({})),
                                )
                                .await?;
                        }
                        Command::BrowserControl(request) => {
                            let response = client.rpc.browser_control(request).await?;
                            let _ = tx
                                .send(Update::BrowserControl(
                                    crate::browser_control::format_response(&response),
                                ))
                                .await;
                        }
                        Command::Browser(account, action) => {
                            let value = if action == "show" || action == "hide" {
                                client
                                    .request_async(
                                        "POST",
                                        &format!("/api/accounts/{account}/browser-mode"),
                                        Some(&json!({"headless": action == "hide"})),
                                    )
                                    .await?
                            } else {
                                if action == "refresh" || action == "login" {
                                    let route = if action == "refresh" {
                                        "resolve"
                                    } else {
                                        "login"
                                    };
                                    client
                                        .request_async(
                                            "POST",
                                            &format!("/api/accounts/{account}/{route}"),
                                            Some(&json!({})),
                                        )
                                        .await?;
                                }
                                client
                                    .request_async(
                                        "GET",
                                        &format!("/api/accounts/{account}/browser"),
                                        None,
                                    )
                                    .await?
                            };
                            if action == "login" {
                                status_fast = true;
                                next_status_at = Instant::now() + Duration::from_millis(500);
                            }
                            let update = if action == "login" {
                                Update::LoginOpened(value)
                            } else {
                                Update::Browser(value)
                            };
                            let _ = tx.send(update).await;
                        }
                        Command::WebUsage(account) => {
                            let client = client.clone();
                            let tx = tx.clone();
                            tokio::spawn(async move {
                                let result = client
                                    .request_async(
                                        "GET",
                                        &format!("/api/accounts/{account}/usage"),
                                        None,
                                    )
                                    .await;
                                let update = match result {
                                    Ok(value) => Update::WebUsage(value),
                                    Err(error) => Update::WebUsageError(error.to_string()),
                                };
                                let _ = tx.send(update).await;
                            });
                        }
                        Command::Models(account, refresh) => {
                            let value = if refresh {
                                client
                                    .request_async(
                                        "POST",
                                        &format!("/api/accounts/{account}/resolve"),
                                        Some(&json!({})),
                                    )
                                    .await?
                            } else {
                                client
                                    .request_async(
                                        "GET",
                                        &format!("/api/accounts/{account}/models"),
                                        None,
                                    )
                                    .await?
                            };
                            let _ = tx.send(Update::Models(value)).await;
                        }
                        Command::Providers(target) => {
                            let value = client.request_async("GET", "/api/status", None).await?;
                            let _ = tx.send(Update::Providers(value, target)).await;
                        }
                        Command::SetupProvider {
                            provider_id,
                            provider_name,
                        } => {
                            let accounts_val =
                                client.request_async("GET", "/api/accounts", None).await?;
                            let accounts =
                                accounts_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
                            let existing_ids: Vec<&str> =
                                accounts.iter().filter_map(|a| a["id"].as_str()).collect();
                            let desired_id =
                                crate::provider::suggest_account_id(&provider_id, &existing_ids);
                            let created = client
                                .request_async(
                                    "POST",
                                    "/api/accounts",
                                    Some(&json!({
                                        "id": desired_id,
                                        "port": 0,
                                        "provider": provider_id
                                    })),
                                )
                                .await?;
                            let account_id =
                                created["id"].as_str().unwrap_or(&desired_id).to_owned();
                            let login_error = client
                                .request_async(
                                    "POST",
                                    &format!("/api/accounts/{account_id}/login"),
                                    Some(&json!({})),
                                )
                                .await
                                .err()
                                .map(|e| e.to_string());
                            let status = client.request_async("GET", "/api/status", None).await?;
                            status_fast = true;
                            let _ = tx
                                .send(Update::ProviderSetup {
                                    status,
                                    account_id,
                                    provider_id,
                                    provider_name,
                                    login_error,
                                })
                                .await;
                        }
                        Command::List(path, account) => {
                            let _ = tx
                                .send(Update::Conversations(
                                    client.conversations_async(&path, &account).await?,
                                ))
                                .await;
                        }
                        Command::DeleteConversation(id, options) => {
                            let update = match client
                                .delete_conversation_async(&id, &options.workspace, &options.account)
                                .await
                            {
                                Ok(()) => Update::ConversationDeleted(id),
                                Err(error) => Update::ConversationDeleteError(error.to_string()),
                            };
                            let _ = tx.send(update).await;
                        }
                        Command::ClearWebHistory(account) => {
                            let update = match client.clear_web_history_async(&account).await {
                                Ok(value) => {
                                    history_poll = Some(account);
                                    history_error.clear();
                                    next_status_at = Instant::now() + Duration::from_secs(1);
                                    Update::WebHistoryQueued(value)
                                }
                                Err(error) => Update::WebHistoryError(error.to_string(), true),
                            };
                            let _ = tx.send(update).await;
                        }
                        Command::WebHistoryStatus(account, announce) => {
                            match client.web_history_cleanup_async(&account).await {
                                Ok(value) => {
                                    history_poll = matches!(
                                        value["state"].as_str(),
                                        Some("pending" | "running")
                                    )
                                    .then_some(account);
                                    history_error.clear();
                                    let _ = tx
                                        .send(Update::WebHistoryStatus(value, announce))
                                        .await;
                                }
                                Err(error) if announce => {
                                    let _ = tx
                                        .send(Update::WebHistoryError(error.to_string(), false))
                                        .await;
                                }
                                Err(_) => {}
                            }
                        }
                        Command::Load(id, mut options) => {
                            let job = client
                                .conversation_async(&id, &options.workspace, &options.account)
                                .await?;
                            options.inherit_from_job(&job)?;
                            if job["state"] == "running" {
                                active = job["id"].as_str().map(str::to_owned);
                                after = 0;
                                active_interval = ACTIVE_POLL_MIN;
                                next_active_at = Instant::now();
                            }
                            let _ = tx.send(Update::Loaded(job)).await;
                        }
                        Command::SearchMentions { .. } => {
                            // Handled by MentionSearchHandle directly; this arm
                            // is kept only for enum exhaustiveness.
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(e) = result {
                    if tx.send(Update::Error(e.to_string())).await.is_err() {
                        break;
                    }
                }
            }
            _ = async {
                match active_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                if let Some(id) = active.clone() {
                    let mut saw_events = false;
                    let result = async {
                        for event in client.events_async(&id, after).await? {
                            saw_events = true;
                            after = event.sequence;
                            if tx.send(Update::Event(event)).await.is_err() {
                                return Err(ServiceError::invalid("terminal closed"));
                            }
                        }
                        // While events stream the turn is still producing output;
                        // check terminal state only once the page comes back empty.
                        if saw_events {
                            return Ok(());
                        }
                        let agent = client.agent_async(&id).await?;
                        if agent.state != "running" {
                            // Drain events written between the first read and terminal state persistence.
                            for event in client.events_async(&id, after).await? {
                                after = event.sequence;
                                let _ = tx.send(Update::Event(event)).await;
                            }
                            active = None;
                            let job = Client::agent_value(agent)?;
                            let _ = tx.send(Update::Finished(job)).await;
                        }
                        Ok(())
                    }
                    .await;
                    active_interval = next_active_poll(active_interval, saw_events);
                    next_active_at = Instant::now() + active_interval;
                    if let Err(e) = result {
                        let message = e.to_string();
                        if message != last_error
                            && tx.try_send(Update::Error(message.clone())).is_ok()
                        {
                            last_error = message;
                        }
                    } else {
                        last_error.clear();
                    }
                }
            }
            _ = tokio::time::sleep_until(next_status_at) => {
                let update = match runtime_status(&client).await {
                    Some(status) => {
                        status_fast = status_needs_fast_poll(&status);
                        Update::Status(status)
                    }
                    None => {
                        status_fast = true;
                        Update::StatusDisconnected
                    }
                };
                let _ = tx.try_send(update);
                if let Some(account) = history_poll.clone() {
                    match client.web_history_cleanup_async(&account).await {
                        Ok(value) => {
                            let finished =
                                !matches!(value["state"].as_str(), Some("pending" | "running"));
                            if tx
                                .try_send(Update::WebHistoryStatus(value, false))
                                .is_ok()
                            {
                                history_error.clear();
                                if finished {
                                    history_poll = None;
                                }
                            }
                        }
                        Err(error) => {
                            let error = error.to_string();
                            if error != history_error
                                && tx
                                    .try_send(Update::WebHistoryError(error.clone(), false))
                                    .is_ok()
                            {
                                history_error = error;
                            }
                        }
                    }
                }
                next_status_at = Instant::now()
                    + status_poll_interval(active.is_some(), status_fast, history_poll.is_some());
            }
        }
    }
    drop(queue);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_poll_backs_off_when_quiet_and_resets_on_events() {
        let mut interval = ACTIVE_POLL_MIN;
        let mut seen = vec![];
        for _ in 0..6 {
            interval = next_active_poll(interval, false);
            seen.push(interval.as_millis());
        }
        assert_eq!(seen, [50, 100, 200, 250, 250, 250]);
        assert_eq!(next_active_poll(interval, true), ACTIVE_POLL_MIN);
    }

    #[test]
    fn status_poll_is_fast_only_while_something_is_pending() {
        assert_eq!(
            status_poll_interval(false, false, false),
            Duration::from_secs(3)
        );
        assert_eq!(
            status_poll_interval(true, false, false),
            Duration::from_secs(1)
        );
        assert_eq!(
            status_poll_interval(false, true, false),
            Duration::from_secs(1)
        );
        assert_eq!(
            status_poll_interval(false, false, true),
            Duration::from_secs(1)
        );
        assert!(status_needs_fast_poll(
            &json!({"chat_cleanup": {"pending": 1}})
        ));
        assert!(status_needs_fast_poll(
            &json!({"accounts": [{"state": "ready", "login": {"state": "waiting"}}]})
        ));
        assert!(!status_needs_fast_poll(
            &json!({"accounts": [{"state": "ready"}], "chat_cleanup": {"pending": 0, "running": 0}})
        ));
    }
}
