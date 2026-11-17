//! Bounded CDP transport. A reader dispatches responses and events independently.
use crate::agent::CancelToken;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, mpsc as async_mpsc};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
// Rich-text editors can take more than 30 seconds to process a checkpoint's
// multiline prompt. The enclosing turn deadline and cancellation still apply.
const TEXT_INPUT_TIMEOUT: Duration = Duration::from_secs(120);

type Reply = mpsc::SyncSender<Result<Value, String>>;
type BlockedDocuments = Arc<Mutex<HashMap<String, String>>>;

fn record_document_block(blocks: &BlockedDocuments, frame: Option<String>, reason: String) {
    if let (Some(frame), Ok(mut blocks)) = (frame, blocks.lock()) {
        if blocks.len() < 64 || blocks.contains_key(&frame) {
            blocks.insert(frame, reason);
        }
    }
}

enum Command {
    RestrictNetwork(mpsc::SyncSender<Result<(), String>>),
    Call {
        method: String,
        params: Value,
        reply: Reply,
    },
    Close,
}

pub struct CdpClient {
    tx: async_mpsc::Sender<Command>,
    events: broadcast::Sender<Value>,
    blocked_documents: BlockedDocuments,
    cancel: CancelToken,
    deadline: Option<Instant>,
    invalid: bool,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl CdpClient {
    pub fn connect(port: u16, path: &str) -> Result<Self, String> {
        let (tx, mut rx) = async_mpsc::channel::<Command>(32);
        let (events, _) = broadcast::channel(256);
        let dispatch = events.clone();
        let blocked_documents = BlockedDocuments::default();
        let blocks = blocked_documents.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let url = format!("ws://127.0.0.1:{port}/{}", path.trim_start_matches('/'));
        let page_target = path
            .trim_start_matches('/')
            .strip_prefix("devtools/page/")
            .map(str::to_owned);
        let worker = std::thread::Builder::new().name("cdp-events".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt, Err(e) => { let _ = ready_tx.send(Err(e.to_string())); return; }
            };
            rt.block_on(async move {
                let config = WebSocketConfig::default()
                    .max_message_size(Some(48 * 1024 * 1024)).max_frame_size(Some(48 * 1024 * 1024));
                let socket = tokio::time::timeout(Duration::from_secs(5), connect_async_with_config(url, Some(config), false)).await;
                let (mut socket, _) = match socket {
                    Ok(Ok(ws)) => ws,
                    other => { let _ = ready_tx.send(Err(format!("connect CDP WebSocket: {other:?}"))); return; }
                };
                if ready_tx.send(Ok(())).is_err() { return; }
                let mut pending = HashMap::<u64, Reply>::new();
                let mut next_id = 0u64;
                let mut restricted = false;
                let mut checks = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        checked = checks.join_next(), if !checks.is_empty() => {
                            let Some(Ok((request_id, frame, validation))) = checked else { break };
                            next_id += 1;
                            let (method, params) = match validation {
                                Ok(()) => ("Fetch.continueRequest", json!({"requestId": request_id})),
                                Err(reason) => {
                                    record_document_block(&blocks, frame, reason);
                                    ("Fetch.failRequest", json!({"requestId": request_id, "errorReason": "BlockedByClient"}))
                                }
                            };
                            let msg = Message::Text(json!({"id":next_id,"method":method,"params":params}).to_string().into());
                            if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(msg)).await,Ok(Ok(()))) { break; }
                        }
                        command = rx.recv() => match command {
                            Some(Command::RestrictNetwork(reply)) => {
                                if page_target.is_some() {
                                    restricted = true;
                                    let _ = reply.send(Ok(()));
                                } else {
                                    let _ = reply.send(Err("network restriction requires an owned page target".into()));
                                }
                            }
                            Some(Command::Call { method, params, reply }) => {
                                next_id += 1;
                                let msg = Message::Text(json!({"id":next_id,"method":method,"params":params}).to_string().into());
                                pending.insert(next_id, reply);
                                if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(msg)).await,Ok(Ok(()))) { break; }
                            }
                            Some(Command::Close) | None => break,
                        },
                        message = socket.next() => match message {
                            Some(Ok(Message::Text(text))) => {
                                let value: Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => break };
                                if let Some(id) = value["id"].as_u64() {
                                    if let Some(reply) = pending.remove(&id) {
                                        let result = if value.get("error").is_some() { Err(format!("CDP error: {}", value["error"])) }
                                            else { Ok(value["result"].clone()) };
                                        let _ = reply.send(result);
                                    }
                                } else if restricted && value["method"] == "Fetch.requestPaused" {
                                    let Some(request_id) = value["params"]["requestId"].as_str().map(str::to_owned) else { break };
                                    let url = value["params"]["request"]["url"].as_str().unwrap_or_default().to_owned();
                                    let frame = (value["params"]["resourceType"] == "Document")
                                        .then(|| value["params"]["frameId"].as_str().map(str::to_owned)).flatten();
                                    if checks.len() >= 64 {
                                        // Fail closed instead of retaining an unbounded request queue.
                                        record_document_block(&blocks, frame, "too many concurrent browser network checks (64)".into());
                                        next_id += 1;
                                        let msg = Message::Text(json!({"id":next_id,"method":"Fetch.failRequest","params":{"requestId":request_id,"errorReason":"BlockedByClient"}}).to_string().into());
                                        if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(msg)).await,Ok(Ok(()))) { break; }
                                    } else {
                                        checks.spawn(async move {
                                            let validation = crate::web::validate_public_request(&url).await;
                                            (request_id, frame, validation)
                                        });
                                    }
                                } else { let _ = dispatch.send(value); }
                            }
                            Some(Ok(Message::Ping(_))) => { if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.flush()).await,Ok(Ok(()))) { break; } }
                            Some(Ok(Message::Pong(_))) => {},
                            _ => break,
                        }
                    }
                }
                checks.abort_all();
                if restricted {
                    // Keep Fetch attached until the owned tab is closed. Otherwise
                    // page timers could issue unchecked requests after cancellation
                    // or while a controlled tab waits to reconnect.
                    if let Some(target) = page_target {
                        let _ = tokio::task::spawn_blocking(move || crate::browser_host::close_tab(port, &target)).await;
                    }
                }
                for (_, reply) in pending { let _ = reply.send(Err("CDP WebSocket disconnected".into())); }
            });
        }).map_err(|e| e.to_string())?;
        if let Err(error) = ready_rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        {
            let _ = stop.send(());
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            tx,
            events,
            blocked_documents,
            cancel: CancelToken::new(),
            deadline: None,
            invalid: false,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Check every HTTP request before Chrome sends it, including redirects and
    /// subresources initiated by page scripts. Keep this active across UI actions.
    /// This takes ownership of the page: it is closed before the guard detaches.
    pub fn restrict_to_public_network(&mut self) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Command::RestrictNetwork(tx))
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())??;
        self.call("Network.enable", json!({}))?;
        self.call("Network.setBypassServiceWorker", json!({"bypass": true}))?;
        self.call("Network.setCacheDisabled", json!({"cacheDisabled": true}))?;
        self.call(
            "Fetch.enable",
            json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
        )?;
        Ok(())
    }

    pub fn is_invalid(&self) -> bool {
        self.invalid
    }

    pub fn blocked_navigation_reason(&self, frame: &str) -> Option<String> {
        self.blocked_documents.lock().ok()?.get(frame).cloned()
    }

    pub fn set_control(&mut self, cancel: CancelToken, deadline: Instant) {
        self.cancel = cancel;
        self.deadline = Some(deadline);
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if method == "Page.navigate" {
            if let Ok(mut blocks) = self.blocked_documents.lock() {
                blocks.clear();
            }
        }
        let timeout = if method == "Input.insertText" {
            TEXT_INPUT_TIMEOUT
        } else {
            CALL_TIMEOUT
        };
        self.call_with_timeout(method, params, timeout)
    }

    fn call_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.invalid {
            return Err("CDP connection invalidated; reconnect before reuse".into());
        }
        let started = Instant::now();
        let deadline = self
            .deadline
            .unwrap_or(started + timeout)
            .min(started + timeout);
        let deadline_error = || {
            format!(
                "CDP deadline exceeded: {method} after {:.3}s (budget {:.3}s)",
                started.elapsed().as_secs_f64(),
                deadline.saturating_duration_since(started).as_secs_f64(),
            )
        };
        self.cancel.check()?;
        if Instant::now() >= deadline {
            return Err(deadline_error());
        }
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Command::Call {
                method: method.into(),
                params,
                reply,
            })
            .map_err(|e| e.to_string())?;
        loop {
            if self.cancel.is_cancelled() || Instant::now() >= deadline {
                self.invalid = true;
                let _ = self.tx.try_send(Command::Close);
                return Err(if self.cancel.is_cancelled() {
                    "cancelled".into()
                } else {
                    deadline_error()
                });
            }
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => {
                    self.invalid = true;
                    return Err(e.to_string());
                }
            }
        }
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // Native modules cannot unload while this thread still executes their code.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delayed_reply(
        delay: Duration,
        on_call: impl FnOnce(&Value) + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tokio_tungstenite::tungstenite::accept(stream).unwrap();
            let request: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            on_call(&request);
            std::thread::sleep(delay);
            let _ = socket.send(Message::Text(
                json!({"id":request["id"],"result":{"accepted":true}})
                    .to_string()
                    .into(),
            ));
        });
        (port, worker)
    }

    #[test]
    fn text_input_still_respects_the_overall_deadline() {
        let (port, worker) = delayed_reply(Duration::from_millis(250), |request| {
            assert_eq!(request["method"], "Input.insertText");
        });
        let mut client = CdpClient::connect(port, "/devtools/page/fixture").unwrap();
        client.set_control(
            CancelToken::new(),
            Instant::now() + Duration::from_millis(75),
        );
        let started = Instant::now();
        let error = client
            .call("Input.insertText", json!({"text":"private prompt"}))
            .unwrap_err();
        assert!(
            error.contains("CDP deadline exceeded: Input.insertText"),
            "{error}"
        );
        assert!(!error.contains("private prompt"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(client.is_invalid());
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn text_input_wait_remains_cancellable() {
        let cancel = CancelToken::new();
        let trigger = cancel.clone();
        let (port, worker) = delayed_reply(Duration::from_millis(250), move |_| trigger.cancel());
        let mut client = CdpClient::connect(port, "/devtools/page/fixture").unwrap();
        client.set_control(cancel, Instant::now() + TEXT_INPUT_TIMEOUT);
        let started = Instant::now();
        assert_eq!(
            client
                .call("Input.insertText", json!({"text":"hello"}))
                .unwrap_err(),
            "cancelled"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(client.is_invalid());
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn command_timeout_reports_method_and_elapsed_time_without_parameters() {
        let (port, worker) = delayed_reply(Duration::from_millis(250), |_| {});
        let mut client = CdpClient::connect(port, "/devtools/page/fixture").unwrap();
        client.set_control(CancelToken::new(), Instant::now() + Duration::from_secs(10));
        let error = client
            .call_with_timeout(
                "Runtime.evaluate",
                json!({"expression":"private expression"}),
                Duration::from_millis(75),
            )
            .unwrap_err();
        assert!(
            error.starts_with("CDP deadline exceeded: Runtime.evaluate after "),
            "{error}"
        );
        assert!(error.ends_with("(budget 0.075s)"), "{error}");
        assert!(!error.contains("private expression"));
        assert!(client.is_invalid());
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "waits 31 seconds to exercise the former CDP command limit"]
    fn text_input_can_finish_after_thirty_seconds() {
        let (port, worker) = delayed_reply(Duration::from_secs(31), |request| {
            assert_eq!(request["method"], "Input.insertText");
        });
        let mut client = CdpClient::connect(port, "/devtools/page/fixture").unwrap();
        client.set_control(CancelToken::new(), Instant::now() + Duration::from_secs(40));
        let result = client
            .call("Input.insertText", json!({"text":"checkpoint"}))
            .unwrap();
        assert_eq!(result["accepted"], true);
        assert!(!client.is_invalid());
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn restricted_connection_rejects_paused_requests_while_navigation_waits() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tokio_tungstenite::tungstenite::accept(stream).unwrap();
            let mut navigation = None;
            let mut blocked = Vec::new();
            while let Ok(message) = socket.read() {
                if !message.is_text() {
                    continue;
                }
                let call: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                match call["method"].as_str().unwrap() {
                    "Page.navigate" => {
                        navigation = Some(call["id"].clone());
                        for (id, resource_type, url) in [
                            ("redirect", "Document", "http://127.0.0.1/private"),
                            (
                                "subresource",
                                "Image",
                                "http://169.254.169.254/latest/meta-data/",
                            ),
                        ] {
                            socket.send(Message::Text(json!({"method":"Fetch.requestPaused","params":{"requestId":id,"frameId":"main","resourceType":resource_type,"request":{"url":url}}}).to_string().into())).unwrap();
                        }
                        continue;
                    }
                    "Fetch.failRequest" => {
                        assert_eq!(call["params"]["errorReason"], "BlockedByClient");
                        blocked.push(call["params"]["requestId"].as_str().unwrap().to_owned());
                    }
                    "Fetch.continueRequest" => panic!("private request must never continue"),
                    _ => (),
                }
                socket
                    .send(Message::Text(
                        json!({"id":call["id"],"result":{}}).to_string().into(),
                    ))
                    .unwrap();
                if blocked.len() == 2 {
                    socket.send(Message::Text(json!({"id":navigation.take().unwrap(),"result":{"frameId":"main","errorText":"net::ERR_BLOCKED_BY_CLIENT"}}).to_string().into())).unwrap();
                    blocked.sort();
                    assert_eq!(blocked, ["redirect", "subresource"]);
                    return;
                }
            }
            panic!("CDP disconnected before blocking both requests");
        });
        let mut client = CdpClient::connect(port, "/devtools/page/fixture").unwrap();
        client.restrict_to_public_network().unwrap();
        let result = client
            .call("Page.navigate", json!({"url":"https://example.com/"}))
            .unwrap();
        assert_eq!(result["errorText"], "net::ERR_BLOCKED_BY_CLIENT");
        assert_eq!(
            client.blocked_navigation_reason("main").as_deref(),
            Some("blocked non-public IP address 127.0.0.1")
        );
        assert!(client.blocked_navigation_reason("other-frame").is_none());
        drop(client);
        worker.join().unwrap();
    }
}
