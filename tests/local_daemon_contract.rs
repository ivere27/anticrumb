//! Real-process coverage of IPC ownership, concurrency, and automatic cleanup.
use fs2::FileExt;
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn command(root: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_anticrumb"));
    command
        .current_dir(cwd)
        .env("ANTICRUMB_HOME", root)
        .env("ANTICRUMB_TOKEN", "ignored-for-local-ipc")
        .env("ANTICRUMB_ENDPOINT", "https://127.0.0.1:1")
        .env("CHROME_BIN", std::env::current_exe().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
fn released(root: &Path) -> bool {
    if root.join("run/daemon.json").exists() {
        return false;
    }
    [
        "run/daemon.lock",
        "workspace/host.lock",
        "browser/host.lock",
    ]
    .iter()
    .all(|path| {
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(path))
        else {
            return false;
        };
        if file.try_lock_exclusive().is_err() {
            return false;
        }
        FileExt::unlock(&file).unwrap();
        true
    })
}
struct Session {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<String>,
}
impl Session {
    fn start(root: &Path, cwd: &Path) -> Self {
        let mut child = command(root, cwd)
            .args(["browser", "--json"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
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
        for state in ["workspace", "browser"] {
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
    let mut daemon = Daemon(command(root, root).arg("daemon").spawn().unwrap());
    wait_for(|| record(root).is_some());
    assert_eq!(record(root).unwrap()["managed"], false);
    let runtime = status(root, root)["runtime"].clone();
    assert_eq!(runtime["automatic"], false);
    assert_eq!(runtime["pid"], daemon.0.id());
    Session::start(root, root).exit();
    thread::sleep(Duration::from_millis(600));
    assert!(daemon.0.try_wait().unwrap().is_none());
    Session::start(root, root).exit();
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(daemon.0.id() as i32, libc::SIGTERM);
        }
        wait_for(|| daemon.0.try_wait().unwrap().is_some());
        assert!(daemon.0.wait().unwrap().success());
        assert!(released(root));
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
