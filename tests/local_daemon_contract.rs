//! Real-process coverage of IPC ownership, concurrency, and automatic cleanup.
use anticrumb::rpc::daemon::certificate_fingerprint;
use fs2::FileExt;
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::{
    handshake::derive_accept_key, protocol::Role, Message, WebSocket,
};

fn clean_command(root: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_anticrumb"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ANTICRUMB_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(cwd)
        .env("ANTICRUMB_HOME", root)
        .env("CHROME_BIN", std::env::current_exe().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
fn command(root: &Path, cwd: &Path) -> Command {
    let mut command = clean_command(root, cwd);
    command
        .env("ANTICRUMB_TOKEN", "ignored-for-local-ipc")
        .env("ANTICRUMB_ENDPOINT", "https://127.0.0.1:1");
    command
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(20));
    }
}
fn record(root: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(root.join("run/daemon.json")).ok()?).ok()
}
fn status(root: &Path, cwd: &Path) -> Value {
    let output = command(root, cwd).arg("doctor").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn remote_doctor(root: &Path, cwd: &Path, endpoint: &str, token: &str, pin: &str) -> Value {
    let output = clean_command(root, cwd)
        .args([
            "--connect",
            endpoint,
            "--token",
            token,
            "--tls-pin",
            pin,
            "doctor",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn lock_state(path: &Path) -> Option<bool> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok()?;
    if file.try_lock_exclusive().is_err() {
        return Some(false);
    }
    FileExt::unlock(&file).unwrap();
    Some(true)
}
fn released(root: &Path) -> bool {
    if root.join("run/daemon.json").exists() {
        return false;
    }
    ["run/daemon.lock", "workspace/host.lock", "server/host.lock"]
        .iter()
        .all(|path| lock_state(&root.join(path)) == Some(true))
}
fn daemon_released(root: &Path) -> bool {
    if root.join("run/daemon.json").exists() {
        return false;
    }
    if lock_state(&root.join("run/daemon.lock")) != Some(true) {
        return false;
    }
    ["workspace/host.lock", "server/host.lock"]
        .iter()
        .all(|path| lock_state(&root.join(path)) != Some(false))
}
struct Session {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<String>,
}
impl Session {
    fn start(root: &Path, cwd: &Path) -> Self {
        let mut cmd = command(root, cwd);
        cmd.args(["browser", "--json"]);
        Self::start_command(cmd)
    }
    fn start_connect(root: &Path, cwd: &Path, endpoint: &str, token: &str, pin: &str) -> Self {
        let mut cmd = clean_command(root, cwd);
        cmd.args([
            "--connect",
            endpoint,
            "--token",
            token,
            "--tls-pin",
            pin,
            "browser",
            "--json",
        ]);
        Self::start_command(cmd)
    }
    fn start_command(mut cmd: Command) -> Self {
        let mut child = cmd.stdin(Stdio::piped()).spawn().unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut session = Self {
            child,
            input,
            output,
        };
        session.help();
        session
    }
    fn help(&mut self) {
        writeln!(self.input.as_mut().unwrap(), "help").unwrap();
        let line = self
            .output
            .recv_timeout(Duration::from_secs(15))
            .expect("browser client did not connect");
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ok"], true);
    }
    fn exit(mut self) {
        self.input.take();
        wait_for(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success());
    }
    fn crash(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn commands_use_local_ipc_without_network_credentials_and_release_leases() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut identity = None;
    for name in ["doctor", "models", "cache", "doctor"] {
        let output = command(root, root)
            .env(
                "ANTICRUMB_RPC_LISTEN",
                listener.local_addr().unwrap().to_string(),
            )
            .arg(name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        if name == "cache" {
            let stores = value["stores"].as_array().unwrap();
            assert_eq!(stores.len(), 2);
            assert_eq!(stores[0]["role"], "workspace");
            assert_eq!(stores[1]["role"], "browser");
            for store in stores {
                assert_eq!(store["error"], "");
                assert_eq!(store["stats"]["retention_days"], 7);
                assert_eq!(store["stats"]["pruned"], false);
            }
        }
        if name == "doctor" {
            assert_eq!(value["browser_connection"]["transport"], "local");
            assert_eq!(value["browser_connection"]["connected"], true);
            assert_eq!(value["runtime"]["automatic"], true);
            assert_eq!(value["runtime"]["stopping"], false);
            assert_eq!(value["runtime"]["version"], env!("CARGO_PKG_VERSION"));
            assert_eq!(
                value["runtime"]["transport"],
                if cfg!(windows) { "named_pipe" } else { "unix" }
            );
            assert_eq!(value["runtime"]["connected_clients"], 1);
            assert!(value["runtime"]["pid"].as_u64().unwrap() > 0);
            assert_eq!(value["active_agents"], 0);
            if let Some(previous) = &identity {
                assert_eq!(&value["server_id"], previous);
            }
            identity = Some(value["server_id"].clone());
        }
        wait_for(|| released(root));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        for state in ["workspace", "server"] {
            assert!(!root.join(state).join("token").exists());
            assert!(!root.join(state).join("tls").exists());
        }
        assert!(
            !root.join("profile").exists(),
            "offline commands launched Chrome"
        );
        assert!(!root.join("run/daemon.sock").exists());
    }
}

#[cfg(unix)]
#[test]
fn relative_state_under_a_long_working_directory_resolves_the_same_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("long-project-directory".repeat(7));
    fs::create_dir(&cwd).unwrap();
    let state = cwd.join("state");
    let output = command(&state, &cwd)
        .env("ANTICRUMB_HOME", "state")
        .arg("doctor")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoint = record(&state).map(|record| record["endpoint"].as_str().unwrap().to_owned());
    wait_for(|| released(&state));
    if let Some(endpoint) = endpoint {
        assert!(!Path::new(&endpoint).exists());
        assert!(!Path::new(&endpoint).parent().unwrap().exists());
    }
}

#[test]
fn different_directories_share_one_daemon_until_the_last_client_exits() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let a = root.join("project-a");
    let b = root.join("project-b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let first = Session::start(root, &a);
    let pid = record(root).unwrap()["pid"].clone();
    assert_eq!(record(root).unwrap()["managed"], true);
    let mut second = Session::start(root, &b);
    assert_eq!(record(root).unwrap()["pid"], pid);
    let before = status(root, &a);
    assert_eq!(before["runtime"]["pid"], pid);
    assert_eq!(before["runtime"]["connected_clients"], 3); // Two shells + doctor.
    first.exit();
    thread::sleep(Duration::from_millis(600));
    second.help();
    assert_eq!(record(root).unwrap()["pid"], pid);
    let after = status(root, &b);
    assert_eq!(after["runtime"]["connected_clients"], 2); // Remaining shell + doctor.
    assert!(
        after["runtime"]["uptime_ms"].as_u64().unwrap()
            >= before["runtime"]["uptime_ms"].as_u64().unwrap()
    );
    second.exit();
    wait_for(|| released(root));
}

#[test]
fn concurrent_startup_uses_one_daemon_and_crashed_clients_do_not_keep_it_alive() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let sessions = thread::scope(|scope| {
        let starters: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| Session::start(root, root)))
            .collect();
        starters
            .into_iter()
            .map(|starter| starter.join().unwrap())
            .collect::<Vec<_>>()
    });
    let pid = record(root).unwrap()["pid"].clone();
    for mut session in sessions {
        session.help();
        assert_eq!(record(root).unwrap()["pid"], pid);
        session.crash();
    }
    wait_for(|| released(root));
}

#[test]
fn explicit_local_daemon_survives_its_last_client() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let profile = root.join("profiles/claude-web");
    fs::create_dir_all(&profile).unwrap();
    let cdp = MockCdpServer::start("persistent-test");
    let meta_path = cdp.write_metadata(root, &profile);

    let mut daemon = Daemon(command(root, root).arg("daemon").spawn().unwrap());
    wait_for(|| record(root).is_some());
    assert_eq!(record(root).unwrap()["managed"], false);
    let runtime = status(root, root)["runtime"].clone();
    assert_eq!(runtime["automatic"], false);
    assert_eq!(runtime["pid"], daemon.0.id());
    Session::start(root, root).exit();
    thread::sleep(Duration::from_millis(600));
    assert!(daemon.0.try_wait().unwrap().is_none());
    assert!(
        !cdp.is_closed() && meta_path.exists(),
        "persistent daemon must not shut down managed Chrome when idle"
    );
    assert_eq!(lock_state(&root.join("workspace/host.lock")), Some(false));
    assert_eq!(lock_state(&root.join("server/host.lock")), Some(false));
    Session::start(root, root).exit();
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(daemon.0.id() as i32, libc::SIGTERM);
        }
        wait_for(|| daemon.0.try_wait().unwrap().is_some());
        assert!(daemon.0.wait().unwrap().success());
        assert!(released(root));
        assert!(
            cdp.finish() && !meta_path.exists(),
            "persistent daemon must still shut down managed Chrome on daemon exit"
        );
    }
}

