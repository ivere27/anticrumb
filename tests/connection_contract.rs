//! Real CLI setup uses isolated state and owned processes; no browser is opened.
use anticrumb::rpc::daemon::certificate_fingerprint;
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TOKEN: &str = "connection-test-token-0123456789abcdef";
const WRONG_TOKEN: &str = "incorrect-test-token-0123456789abcdef";

struct OwnedChild(Child);
impl OwnedChild {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Environment {
    root: tempfile::TempDir,
}
impl Environment {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }
    fn state(&self) -> PathBuf {
        self.root.path().join("workspace")
    }
    fn saved(&self) -> PathBuf {
        self.state().join("connection.json")
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_anticrumb"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ANTICRUMB_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(self.root.path())
            .env("ANTICRUMB_HOME", self.root.path());
        command
    }
    fn run(&self, args: &[&str], answer: Option<&str>) -> Output {
        let stdout = tempfile::NamedTempFile::new_in(self.root.path()).unwrap();
        let stderr = tempfile::NamedTempFile::new_in(self.root.path()).unwrap();
        let mut child = OwnedChild(
            self.command()
                .args(args)
                .stdin(Stdio::piped())
                .stdout(stdout.reopen().unwrap())
                .stderr(stderr.reopen().unwrap())
                .spawn()
                .unwrap(),
        );
        if let Some(answer) = answer {
            child
                .0
                .stdin
                .as_mut()
                .unwrap()
                .write_all(answer.as_bytes())
                .unwrap();
            // Keep stdin open: reading a trust answer must finish at the newline,
            // without waiting for the terminal/input stream to reach EOF.
        } else {
            drop(child.0.stdin.take());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not finish: {}",
                fs::read_to_string(stderr.path()).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(20));
        };
        let output = Output {
            success: status.success(),
            stdout: fs::read_to_string(stdout.path()).unwrap(),
            stderr: fs::read_to_string(stderr.path()).unwrap(),
        };
        for secret in [TOKEN, WRONG_TOKEN] {
            assert!(!output.stdout.contains(secret));
            assert!(!output.stderr.contains(secret));
        }
        output
    }
}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}
impl Output {
    fn succeeded(&self) {
        assert!(self.success, "{}", self.stderr);
    }
    fn error(&self) -> Value {
        assert!(!self.success, "unexpected success: {}", self.stdout);
        // Trust prompts precede the structured CLI error on stderr.
        let json = self.stderr.rfind("{\"error\":").unwrap();
        serde_json::from_str(&self.stderr[json..]).unwrap()
    }
    fn json(&self) -> Value {
        self.succeeded();
        serde_json::from_str(&self.stdout).unwrap()
    }
}

struct Daemon {
    child: OwnedChild,
    endpoint: String,
    pin: String,
    token: String,
}
impl Daemon {
    fn start(env: &Environment, listen: &str, regenerate: bool) -> Self {
        Self::start_with_token(env, listen, regenerate, Some(TOKEN))
    }
    fn start_with_token(
        env: &Environment,
        listen: &str,
        regenerate: bool,
        provided_token: Option<&str>,
    ) -> Self {
        let log = env.root.path().join("owned-daemon.log");
        let mut command = env.command();
        command.args(["daemon", "--listen", listen]);
        if let Some(token) = provided_token {
            command.args(["--token", token]);
        }
        if regenerate {
            command.arg("--regenerate-tls");
        }
        let mut child = OwnedChild(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let text = wait_for_log(&mut child, &log, "Connect:");
        assert!(!text.contains(TOKEN));
        assert!(text.contains("TLS certificate emoji:"));
        let token = match provided_token {
            Some(token) => {
                assert!(!text.contains(token));
                assert!(!text.contains("Connection token:"));
                token.to_owned()
            }
            None => text
                .lines()
                .find_map(|line| line.strip_prefix("Connection token: "))
                .expect("generated connection token must be displayed at startup")
                .to_owned(),
        };
        let endpoint = text
            .lines()
            .find_map(|line| line.strip_prefix("anticrumb daemon: "))
            .unwrap()
            .to_owned();
        if provided_token.is_none() {
            assert!(text.contains(&format!(
                "Connect: anticrumb connect '{endpoint}' --token '{token}'"
            )));
        }
        let pin = certificate_fingerprint(
            &fs::read(env.root.path().join("browser/tls/certificate.pem")).unwrap(),
        )
        .unwrap();
        assert!(text.contains(&pin));
        Self {
            child,
            endpoint,
            pin,
            token,
        }
    }
    fn regenerate(&mut self, env: &Environment) {
        self.child.stop();
        let listen = self.endpoint.strip_prefix("https://").unwrap().to_owned();
        *self = Self::start(env, &listen, true);
    }
}

fn wait_for_log(child: &mut OwnedChild, log: &Path, marker: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = fs::read_to_string(log).unwrap_or_default();
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited: {text}"
        );
        if text.contains(marker) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "child did not become ready: {text}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn interactive_trust_saves_only_after_acceptance_and_successful_authentication() {
    let env = Environment::new();
    let daemon = Daemon::start(&env, "127.0.0.1:0", false);
    let args = [
        "--connect",
        &daemon.endpoint,
        "--token",
        TOKEN,
        "connect",
        "status",
    ];
    let declined = env.run(&args, Some("n\n"));
    assert_eq!(declined.error()["error"]["code"], "cancelled");
    assert!(declined.stderr.contains(&daemon.pin));
    assert!(declined.stderr.contains("Emoji:"));
    assert!(!env.saved().exists());

