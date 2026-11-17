use crate::browser_host::{self, TargetInfo};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::Duration;

pub struct MockCdpServer {
    pub port: u16,
    thread: Option<JoinHandle<()>>,
}

impl MockCdpServer {
    pub fn start(
        connections: usize,
        timeout: Duration,
        mut respond: impl FnMut(&Value, usize) -> Value + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            for conn in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(timeout)).unwrap();
                let mut socket = tokio_tungstenite::tungstenite::accept(stream).unwrap();
                while let Ok(message) = socket.read() {
                    if !message.is_text() {
                        continue;
                    }
                    let Ok(text) = message.to_text() else {
                        continue;
                    };
                    let request: Value = serde_json::from_str(text).unwrap();
                    let payload = respond(&request, conn);
                    if socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            payload.to_string().into(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            port,
            thread: Some(thread),
        }
    }

    pub fn start_results(
        connections: usize,
        timeout: Duration,
        mut respond: impl FnMut(&Value, usize) -> Value + Send + 'static,
    ) -> Self {
        Self::start(connections, timeout, move |request, conn| {
            json!({
                "id": request["id"],
                "result": respond(request, conn),
            })
        })
    }

    pub fn target(&self, id: &str, url: &str) -> TargetInfo {
        TargetInfo {
            id: id.into(),
            target_type: "page".into(),
            title: String::new(),
            url: url.into(),
            ws_url: format!("ws://127.0.0.1:{}/devtools/page/{id}", self.port),
        }
    }

    pub fn gemini_target(&self) -> TargetInfo {
        self.target("fixture", browser_host::GEMINI_APP_URL)
    }

    pub fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