#[cfg(unix)]
#[test]
fn stale_socket_is_reclaimed_only_under_the_owner_lock() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("run")).unwrap();
    let socket = root.join("run/daemon.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let session = Session::start(root, root);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(root.join("run")).unwrap().permissions().mode() & 0o777,
        0o700
    );
    session.exit();
    wait_for(|| released(root));
    assert!(!socket.exists());
}

struct MockCdpServer {
    port: u16,
    closed_via_cdp: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl MockCdpServer {
    fn start(browser_id: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let closed_via_cdp = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let closed_srv = closed_via_cdp.clone();
        let stop_srv = stop.clone();
        let ws_path = format!("/devtools/browser/{browser_id}");

        let server = thread::spawn(move || {
            let mut alive = true;
            while !stop_srv.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                if !alive {
                    continue;
                }
                let _ = stream.set_nonblocking(false);
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]).into_owned();
                if req_str.starts_with("GET /json/version ") {
                    let body = serde_json::json!({
                        "Browser": "Chrome/130.0.0.0",
                        "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}{ws_path}")
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                } else if req_str.contains(&ws_path) {
                    let key = req_str
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("sec-websocket-key")
                                .then(|| v.trim())
                        })
                        .unwrap_or("");
                    let accept = derive_accept_key(key.as_bytes());
                    let hs = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    );
                    let _ = stream.write_all(hs.as_bytes());
                    let mut ws = WebSocket::from_raw_socket(stream, Role::Server, None);
                    if let Ok(Message::Text(txt)) = ws.read() {
                        let msg: Value = serde_json::from_str(&txt).unwrap();
                        if msg["method"] == "Browser.close" {
                            closed_srv.store(true, Ordering::SeqCst);
                            let _ = ws.send(Message::Text(
                                serde_json::json!({"id": msg["id"], "result": {}})
                                    .to_string()
                                    .into(),
                            ));
                            alive = false;
                        }
                    }
                }
            }
        });

        Self {
            port,
            closed_via_cdp,
            stop,
            server: Some(server),
        }
    }
    fn write_metadata(&self, root: &Path, profile: &Path) -> PathBuf {
        let meta_path = root.join(format!("cdp-port-{}.json", self.port));
        fs::write(
            &meta_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "port": self.port,
                "user_data_dir": profile,
                "profile_dir": "Default",
                "mode": "headless",
                "headless": true,
                "xvfb": false,
                "pid": 0
            }))
            .unwrap(),
        )
        .unwrap();
        meta_path
    }
    fn is_closed(&self) -> bool {
        self.closed_via_cdp.load(Ordering::SeqCst)
    }
    fn finish(mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
        self.closed_via_cdp.load(Ordering::SeqCst)
    }
}
impl Drop for MockCdpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