    env.run(&args, Some("yes\n")).succeeded();
    let saved = fs::read(env.saved()).unwrap();
    let status = env.run(&["connect", "status"], None).json();
    assert_eq!(status["default"], "local");
    assert_eq!(status["saved_remote"]["fingerprint"], daemon.pin);
    let reconnect = env.run(&args, None);
    reconnect.succeeded();
    assert!(!reconnect.stderr.contains("Trust this certificate?"));
    let wrong = env.run(
        &[
            "--connect",
            &daemon.endpoint,
            "--token",
            WRONG_TOKEN,
            "connect",
            "status",
        ],
        None,
    );
    assert_eq!(wrong.error()["error"]["code"], "auth_required");
    assert!(fs::read(env.saved()).unwrap() == saved);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(env.saved()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn regenerated_certificate_requires_new_explicit_trust() {
    let env = Environment::new();
    let mut daemon = Daemon::start(&env, "127.0.0.1:0", false);
    let old_pin = daemon.pin.clone();
    env.run(
        &[
            "--connect",
            &daemon.endpoint,
            "--token",
            TOKEN,
            "--tls-pin",
            &old_pin,
            "connect",
            "status",
        ],
        None,
    )
    .succeeded();
    let saved = fs::read(env.saved()).unwrap();
    daemon.regenerate(&env);
    assert_ne!(daemon.pin, old_pin);
    assert!(!env.run(&["connect"], None).success);
    assert!(fs::read(env.saved()).unwrap() == saved);
    let args = [
        "--connect",
        &daemon.endpoint,
        "--token",
        TOKEN,
        "connect",
        "status",
    ];
    let declined = env.run(&args, Some("no\n"));
    assert_eq!(declined.error()["error"]["code"], "cancelled");
    assert!(declined.stderr.contains("certificate has changed"));
    assert!(fs::read(env.saved()).unwrap() == saved);
    env.run(&args, Some("y\n")).succeeded();
    assert_eq!(
        env.run(&["connect", "status"], None).json()["saved_remote"]["fingerprint"],
        daemon.pin
    );
}

#[test]
fn default_daemon_uses_tls_and_displays_its_persistent_generated_token_only_after_successful_setup()
{
    let env = Environment::new();
    let mut daemon = Daemon::start_with_token(&env, "127.0.0.1:0", false, None);
    assert!(daemon.endpoint.starts_with("https://"));
    assert_eq!(daemon.token.len(), 64);
    assert!(daemon.token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let first_token = daemon.token.clone();
    let first_pin = daemon.pin.clone();
    let listen = daemon.endpoint.strip_prefix("https://").unwrap().to_owned();
    let setup = env.run(
        &[
            "--connect",
            &daemon.endpoint,
            "--token",
            &daemon.token,
            "--tls-pin",
            &daemon.pin,
            "connect",
            "status",
        ],
        None,
    );
    setup.succeeded();
    assert!(!setup.stdout.contains(&first_token));
    assert!(!setup.stderr.contains(&first_token));

    let conflicting_state = env.root.path().join("conflicting-browser");
    let failed = env.run(
        &[
            "daemon",
            "--listen",
            &listen,
            "--state-dir",
            conflicting_state.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(failed.error()["error"]["stage"], "daemon_listen");
    assert!(!failed.stderr.contains("Connection token:"));
    assert!(!conflicting_state.join("token").exists());

    daemon.child.stop();
    daemon = Daemon::start_with_token(&env, &listen, false, None);
    assert!(daemon.token == first_token);
    assert_eq!(daemon.pin, first_pin);
    assert_eq!(
        env.run(&["connect", "status"], None).json()["saved_remote"]["fingerprint"],
        first_pin
    );
    let insecure = env.run(&["daemon", "--insecure-http"], None);
    assert_eq!(insecure.error()["error"]["code"], "invalid_request");
    assert!(!insecure.stderr.contains("Connection token:"));
}

#[test]
fn saved_remote_is_explicit_and_profile_edits_are_independent_of_runtime_ownership() {
    let env = Environment::new();
    let mut daemon = Daemon::start(&env, "127.0.0.1:0", false);
    let status = env
        .run(
            &[
                "--connect",
                &daemon.endpoint,
                "--token",
                TOKEN,
                "--tls-pin",
                &daemon.pin,
                "connect",
                "status",
            ],
            None,
        )
        .json();
    assert_eq!(status["default"], "local");
    assert_eq!(status["saved_remote"]["endpoint"], daemon.endpoint);
    let remote = env
        .run(
            &[
                "--connect",
                &daemon.endpoint,
                "--token",
                TOKEN,
                "--tls-pin",
                &daemon.pin,
                "doctor",
            ],
            None,
        )
        .json();
    assert_eq!(remote["browser_connection"]["transport"], "grpc");
    assert_eq!(remote["browser_connection"]["connected"], true);
    // Same-machine clients of an explicitly network-enabled daemon need no token.
    let local = env.run(&["doctor"], None).json();
    assert_eq!(local["browser_connection"]["transport"], "local");
    assert!(daemon.child.0.try_wait().unwrap().is_none());
    let saved = fs::read(env.saved()).unwrap();
    // A persistent daemon owns workspace state while profiles remain editable.
    wait_for_log(
        &mut daemon.child,
        &env.root.path().join("owned-daemon.log"),
        "anticrumb local daemon:",
    );
    assert!(anticrumb::rpc::host::Host::lease(&env.state()).is_err());
    use fs2::FileExt;
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(env.state().join("connection.lock"))
        .unwrap();
    lease.try_lock_exclusive().unwrap();
    let changed = env.run(
        &[
            "--connect",
            &daemon.endpoint,
            "--token",
            WRONG_TOKEN,
            "connect",
            "status",
        ],
        None,
    );
    assert_eq!(changed.error()["error"]["code"], "workspace_conflict");
    assert_eq!(
        env.run(&["connect", "local"], None).error()["error"]["code"],
        "workspace_conflict"
    );
    assert!(fs::read(env.saved()).unwrap() == saved);
    // Reading needs neither the profile-edit lock nor a live daemon connection.
    env.run(&["connect", "status"], None).succeeded();
    drop(lease);
    env.run(&["connect", "local"], None).succeeded();
    assert!(daemon.child.0.try_wait().unwrap().is_none());
    daemon.child.stop();
    assert!(!env.saved().exists());
    assert_eq!(
        env.run(&["connect", "status"], None).json(),
        serde_json::json!({"default":"local", "saved_remote":null})
    );
    let missing = env.run(&["connect"], None);
    assert_eq!(missing.error()["error"]["code"], "invalid_request");
    assert!(missing.stderr.contains("no saved remote profile"));
}

#[test]
fn invalid_connection_flags_do_not_change_state_or_disclose_url_credentials() {
    let env = Environment::new();
    for args in [
        vec!["--connect"],
        vec!["--connect", "https://localhost:50051", "--token"],
        vec![
            "--connect",
            "https://localhost:50051",
            "--connect",
            "https://localhost:50052",
        ],
        vec!["connect", "http://localhost:50051", "--token", TOKEN],
        vec![
            "connect",
            "https://private-user:private-password@localhost:50051",
            "--token",
            TOKEN,
        ],
        vec![
            "connect",
            "https://localhost:50051/path?token=private-query",
            "--token",
            TOKEN,
        ],
    ] {
        let output = env.run(&args, None);
        assert_eq!(output.error()["error"]["code"], "invalid_request");
        assert!(!output.stderr.contains("private-"));
        assert!(!env.saved().exists());
    }
    // Arguments following -- belong to the prompt, not connection setup.
    env.run(
        &["help", "--", "--connect", "https://localhost:50051"],
        None,
    )
    .succeeded();
    assert!(!env.saved().exists());
}
