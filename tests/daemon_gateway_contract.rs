//! One real TLS listener serves authenticated gRPC and provider HTTP/1.1.
//! Discovery only: this fixture never starts Chrome or submits model prompts.
use anticrumb::{
    domain::ErrorCode,
    rpc::{client::RpcClient, pb},
};
use reqwest::{blocking::Client, StatusCode, Version};
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TOKEN: &str = "gateway-contract-token-0123456789abcdef";
const WRONG_TOKEN: &str = "wrong-gateway-token-0123456789abcdef";

struct Daemon {
    child: Child,
    root: tempfile::TempDir,
    endpoint: String,
}

impl Daemon {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("daemon.log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_anticrumb"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ANTICRUMB_") {
                command.env_remove(name);
            }
        }
        let child = command
            .args(["server", "127.0.0.1:0", "--token", TOKEN])
            .arg("--state-dir")
            .arg(root.path().join("server"))
            .env("ANTICRUMB_HOME", root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            root,
            endpoint: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let output = fs::read_to_string(&log).unwrap_or_default();
            assert!(!output.contains(TOKEN));
            assert!(daemon.child.try_wait().unwrap().is_none(), "{output}");
            if output.contains("Provider API:") {
                daemon.endpoint = output
                    .lines()
                    .find_map(|line| line.strip_prefix("anticrumb server: "))
                    .expect("server must announce its TLS address")
                    .to_owned();
                assert!(daemon.endpoint.starts_with("https://"));
                return daemon;
            }
            assert!(Instant::now() < deadline, "daemon did not start: {output}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn certificate(&self) -> PathBuf {
        self.root.path().join("server/tls/ca-certificate.pem")
    }

    fn http1(&self) -> Client {
        let certificate =
            reqwest::Certificate::from_pem(&fs::read(self.certificate()).unwrap()).unwrap();
        Client::builder()
            .http1_only()
            .no_proxy()
            .add_root_certificate(certificate)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn same_tls_listener_serves_rpc_and_http1_with_required_authentication() {
    let daemon = Daemon::start();
    let http = daemon.http1();
    for path in ["/health", "/v1/models"] {
        let response = http
            .get(format!("{}{path}", daemon.endpoint))
            .bearer_auth(TOKEN)
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.version(), Version::HTTP_11);
        let body: Value = response.json().unwrap();
        if path == "/health" {
            assert_eq!(body["status"], "ok");
        } else {
            assert_eq!(body["object"], "list");
            assert!(body["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "gemini-web-default"));
        }

        for token in [None, Some(WRONG_TOKEN)] {
            let mut request = http.get(format!("{}{path}", daemon.endpoint));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let response = request.send().unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let text = response.text().unwrap();
            assert!(!text.contains(TOKEN));
            assert!(!text.contains(WRONG_TOKEN));
            let body: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(body["error"]["code"], "invalid_daemon_token");
            assert_eq!(body["error"]["stage"], "daemon_auth");
            let hint = body["error"]["hint"].as_str().unwrap();
            assert!(hint.contains("anticrumb server"));
            assert!(hint.contains("API key"));
            assert!(hint.contains("Signing in to Chrome does not change this token"));
        }
    }

    // The alternate provider API-key headers use the same daemon token.
    for header in ["x-api-key", "x-goog-api-key"] {
        assert_eq!(
            http.get(format!("{}/v1/models", daemon.endpoint))
                .header(header, TOKEN)
                .send()
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let rpc = RpcClient::network(&daemon.endpoint, TOKEN, Some(&daemon.certificate()), None)
            .await
            .unwrap();
        let info = rpc.get_info(pb::Empty {}).await.unwrap();
        assert_eq!(info.role, "browser");
        assert!(!info.server_id.is_empty());
        let wrong = RpcClient::network(
            &daemon.endpoint,
            WRONG_TOKEN,
            Some(&daemon.certificate()),
            None,
        )
        .await
        .unwrap();
        let error = wrong.get_info(pb::Empty {}).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::AuthRequired);
        assert!(!error.message.contains(TOKEN));
        assert!(!error.message.contains(WRONG_TOKEN));
    });

    // Never send credentials on this probe: the listener must reject plaintext
    // before an HTTP route or authentication handler can be reached.
    let plaintext = daemon.endpoint.replacen("https://", "http://", 1);
    assert!(http.get(format!("{plaintext}/health")).send().is_err());
    assert_eq!(
        http.get(format!("{}/health", daemon.endpoint))
            .bearer_auth(TOKEN)
            .send()
            .unwrap()
            .status(),
        StatusCode::OK,
        "a rejected plaintext connection must not stop the TLS listener"
    );
}