struct RemoteServer {
    _child: Daemon,
    endpoint: String,
    pin: String,
    token: String,
}
impl RemoteServer {
    fn start(root: &Path, token: &str) -> Self {
        let log = root.join("remote-server.log");
        let mut child = Daemon(
            clean_command(root, root)
                .args(["server", "127.0.0.1:0", "--token", token])
                .stdout(Stdio::null())
                .stderr(fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let text = loop {
            let text = fs::read_to_string(&log).unwrap_or_default();
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "remote server exited: {text}"
            );
            if text.contains("Connect:") {
                break text;
            }
            assert!(
                Instant::now() < deadline,
                "remote server did not become ready: {text}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        let endpoint = text
            .lines()
            .find_map(|line| line.strip_prefix("anticrumb server: "))
            .unwrap()
            .to_owned();
        let pin =
            certificate_fingerprint(&fs::read(root.join("server/tls/ca-certificate.pem")).unwrap())
                .unwrap();
        Self {
            _child: child,
            endpoint,
            pin,
            token: token.to_owned(),
        }
    }
}

#[test]
fn automatic_daemon_gracefully_shuts_down_managed_chrome_on_idle_exit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let profile = root.join("profiles/claude-web");
    fs::create_dir_all(&profile).unwrap();

    let cdp = MockCdpServer::start("local-test");
    let meta_path = cdp.write_metadata(root, &profile);

    let session = Session::start(root, root);
    session.exit();
    wait_for(|| released(root));

    assert!(
        cdp.finish(),
        "automatic daemon did not send CDP Browser.close on idle exit"
    );
    assert!(
        !meta_path.exists(),
        "cdp metadata file was not cleaned up after Chrome shutdown"
    );
}

#[test]
fn local_managed_chrome_shuts_down_when_local_clients_exit_while_remote_connect_stays_active() {
    const TOKEN: &str = "local-daemon-remote-connect-test-token-0123456789";

    let remote_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let client_root = client_dir.path();
    let profile = client_root.join("profiles/claude-web");
    fs::create_dir_all(&profile).unwrap();

    let _server_guard = RemoteServer::start(remote_dir.path(), TOKEN);

    // 1. Remote `anticrumb --connect ...` starts the shared automatic local daemon
    // without eagerly initializing or locking `workspace/host.lock` / `server/host.lock`.
    let remote = Session::start_connect(
        client_root,
        client_root,
        &_server_guard.endpoint,
        &_server_guard.token,
        &_server_guard.pin,
    );
    let initial_record = record(client_root).expect("automatic local daemon record");
    assert_eq!(initial_record["managed"], true);
    let daemon_pid = initial_record["pid"].clone();
    assert_eq!(
        lock_state(&client_root.join("run/daemon.lock")),
        Some(false)
    );
    assert!(!client_root.join("workspace/host.lock").exists());
    assert!(!client_root.join("server/host.lock").exists());

    // 2. A local client joins the same daemon, lazily bringing up Connection::Local
    // alongside a managed local Chrome instance.
    let cdp1 = MockCdpServer::start("local-reclaim-1");
    let meta1 = cdp1.write_metadata(client_root, &profile);
    let local1 = Session::start(client_root, client_root);
    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    wait_for(|| {
        lock_state(&client_root.join("workspace/host.lock")) == Some(false)
            && lock_state(&client_root.join("server/host.lock")) == Some(false)
    });

    // 3. Exiting the last local client must close managed Chrome and release local host
    // locks after IDLE_GRACE even while the remote connect client stays alive.
    local1.exit();
    wait_for(|| {
        cdp1.is_closed()
            && !meta1.exists()
            && lock_state(&client_root.join("workspace/host.lock")) == Some(true)
            && lock_state(&client_root.join("server/host.lock")) == Some(true)
    });
    assert!(cdp1.finish());

    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    assert_eq!(
        lock_state(&client_root.join("run/daemon.lock")),
        Some(false)
    );

    let doctor = remote_doctor(
        client_root,
        client_root,
        &_server_guard.endpoint,
        &_server_guard.token,
        &_server_guard.pin,
    );
    assert_eq!(doctor["browser_connection"]["transport"], "grpc");
    assert_eq!(doctor["browser_connection"]["connected"], true);
    assert_eq!(doctor["runtime"]["pid"], daemon_pid);
    assert_eq!(doctor["runtime"]["connected_clients"], 2);
    assert_eq!(
        lock_state(&client_root.join("workspace/host.lock")),
        Some(true)
    );
    assert_eq!(
        lock_state(&client_root.join("server/host.lock")),
        Some(true)
    );

    // 4. Launching another local client on the same daemon cleanly recreates
    // Connection::Local and again shuts down Chrome when that client exits.
    let cdp2 = MockCdpServer::start("local-reclaim-2");
    let meta2 = cdp2.write_metadata(client_root, &profile);
    let local2 = Session::start(client_root, client_root);
    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    wait_for(|| {
        lock_state(&client_root.join("workspace/host.lock")) == Some(false)
            && lock_state(&client_root.join("server/host.lock")) == Some(false)
    });

    local2.exit();
    wait_for(|| {
        cdp2.is_closed()
            && !meta2.exists()
            && lock_state(&client_root.join("workspace/host.lock")) == Some(true)
            && lock_state(&client_root.join("server/host.lock")) == Some(true)
    });
    assert!(cdp2.finish());

    let doctor_after = remote_doctor(
        client_root,
        client_root,
        &_server_guard.endpoint,
        &_server_guard.token,
        &_server_guard.pin,
    );
    assert_eq!(doctor_after["runtime"]["pid"], daemon_pid);
    assert_eq!(doctor_after["runtime"]["connected_clients"], 2);

    // 5. Exiting the final remote connect client shuts down the shared daemon.
    remote.exit();
    wait_for(|| released(client_root));
}

#[test]
fn connect_only_exits_cleanly_without_creating_local_host_locks() {
    const TOKEN: &str = "connect-only-daemon-release-test-token-0123456789";

    let remote_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let client_root = client_dir.path();
    let server = RemoteServer::start(remote_dir.path(), TOKEN);

    let remote = Session::start_connect(
        client_root,
        client_root,
        &server.endpoint,
        &server.token,
        &server.pin,
    );
    assert!(!client_root.join("workspace/host.lock").exists());
    assert!(!client_root.join("server/host.lock").exists());

    remote.exit();
    wait_for(|| daemon_released(client_root));
    assert!(!client_root.join("workspace/host.lock").exists());
    assert!(!client_root.join("server/host.lock").exists());
}

#[test]
fn multiple_remote_and_local_clients_keep_chrome_alive_until_last_local_exits() {
    const TOKEN1: &str = "multi-client-server-one-test-token-0123456789abcdef";
    const TOKEN2: &str = "multi-client-server-two-test-token-0123456789abcdef";

    let remote_dir1 = tempfile::tempdir().unwrap();
    let remote_dir2 = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let client_root = client_dir.path();
    let profile = client_root.join("profiles/claude-web");
    fs::create_dir_all(&profile).unwrap();

    let server1 = RemoteServer::start(remote_dir1.path(), TOKEN1);
    let server2 = RemoteServer::start(remote_dir2.path(), TOKEN2);

    // 1. Multiple remote `connect` sessions share one automatic local daemon without
    // initializing local workspace/browser hosts.
    let remote1 = Session::start_connect(
        client_root,
        client_root,
        &server1.endpoint,
        &server1.token,
        &server1.pin,
    );
    let daemon_pid = record(client_root).unwrap()["pid"].clone();
    let remote2 = Session::start_connect(
        client_root,
        client_root,
        &server2.endpoint,
        &server2.token,
        &server2.pin,
    );
    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    assert!(!client_root.join("workspace/host.lock").exists());
    assert!(!client_root.join("server/host.lock").exists());

    // 2. Multiple local clients share Connection::Local and managed Chrome on the same daemon.
    let cdp = MockCdpServer::start("multi-local-test");
    let meta = cdp.write_metadata(client_root, &profile);
    let local1 = Session::start(client_root, client_root);
    let mut local2 = Session::start(client_root, client_root);
    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    wait_for(|| {
        lock_state(&client_root.join("workspace/host.lock")) == Some(false)
            && lock_state(&client_root.join("server/host.lock")) == Some(false)
    });

    // Exiting only one of two local clients must keep managed Chrome and local locks alive
    // well past IDLE_GRACE.
    local1.exit();
    thread::sleep(Duration::from_millis(600));
    assert!(
        !cdp.is_closed() && meta.exists(),
        "managed Chrome closed while another local client was still connected"
    );
    assert_eq!(
        lock_state(&client_root.join("workspace/host.lock")),
        Some(false)
    );
    assert_eq!(
        lock_state(&client_root.join("server/host.lock")),
        Some(false)
    );
    local2.help();
    let d_mid = remote_doctor(
        client_root,
        client_root,
        &server1.endpoint,
        &server1.token,
        &server1.pin,
    );
    assert_eq!(d_mid["runtime"]["pid"], daemon_pid);
    assert_eq!(d_mid["runtime"]["connected_clients"], 4); // remote1 + remote2 + local2 + doctor

    // 3. Exiting the final local client shuts down Chrome while keeping the shared daemon
    // and both remote `connect` clients active.
    local2.exit();
    wait_for(|| {
        cdp.is_closed()
            && !meta.exists()
            && lock_state(&client_root.join("workspace/host.lock")) == Some(true)
            && lock_state(&client_root.join("server/host.lock")) == Some(true)
    });
    assert!(cdp.finish());
    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    assert_eq!(
        lock_state(&client_root.join("run/daemon.lock")),
        Some(false)
    );

    for srv in [&server1, &server2] {
        let d = remote_doctor(
            client_root,
            client_root,
            &srv.endpoint,
            &srv.token,
            &srv.pin,
        );
        assert_eq!(d["browser_connection"]["transport"], "grpc");
        assert_eq!(d["browser_connection"]["connected"], true);
        assert_eq!(d["runtime"]["pid"], daemon_pid);
        assert_eq!(d["runtime"]["connected_clients"], 3); // remote1 + remote2 + doctor
    }

    // Closing one remote client keeps the daemon alive for the remaining remote client.
    remote1.exit();
    thread::sleep(Duration::from_millis(600));
    let d_last = remote_doctor(
        client_root,
        client_root,
        &server2.endpoint,
        &server2.token,
        &server2.pin,
    );
    assert_eq!(d_last["runtime"]["pid"], daemon_pid);
    assert_eq!(d_last["runtime"]["connected_clients"], 2); // remote2 + doctor

    remote2.exit();
    wait_for(|| released(client_root));
}

#[cfg(unix)]
#[test]
fn platform_shutdown_on_grpc_binding_preserves_shared_daemon_and_local_chrome() {
    const TOKEN1: &str = "shutdown-scope-server-one-token-0123456789abcdef";
    const TOKEN2: &str = "shutdown-scope-server-two-token-0123456789abcdef";

    async fn connect_uds_client(
        socket_path: PathBuf,
        protocol: String,
        remote: Option<anticrumb::rpc::pb::RemoteBrowser>,
    ) -> anticrumb::rpc::pb::platform_service_client::PlatformServiceClient<tonic::transport::Channel>
    {
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket_path = socket_path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(&socket_path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .unwrap();
        let mut workspace =
            anticrumb::rpc::pb::workspace_service_client::WorkspaceServiceClient::new(
                channel.clone(),
            );
        workspace
            .connect_client(anticrumb::rpc::pb::ConnectClientRequest {
                protocol,
                remote,
                workers: None,
                pace: None,
            })
            .await
            .unwrap();
        anticrumb::rpc::pb::platform_service_client::PlatformServiceClient::new(channel)
    }

    let remote_dir1 = tempfile::tempdir().unwrap();
    let remote_dir2 = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let client_root = client_dir.path();
    let profile = client_root.join("profiles/claude-web");
    fs::create_dir_all(&profile).unwrap();

    let server1 = RemoteServer::start(remote_dir1.path(), TOKEN1);
    let server2 = RemoteServer::start(remote_dir2.path(), TOKEN2);

    let remote2 = Session::start_connect(
        client_root,
        client_root,
        &server2.endpoint,
        &server2.token,
        &server2.pin,
    );
    let info = record(client_root).unwrap();
    let daemon_pid = info["pid"].clone();
    let endpoint = PathBuf::from(info["endpoint"].as_str().unwrap());
    let protocol = info["protocol"].as_str().unwrap().to_owned();

    let cdp = MockCdpServer::start("shutdown-scope-local");
    let meta = cdp.write_metadata(client_root, &profile);
    let mut local = Session::start(client_root, client_root);
    wait_for(|| lock_state(&client_root.join("workspace/host.lock")) == Some(false));

    let server1_remote = anticrumb::rpc::pb::RemoteBrowser {
        endpoint: server1.endpoint.clone(),
        token: server1.token.clone(),
        ca_file: None,
        server_name: None,
        tls_pin: Some(server1.pin.clone()),
    };

    let rt = tokio::runtime::Runtime::new().unwrap();

    // Two concurrent clients bound to the SAME remote server (server1) share one workspace Host.
    let (mut client_a, mut client_b) = rt.block_on(async {
        let a = connect_uds_client(
            endpoint.clone(),
            protocol.clone(),
            Some(server1_remote.clone()),
        )
        .await;
        let b = connect_uds_client(endpoint.clone(), protocol.clone(), Some(server1_remote)).await;
        (a, b)
    });

    // 1a. Client A calling PlatformService/Shutdown over Connection::Grpc detaches only A:
    // client B's existing binding to server1, remote2, shared daemon, and local Chrome keep working.
    rt.block_on(async {
        client_a
            .shutdown(anticrumb::rpc::pb::Empty {})
            .await
            .unwrap();
        let after_a = client_a.get_status(anticrumb::rpc::pb::Empty {}).await;
        assert!(
            after_a.is_err(),
            "shut-down connection must require re-binding before further workspace operations"
        );

        let b_status = client_b
            .get_status(anticrumb::rpc::pb::Empty {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            b_status.browser_connection.as_ref().map(|c| c.connected),
            Some(true),
            "other --connect client sharing server1 broke after sibling Shutdown"
        );
    });
    thread::sleep(Duration::from_millis(400));

    assert_eq!(record(client_root).unwrap()["pid"], daemon_pid);
    assert_eq!(
        lock_state(&client_root.join("run/daemon.lock")),
        Some(false)
    );
    assert!(!cdp.is_closed() && meta.exists());
    local.help();

    // 1b. Client B (now the sole client on server1) calling Shutdown cleanly tears down server1's
    // workspace Host without harming server2, shared daemon, or local Chrome, and a fresh caller
    // recreates a working server1 workspace cleanly.
    rt.block_on(async {
        client_b
            .shutdown(anticrumb::rpc::pb::Empty {})
            .await
            .unwrap();
    });

    let d2 = remote_doctor(
        client_root,
        client_root,
        &server2.endpoint,
        &server2.token,
        &server2.pin,
    );
    assert_eq!(d2["runtime"]["pid"], daemon_pid);
    let d1 = remote_doctor(
        client_root,
        client_root,
        &server1.endpoint,
        &server1.token,
        &server1.pin,
    );
    assert_eq!(d1["runtime"]["pid"], daemon_pid);
    assert_eq!(d1["browser_connection"]["connected"], true);

    // 2. PlatformService/Shutdown over a Connection::Local binding triggers local daemon shutdown
    // and shuts down managed Chrome cleanly.
    rt.block_on(async {
        let mut local_shutdown = connect_uds_client(endpoint, protocol, None).await;
        local_shutdown
            .shutdown(anticrumb::rpc::pb::Empty {})
            .await
            .unwrap();
    });
    wait_for(|| released(client_root));
    assert!(cdp.finish() && !meta.exists());

    drop(local);
    drop(remote2);
}
