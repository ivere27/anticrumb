//! Same-user IPC and lifecycle for the shared local workspace daemon.
//! Connections, not client-supplied PIDs or heartbeat files, own its lifetime.
mod platform;

use super::{
    client::{Connection, RpcClient},
    host::Host,
    pb,
};
use crate::{
    agent::CancelToken,
    domain::{digest, Result, ServiceError},
};
use fs2::FileExt;
use futures_util::Future;
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_GRACE: Duration = Duration::from_millis(300);

fn protocol() -> String {
    format!(
        "{}:{}",
        env!("CARGO_PKG_VERSION"),
        digest(include_bytes!("../../proto/anticrumb.proto"))
    )
}

pub(crate) fn workspace_root(connection: &Connection) -> PathBuf {
    match connection {
        Connection::Local => crate::cli::app_dir().join("workspace"),
        Connection::Grpc {
            endpoint,
            server_name,
            ..
        } => {
            // Credentials can rotate without losing local history. Distinct
            // remote browser servers must never recover each other's jobs.
            let key = digest(format!("{endpoint}\n{server_name:?}").as_bytes());
            crate::cli::app_dir().join("workspaces").join(&key[..24])
        }
    }
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(ServiceError::storage)?;
    let metadata = fs::symlink_metadata(path).map_err(ServiceError::storage)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ServiceError::invalid(
            "local runtime directory must be a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(ServiceError::invalid(
                "local runtime directory belongs to another user",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(ServiceError::storage)?;
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct Paths {
    pub directory: PathBuf,
    pub endpoint: PathBuf,
}
impl Paths {
    fn new() -> Result<Self> {
        let directory = crate::cli::app_dir().join("run");
        private_directory(&directory)?;
        let directory = fs::canonicalize(directory).map_err(ServiceError::storage)?;
        let home = fs::canonicalize(crate::cli::app_dir()).map_err(ServiceError::storage)?;
        let endpoint = platform::endpoint(&home, &directory)?;
        Ok(Self {
            directory,
            endpoint,
        })
    }
    fn lock(&self, name: &str) -> Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options
            .open(self.directory.join(name))
            .map_err(ServiceError::storage)
    }
}

fn configuration(connection: &Connection) -> pb::ConnectClientRequest {
    let remote = match connection {
        Connection::Local => None,
        Connection::Grpc {
            endpoint,
            token,
            ca_file,
            server_name,
            tls_pin,
        } => Some(pb::RemoteBrowser {
            endpoint: endpoint.clone(),
            token: token.clone(),
            ca_file: ca_file.as_ref().map(|path| {
                fs::canonicalize(path)
                    .unwrap_or_else(|_| path.clone())
                    .to_string_lossy()
                    .into_owned()
            }),
            server_name: server_name.clone(),
            tls_pin: tls_pin.clone(),
        }),
    };
    pb::ConnectClientRequest {
        protocol: protocol(),
        remote,
        workers: crate::tab_pool::worker_override(),
        pace: crate::gemini::pace_override(),
    }
}
fn connection(request: pb::ConnectClientRequest) -> Result<Connection> {
    if request.protocol != protocol() {
        return Err(ServiceError::invalid("local daemon version differs from this executable; close its clients and restart the daemon"));
    }
    if request
        .workers
        .is_some_and(|value| !(1..=16).contains(&value))
    {
        return Err(ServiceError::invalid("workers must be 1..16"));
    }
    if let Some(pace) = &request.pace {
        if crate::gemini::HumanPace::parse(pace).is_none() {
            return Err(ServiceError::invalid("pace must be fast, low, or high"));
        }
    }
    Ok(match request.remote {
        Some(remote) => Connection::Grpc {
            endpoint: remote.endpoint,
            token: remote.token,
            ca_file: remote.ca_file.map(Into::into),
            server_name: remote.server_name,
            tls_pin: remote.tls_pin,
        },
        None => Connection::Local,
    })
}

/// Ensure one daemon exists. Only the startup lock winner launches a child;
/// the child takes the independent lifetime lock before reclaiming its socket.
async fn ensure(paths: &Paths) -> Result<()> {
    if platform::connect(&paths.endpoint).await.is_ok() {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let start = paths.lock("start.lock")?;
    loop {
        if start.try_lock_exclusive().is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ServiceError::invalid(
                "timed out waiting for local daemon startup",
            ));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    if platform::connect(&paths.endpoint).await.is_ok() {
        return Ok(());
    }
    let owner = paths.lock("daemon.lock")?;
    let mut child = None;
    loop {
        if let Some(process) = child.as_mut() {
            let process: &mut std::process::Child = process;
            if let Some(status) = process.try_wait().map_err(ServiceError::storage)? {
                return Err(ServiceError::invalid(format!(
                    "local daemon exited ({status}); see {}",
                    paths.directory.join("daemon.log").display()
                )));
            }
        } else if owner.try_lock_exclusive().is_ok() {
            FileExt::unlock(&owner).map_err(ServiceError::storage)?;
            let log = paths.directory.join("daemon.log");
            super::private_write(&log, b"")?;
            let log = fs::OpenOptions::new()
                .append(true)
                .open(log)
                .map_err(ServiceError::storage)?;
            let mut command = Command::new(std::env::current_exe().map_err(ServiceError::storage)?);
            command
                .args(["daemon", "--managed"])
                .current_dir(crate::cli::app_dir())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log);
            // ANTICRUMB_HOME may have been relative to the invoking terminal.
            command.env(
                "ANTICRUMB_HOME",
                fs::canonicalize(crate::cli::app_dir()).map_err(ServiceError::storage)?,
            );
            platform::detach(&mut command);
            child = Some(command.spawn().map_err(ServiceError::storage)?);
        }
        if platform::connect(&paths.endpoint).await.is_ok() {
            if let Some(mut child) = child {
                // Reap on platforms where the launching client remains alive.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            // A launched child has its own startup idle deadline and will exit
            // if the caller disappears before connecting.
            return Err(ServiceError::invalid(format!(
                "timed out starting local daemon; see {}",
                paths.directory.join("daemon.log").display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

pub(crate) async fn connect(backend: &Connection) -> Result<RpcClient> {
    let paths = Paths::new()?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut attempts = 0;
    loop {
        attempts += 1;
        ensure(&paths).await?;
        let result = tokio::time::timeout_at(deadline, connect_once(&paths, backend))
            .await
            .map_err(|_| {
                ServiceError::invalid("local daemon did not become ready within 30 seconds")
            })?;
        match result {
            Err(error)
                if error.code == crate::domain::ErrorCode::TransportDisconnected
                    && attempts < 3
                    && tokio::time::Instant::now() + Duration::from_millis(100) < deadline =>
            {
                // The last previous client may have left just before this
                // one arrived. Only connection setup is repeated, no work.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            result => return result,
        }
    }
}

async fn connect_once(paths: &Paths, backend: &Connection) -> Result<RpcClient> {
    let configuration = configuration(backend);
    let cancel = CancelToken::new();
    let closing = cancel.clone();
    let endpoint = paths.endpoint.clone();
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_timeout(START_TIMEOUT)
        .connect_with_connector(tower::service_fn(move |_| {
            let endpoint = endpoint.clone();
            let cancel = closing.clone();
            async move {
                if cancel.is_cancelled() {
                    return Err(io::Error::other("local client closed"));
                }
                let socket = platform::connect(&endpoint).await?;
                Ok::<_, io::Error>(hyper_util::rt::TokioIo::new(ClientIo {
                    socket,
                    closed: false,
                    cancelled: Box::pin(async move { cancel.cancelled().await }),
                }))
            }
        }))
        .await
        .map_err(|error| {
            ServiceError::new(
                crate::domain::ErrorCode::TransportDisconnected,
                "local_connect",
                format!(
                    "local daemon connection failed: {}",
                    super::client::error_chain(&error)
                ),
            )
        })?;
    let client = RpcClient::local(
        channel,
        ClientConnection {
            cancel,
            configuration: Mutex::new(configuration),
        },
    );
    client.get_info(pb::Empty {}).await?;
    Ok(client)
}

pub(crate) struct ClientConnection {
    pub cancel: CancelToken,
    pub configuration: Mutex<pb::ConnectClientRequest>,
}
impl Drop for ClientConnection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Explicit close must interrupt pending HTTP/2 reads, including cloned clients
// and streaming watchers; dropping a tonic Channel alone is not sufficient.
struct ClientIo<S> {
    socket: S,
    closed: bool,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
}
impl<S: AsyncRead + Unpin> AsyncRead for ClientIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.closed || self.cancelled.as_mut().poll(cx).is_ready() {
            self.closed = true;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.socket).poll_read(cx, buffer)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for ClientIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.closed || self.cancelled.as_mut().poll(cx).is_ready() {
            self.closed = true;
            return Poll::Ready(Err(io::Error::other("local client closed")));
        }
        Pin::new(&mut self.socket).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}

#[derive(Default)]
struct Clients {
    state: Mutex<ClientState>,
    changed: tokio::sync::Notify,
    automatic: bool,
}
#[derive(Default)]
struct ClientState {
    active: usize,
    seen: bool,
    stopping: bool,
}
struct Lease {
    clients: Arc<Clients>,
    started: std::time::Instant,
}
impl Clients {
    fn new(automatic: bool) -> Self {
        Self {
            state: Mutex::new(ClientState::default()),
            changed: tokio::sync::Notify::new(),
            automatic,
        }
    }
    fn acquire(self: &Arc<Self>) -> Option<Lease> {
        let mut state = self.state.lock().unwrap();
        if state.stopping {
            return None;
        }
        state.active += 1;
        let active = state.active;
        drop(state);
        self.changed.notify_one();
        if !self.automatic {
            eprintln!(
                "{} client connected: local [active: {}]",
                super::server::log_timestamp(),
                active
            );
        }
        Some(Lease {
            clients: self.clone(),
            started: std::time::Instant::now(),
        })
    }
    async fn idle(&self) {
        let startup = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            let changed = self.changed.notified();
            let deadline = {
                let state = self.state.lock().unwrap();
                if state.active == 0 {
                    Some(if state.seen {
                        tokio::time::Instant::now() + IDLE_GRACE
                    } else {
                        startup
                    })
                } else {
                    None
                }
            };
            if let Some(deadline) = deadline {
                tokio::select! {
                    _ = changed => continue,
                    _ = tokio::time::sleep_until(deadline) => {
                        let mut state = self.state.lock().unwrap();
                        if state.active == 0 { state.stopping = true; return; }
                    }
                }
            } else {
                changed.await;
            }
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let active = {
            let mut state = self.clients.state.lock().unwrap();
            state.active = state.active.saturating_sub(1);
            state.active
        };
        self.clients.changed.notify_one();
        if !self.clients.automatic {
            let duration = self.started.elapsed();
            eprintln!(
                "{} client disconnected: local (duration: {}) [active: {}]",
                super::server::log_timestamp(),
                super::server::format_duration(duration),
                active
            );
        }
    }
}

#[derive(Clone)]
pub(super) struct LocalInfo {
    workspaces: Arc<Workspaces>,
    clients: Arc<Clients>,
    runtime: Arc<RuntimeInfo>,
    selected: Arc<tokio::sync::Mutex<Option<Binding>>>,
    pub(super) logged_first: Arc<std::sync::atomic::AtomicBool>,
}

impl LocalInfo {
    pub(super) fn is_automatic(&self) -> bool {
        self.clients.automatic
    }
}
struct RuntimeInfo {
    started: Instant,
    automatic: bool,
    endpoint: PathBuf,
}
struct Binding {
    connection: Connection,
    host: Arc<Host>,
    workers: Option<u32>,
    pace: Option<String>,
}
impl LocalInfo {
    pub fn status(&self) -> Result<pb::RuntimeStatus> {
        let clients = self.clients.state.lock().map_err(ServiceError::storage)?;
        Ok(pb::RuntimeStatus {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            automatic: self.runtime.automatic,
            transport: if cfg!(windows) { "named_pipe" } else { "unix" }.into(),
            endpoint: self.runtime.endpoint.to_string_lossy().into_owned(),
            uptime_ms: self
                .runtime
                .started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            connected_clients: clients.active.try_into().unwrap_or(u32::MAX),
            stopping: clients.stopping,
        })
    }
    pub async fn bind(&self, request: pb::ConnectClientRequest) -> Result<pb::HostInfo> {
        let workers = request.workers;
        let pace = request.pace.clone();
        let connection = connection(request)?;
        let mut selected = self.selected.lock().await;
        let host = if let Some(previous) = &*selected {
            if previous.connection != connection {
                return Err(ServiceError::invalid(
                    "open a new client connection to change browser backends",
                ));
            }
            if previous.workers == workers && previous.pace == pace {
                return Ok(previous.host.info());
            }
            previous.host.clone()
        } else {
            self.workspaces.get(connection.clone()).await?
        };
        if let Some(workers) = workers {
            crate::tab_pool::set_worker_tabs(Some(&host.service.store), workers as usize);
        }
        if let Some(pace) = pace.as_deref().and_then(crate::gemini::HumanPace::parse) {
            crate::gemini::set_human_pace(Some(&host.service.store), pace);
        }
        self.clients.state.lock().unwrap().seen = true;
        let info = host.info();
        *selected = Some(Binding {
            connection,
            host,
            workers,
            pace,
        });
        Ok(info)
    }
    pub async fn host(&self) -> Result<Arc<Host>> {
        self.selected
            .lock()
            .await
            .as_ref()
            .map(|binding| binding.host.clone())
            .ok_or_else(|| {
                ServiceError::invalid(
                    "call WorkspaceService.ConnectClient before workspace operations",
                )
            })
    }
}
struct ServerIo<S> {
    socket: S,
    info: LocalInfo,
    _lease: Lease,
}
impl<S> tonic::transport::server::Connected for ServerIo<S> {
    type ConnectInfo = LocalInfo;
    fn connect_info(&self) -> LocalInfo {
        self.info.clone()
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for ServerIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_read(cx, buffer)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for ServerIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.socket).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}

struct Workspace {
    host: Arc<Host>,
    client: RpcClient,
    connection: Connection,
}
struct Workspaces {
    runtimes: tokio::sync::Mutex<HashMap<PathBuf, Workspace>>,
    browser: Option<Arc<Host>>,
}
impl Workspaces {
    async fn get(&self, connection: Connection) -> Result<Arc<Host>> {
        let root = workspace_root(&connection);
        let mut workspaces = self.runtimes.lock().await;
        if let Some(workspace) = workspaces.get(&root) {
            if workspace.connection != connection {
                return Err(ServiceError::invalid("this browser connection changed; restart the local daemon to use the updated credentials"));
            }
            return Ok(workspace.host.clone());
        }
        let host =
            super::relay::workspace_with_browser(&root, connection.clone(), self.browser.clone())
                .await?;
        // Agents use the registered internal client and never acquire an IPC
        // lease of their own, so they cannot keep an automatic daemon alive.
        let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current())?;
        workspaces.insert(
            root,
            Workspace {
                host: host.clone(),
                client,
                connection,
            },
        );
        Ok(host)
    }
    async fn stop(&self) {
        for workspace in self.runtimes.lock().await.values() {
            workspace.host.shutdown();
        }
    }
    async fn close(&self) -> Result<()> {
        let workspaces = std::mem::take(&mut *self.runtimes.lock().await);
        let mut result = Ok(());
        for (_, workspace) in workspaces {
            if let Err(error) = workspace.client.close().await {
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        result
    }
}

struct Cleanup(Paths);
impl Drop for Cleanup {
    fn drop(&mut self) {
        platform::remove_endpoint(&self.0.endpoint);
        #[cfg(unix)]
        if let Some(parent) = self
            .0
            .endpoint
            .parent()
            .filter(|path| *path != self.0.directory)
        {
            let _ = fs::remove_dir(parent);
        }
        let _ = fs::remove_file(self.0.directory.join("daemon.json"));
    }
}

pub(crate) async fn run(managed: bool, browser: Option<Arc<Host>>) -> Result<()> {
    let paths = Paths::new()?;
    let owner = paths.lock("daemon.lock")?;
    owner.try_lock_exclusive().map_err(|_| {
        ServiceError::invalid("a local daemon is already running; run `anticrumb` to connect")
    })?;
    let mut listener = platform::Listener::bind(&paths.endpoint)?;
    if !managed && browser.is_none() {
        eprintln!(
            "{} {}",
            super::server::log_timestamp(),
            crate::cli::help_banner()
        );
    }
    let _cleanup = Cleanup(paths.clone());
    let workspaces = Arc::new(Workspaces {
        runtimes: Default::default(),
        browser: browser.clone(),
    });
    let host = workspaces.get(Connection::Local).await?;
    let clients = Arc::new(Clients::new(managed));
    let runtime = Arc::new(RuntimeInfo {
        started: Instant::now(),
        automatic: managed,
        endpoint: paths.endpoint.clone(),
    });
    let stop = CancelToken::new();
    let record = serde_json::json!({ "pid": std::process::id(), "managed": managed, "endpoint": paths.endpoint, "protocol": protocol() });
    super::private_write(
        &paths.directory.join("daemon.json"),
        &serde_json::to_vec(&record).map_err(ServiceError::storage)?,
    )?;
    eprintln!(
        "anticrumb local daemon: {} ({})",
        paths.endpoint.display(),
        if managed { "automatic" } else { "persistent" }
    );
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let accepting = {
        let workspaces = workspaces.clone();
        let clients = clients.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    accepted = listener.accept() => {
                        let socket = match accepted { Ok(socket) => socket, Err(error) => { let _ = tx.send(Err(error)).await; break; } };
                        let Some(lease) = clients.acquire() else { break; };
                        let info = LocalInfo {
                            workspaces: workspaces.clone(),
                            clients: clients.clone(),
                            runtime: runtime.clone(),
                            selected: Default::default(),
                            logged_first: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        };
                        let accepted = ServerIo { socket, info, _lease: lease };
                        tokio::select! {
                            _ = stop.cancelled() => break,
                            result = tx.send(Ok(accepted)) => { if result.is_err() { break; } }
                        }
                    }
                }
            }
        })
    };
    let shutdown = {
        let host = host.clone();
        let workspaces = workspaces.clone();
        let stop = stop.clone();
        async move {
            tokio::select! {
                _ = platform::shutdown_signal() => {},
                _ = host.service.shutdown.notified() => {},
                _ = clients.idle(), if managed => {},
                _ = async { match browser { Some(browser) => browser.service.shutdown.notified().await, None => std::future::pending().await } } => {},
            }
            clients.state.lock().unwrap().stopping = true;
            stop.cancel();
            workspaces.stop().await;
        }
    };
    let result = super::server::serve_local(
        host,
        tokio_stream::wrappers::ReceiverStream::new(rx),
        shutdown,
    )
    .await;
    stop.cancel();
    let _ = accepting.await;
    workspaces.stop().await;
    let closed = workspaces.close().await;
    result.and(closed)
}
