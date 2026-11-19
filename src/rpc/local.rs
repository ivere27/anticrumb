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
    generation: u64,
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
        state.generation = state.generation.wrapping_add(1);
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
    fn mark_seen(&self) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            if state.seen {
                false
            } else {
                state.seen = true;
                state.generation = state.generation.wrapping_add(1);
                true
            }
        };
        if changed {
            self.changed.notify_one();
        }
    }
    async fn idle(&self) {
        self.idle_for(START_TIMEOUT, IDLE_GRACE).await
    }
    async fn idle_for(&self, start_timeout: Duration, idle_grace: Duration) {
        let startup = tokio::time::Instant::now() + start_timeout;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let target = {
                let state = self.state.lock().unwrap();
                (state.active == 0).then_some((state.generation, state.seen))
            };
            if let Some((expected_generation, seen)) = target {
                let deadline = if seen {
                    tokio::time::Instant::now() + idle_grace
                } else {
                    startup
                };
                tokio::select! {
                    _ = &mut changed => continue,
                    _ = tokio::time::sleep_until(deadline) => {
                        let mut state = self.state.lock().unwrap();
                        if state.active == 0 && state.generation == expected_generation {
                            state.stopping = true;
                            return;
                        }
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
            state.generation = state.generation.wrapping_add(1);
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

#[derive(Default)]
struct LocalClients {
    state: Mutex<LocalClientState>,
    changed: tokio::sync::Notify,
}
#[derive(Default)]
struct LocalClientState {
    active: usize,
    dirty: bool,
    generation: u64,
}
struct LocalLease {
    clients: Arc<LocalClients>,
}
impl LocalClients {
    fn acquire(self: &Arc<Self>) -> LocalLease {
        {
            let mut state = self.state.lock().unwrap();
            state.active += 1;
            state.dirty = true;
            state.generation = state.generation.wrapping_add(1);
        }
        self.changed.notify_one();
        LocalLease {
            clients: self.clone(),
        }
    }
    fn take_reclaimable(&self, expected_generation: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.active == 0 && state.dirty && state.generation == expected_generation {
            state.dirty = false;
            true
        } else {
            false
        }
    }
    async fn wait_idle(&self) -> u64 {
        self.wait_idle_for(IDLE_GRACE).await
    }
    async fn wait_idle_for(&self, grace: Duration) -> u64 {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let expected_generation = {
                let state = self.state.lock().unwrap();
                (state.active == 0 && state.dirty).then_some(state.generation)
            };
            if let Some(expected_generation) = expected_generation {
                let deadline = tokio::time::Instant::now() + grace;
                tokio::select! {
                    _ = &mut changed => continue,
                    _ = tokio::time::sleep_until(deadline) => {
                        let state = self.state.lock().unwrap();
                        if state.active == 0
                            && state.dirty
                            && state.generation == expected_generation
                        {
                            return expected_generation;
                        }
                    }
                }
            } else {
                changed.await;
            }
        }
    }
}
impl Drop for LocalLease {
    fn drop(&mut self) {
        {
            let mut state = self.clients.state.lock().unwrap();
            state.active = state.active.saturating_sub(1);
            state.generation = state.generation.wrapping_add(1);
        }
        self.clients.changed.notify_one();
    }
}

#[derive(Clone)]
pub(super) struct LocalInfo {
    workspaces: Arc<Workspaces>,
    clients: Arc<Clients>,
    runtime: Arc<RuntimeInfo>,
    daemon_stop: CancelToken,
    selected: Arc<tokio::sync::Mutex<Option<Binding>>>,
    pub(super) logged_first: Arc<std::sync::atomic::AtomicBool>,
}

impl LocalInfo {
    pub(super) fn is_automatic(&self) -> bool {
        self.clients.automatic
    }
    pub(super) async fn shutdown(&self, bytes: Vec<u8>) -> Result<Vec<u8>> {
        enum Target {
            Local(Arc<Host>),
            Grpc(Box<Binding>),
        }
        let target = {
            let mut selected = self.selected.lock().await;
            let is_local = selected
                .as_ref()
                .ok_or_else(|| {
                    ServiceError::invalid(
                        "call WorkspaceService.ConnectClient before workspace operations",
                    )
                })?
                .connection
                == Connection::Local;
            if is_local {
                Target::Local(selected.as_ref().unwrap().host.clone())
            } else {
                Target::Grpc(Box::new(selected.take().unwrap()))
            }
        };
        match target {
            Target::Local(host) => {
                let daemon_stop = self.daemon_stop.clone();
                let task = tokio::spawn(async move {
                    let response = host
                        .dispatch("/anticrumb.v1.PlatformService/Shutdown", bytes)
                        .await?;
                    daemon_stop.cancel();
                    Ok(response)
                });
                match task.await {
                    Ok(result) => result,
                    Err(error) => {
                        if error.is_panic() {
                            std::panic::resume_unwind(error.into_panic());
                        }
                        Err(ServiceError::new(
                            crate::domain::ErrorCode::Cancelled,
                            "shutdown",
                            format!("local daemon shutdown task cancelled: {error}"),
                        ))
                    }
                }
            }
            Target::Grpc(binding) => self.workspaces.shutdown_grpc_binding(*binding, bytes).await,
        }
    }
}
struct RuntimeInfo {
    started: Instant,
    automatic: bool,
    endpoint: PathBuf,
}
struct WorkspaceLease {
    workspaces: Arc<Workspaces>,
    root: PathBuf,
    host: Arc<Host>,
    armed: bool,
}
impl WorkspaceLease {
    fn disarm(&mut self) -> (PathBuf, Arc<Host>) {
        self.armed = false;
        (self.root.clone(), self.host.clone())
    }
}
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut runtimes = self.workspaces.runtimes.lock().unwrap();
        if let Some(workspace) = runtimes
            .get_mut(&self.root)
            .filter(|ws| Arc::ptr_eq(&ws.host, &self.host))
        {
            workspace.bindings = workspace.bindings.saturating_sub(1);
        }
    }
}
struct Binding {
    connection: Connection,
    host: Arc<Host>,
    workers: Option<u32>,
    pace: Option<String>,
    workspace_lease: WorkspaceLease,
    _local_lease: Option<LocalLease>,
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
        if let Some(previous) = selected.as_mut() {
            if previous.connection != connection {
                return Err(ServiceError::invalid(
                    "open a new client connection to change browser backends",
                ));
            }
            if previous.workers != workers || previous.pace != pace {
                if let Some(workers) = workers {
                    crate::tab_pool::set_worker_tabs(
                        Some(&previous.host.service.store),
                        workers as usize,
                    );
                }
                if let Some(pace) = pace.as_deref().and_then(crate::gemini::HumanPace::parse) {
                    crate::gemini::set_human_pace(Some(&previous.host.service.store), pace);
                }
                previous.workers = workers;
                previous.pace = pace;
            }
            self.clients.mark_seen();
            return Ok(previous.host.info());
        }
        let local_lease =
            (connection == Connection::Local).then(|| self.workspaces.acquire_local());
        let (host, workspace_lease) = self.workspaces.get(connection.clone()).await?;
        if let Some(workers) = workers {
            crate::tab_pool::set_worker_tabs(Some(&host.service.store), workers as usize);
        }
        if let Some(pace) = pace.as_deref().and_then(crate::gemini::HumanPace::parse) {
            crate::gemini::set_human_pace(Some(&host.service.store), pace);
        }
        self.clients.mark_seen();
        let info = host.info();
        *selected = Some(Binding {
            connection,
            host,
            workers,
            pace,
            workspace_lease,
            _local_lease: local_lease,
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

#[derive(Default)]
struct ClosingTasks {
    active: Mutex<usize>,
    idle: tokio::sync::Notify,
}
struct ClosingGuard {
    tasks: Arc<ClosingTasks>,
}
impl ClosingTasks {
    fn acquire(self: &Arc<Self>) -> ClosingGuard {
        *self.active.lock().unwrap() += 1;
        ClosingGuard {
            tasks: self.clone(),
        }
    }
    async fn wait_idle(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if *self.active.lock().unwrap() == 0 {
                return;
            }
            idle.await;
        }
    }
}
impl Drop for ClosingGuard {
    fn drop(&mut self) {
        let remaining = {
            let mut active = self.tasks.active.lock().unwrap();
            *active = active.saturating_sub(1);
            *active
        };
        if remaining == 0 {
            self.tasks.idle.notify_waiters();
        }
    }
}

struct Workspace {
    host: Arc<Host>,
    client: RpcClient,
    connection: Connection,
    bindings: usize,
}
struct Workspaces {
    runtimes: Mutex<HashMap<PathBuf, Workspace>>,
    root_locks: Mutex<HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    closing: Arc<ClosingTasks>,
    local_clients: Arc<LocalClients>,
    browser: Option<Arc<Host>>,
}
impl Workspaces {
    fn acquire_local(&self) -> LocalLease {
        self.local_clients.acquire()
    }
    fn root_lock(&self, root: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.root_locks.lock().unwrap();
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(lock) = locks.get(root).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
        lock
    }
    async fn get(self: &Arc<Self>, connection: Connection) -> Result<(Arc<Host>, WorkspaceLease)> {
        let root = workspace_root(&connection);
        let lock = self.root_lock(&root);
        let _root_guard = lock.lock().await;
        {
            let mut runtimes = self.runtimes.lock().unwrap();
            if let Some(workspace) = runtimes.get_mut(&root) {
                if workspace.connection != connection {
                    return Err(ServiceError::invalid("this browser connection changed; restart the local daemon to use the updated credentials"));
                }
                workspace.bindings += 1;
                let host = workspace.host.clone();
                let lease = WorkspaceLease {
                    workspaces: self.clone(),
                    root,
                    host: host.clone(),
                    armed: true,
                };
                return Ok((host, lease));
            }
        }
        let host =
            super::relay::workspace_with_browser(&root, connection.clone(), self.browser.clone())
                .await?;
        // Agents use the registered internal client and never acquire an IPC
        // lease of their own, so they cannot keep an automatic daemon alive.
        let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current())?;
        let lease = WorkspaceLease {
            workspaces: self.clone(),
            root: root.clone(),
            host: host.clone(),
            armed: true,
        };
        self.runtimes.lock().unwrap().insert(
            root,
            Workspace {
                host: host.clone(),
                client,
                connection,
                bindings: 1,
            },
        );
        Ok((host, lease))
    }
    async fn shutdown_grpc_binding(&self, mut binding: Binding, bytes: Vec<u8>) -> Result<Vec<u8>> {
        let root = workspace_root(&binding.connection);
        let lock = self.root_lock(&root);
        let guard = lock.lock_owned().await;
        let (_, host) = binding.workspace_lease.disarm();
        drop(binding);
        let removed = {
            let mut runtimes = self.runtimes.lock().unwrap();
            match runtimes.get_mut(&root) {
                Some(workspace) if Arc::ptr_eq(&workspace.host, &host) => {
                    workspace.bindings = workspace.bindings.saturating_sub(1);
                    if workspace.bindings == 0 {
                        runtimes.remove(&root)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        drop(host);
        let Some(workspace) = removed else {
            return Ok(prost::Message::encode_to_vec(&pb::Empty {}));
        };
        let closing_guard = self.closing.acquire();
        // Run shutdown/close inside a tracked spawned task holding `guard` so cancelling the RPC
        // mid-dispatch cannot drop `workspace` without closing it, release the root lock early,
        // or outlive `Workspaces::close`.
        let task = tokio::spawn(async move {
            let _closing = closing_guard;
            let _guard = guard;
            let workspace = workspace;
            let dispatch_result = workspace
                .host
                .dispatch("/anticrumb.v1.PlatformService/Shutdown", bytes)
                .await;
            workspace.host.shutdown();
            if let Err(error) = workspace.client.close().await {
                eprintln!(
                    "{} failed to close workspace host on shutdown: {error}",
                    super::server::log_timestamp()
                );
            }
            drop(workspace);
            dispatch_result
        });
        match task.await {
            Ok(result) => result,
            Err(error) => {
                if error.is_panic() {
                    std::panic::resume_unwind(error.into_panic());
                }
                Err(ServiceError::new(
                    crate::domain::ErrorCode::Cancelled,
                    "shutdown",
                    format!("workspace shutdown task cancelled: {error}"),
                ))
            }
        }
    }
    async fn reclaim_local(&self, expected_generation: u64) {
        if self.browser.is_some() {
            return;
        }
        let root = workspace_root(&Connection::Local);
        let lock = self.root_lock(&root);
        let _local_guard = lock.lock().await;
        if !self.local_clients.take_reclaimable(expected_generation) {
            return;
        }
        let workspace = self.runtimes.lock().unwrap().remove(&root);
        let Some(workspace) = workspace else {
            return;
        };
        workspace.host.shutdown();
        let browser_host = match &workspace.host.remote {
            Some(remote) => remote.embedded_host().await,
            None => None,
        };
        if let Err(error) = workspace.client.close().await {
            eprintln!(
                "{} failed to close reclaimed local workspace: {error}",
                super::server::log_timestamp()
            );
        }
        drop(workspace);
        if let Some(browser_host) = browser_host {
            if let Err(error) =
                tokio::task::spawn_blocking(move || browser_host.shutdown_managed_browsers()).await
            {
                eprintln!(
                    "{} failed to shut down managed local browsers: {error}",
                    super::server::log_timestamp()
                );
            }
        }
    }
    fn stop(&self) {
        let hosts: Vec<Arc<Host>> = self
            .runtimes
            .lock()
            .unwrap()
            .values()
            .map(|workspace| workspace.host.clone())
            .collect();
        for host in hosts {
            host.shutdown();
        }
    }
    async fn close(&self) -> Result<()> {
        let workspaces = std::mem::take(&mut *self.runtimes.lock().unwrap());
        let mut result = Ok(());
        for (_, workspace) in workspaces {
            if let Err(error) = workspace.client.close().await {
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        self.closing.wait_idle().await;
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
        root_locks: Default::default(),
        closing: Default::default(),
        local_clients: Default::default(),
        browser: browser.clone(),
    });
    if !managed {
        let _ = workspaces.get(Connection::Local).await?;
    }
    let clients = Arc::new(Clients::new(managed));
    let runtime = Arc::new(RuntimeInfo {
        started: Instant::now(),
        automatic: managed,
        endpoint: paths.endpoint.clone(),
    });
    let stop = CancelToken::new();
    let daemon_stop = CancelToken::new();
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
    let reclaim = (managed && browser.is_none()).then(|| {
        let workspaces = workspaces.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                let generation = tokio::select! {
                    _ = stop.cancelled() => break,
                    generation = workspaces.local_clients.wait_idle() => generation,
                };
                if stop.is_cancelled() {
                    break;
                }
                workspaces.reclaim_local(generation).await;
            }
        })
    });
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let accepting = {
        let workspaces = workspaces.clone();
        let clients = clients.clone();
        let stop = stop.clone();
        let daemon_stop = daemon_stop.clone();
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
                            daemon_stop: daemon_stop.clone(),
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
        let clients = clients.clone();
        let workspaces = workspaces.clone();
        let stop = stop.clone();
        let daemon_stop = daemon_stop.clone();
        let browser = browser.clone();
        async move {
            tokio::select! {
                _ = platform::shutdown_signal() => {},
                _ = daemon_stop.cancelled() => {},
                _ = clients.idle(), if managed => {},
                _ = async { match browser { Some(browser) => browser.service.shutdown.notified().await, None => std::future::pending().await } } => {},
            }
            clients.state.lock().unwrap().stopping = true;
            stop.cancel();
            workspaces.stop();
        }
    };
    let result =
        super::server::serve_local(tokio_stream::wrappers::ReceiverStream::new(rx), shutdown).await;
    stop.cancel();
    let _ = accepting.await;
    if let Some(reclaim) = reclaim {
        let _ = reclaim.await;
    }
    workspaces.stop();
    let mut local_browser_hosts = Vec::new();
    if let Some(browser) = browser {
        local_browser_hosts.push(browser);
    } else {
        let remotes: Vec<RpcClient> = {
            let runtimes = workspaces.runtimes.lock().unwrap();
            runtimes
                .values()
                .filter(|workspace| workspace.connection == Connection::Local)
                .filter_map(|workspace| workspace.host.remote.clone())
                .collect()
        };
        for remote in remotes {
            if let Some(browser_host) = remote.embedded_host().await {
                local_browser_hosts.push(browser_host);
            }
        }
    }
    let closed = workspaces.close().await;
    for browser_host in local_browser_hosts {
        if let Err(error) =
            tokio::task::spawn_blocking(move || browser_host.shutdown_managed_browsers()).await
        {
            eprintln!(
                "{} failed to shut down managed local browsers on exit: {error}",
                super::server::log_timestamp()
            );
        }
    }
    result.and(closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_clients_generation_prevents_bypassing_idle_grace_after_wait_returns() {
        let clients = Arc::new(LocalClients::default());
        let first = clients.acquire();
        drop(first);

        let stale_generation = clients.wait_idle().await;
        // Another short-lived client arrives and departs before reclaim executes.
        let second = clients.acquire();
        let quick_exit = tokio::time::Instant::now();
        drop(second);

        assert!(
            !clients.take_reclaimable(stale_generation),
            "stale generation from before second client must not bypass IDLE_GRACE"
        );

        let fresh_generation = clients.wait_idle().await;
        assert!(
            quick_exit.elapsed() >= IDLE_GRACE - Duration::from_millis(20),
            "second client did not receive a full IDLE_GRACE window: {:?}",
            quick_exit.elapsed()
        );
        assert!(clients.take_reclaimable(fresh_generation));
        assert!(
            !clients.take_reclaimable(fresh_generation),
            "already-reclaimed dirty bit must not trigger again"
        );
    }

    #[tokio::test]
    async fn local_clients_wait_idle_ignores_expired_timer_when_generation_advances() {
        let clients = Arc::new(LocalClients::default());
        let grace = Duration::from_millis(80);

        for _ in 0..6 {
            drop(clients.acquire());
            let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let waiter = tokio::spawn({
                let clients = clients.clone();
                let started = started.clone();
                async move {
                    started.store(true, std::sync::atomic::Ordering::Release);
                    clients.wait_idle_for(grace).await
                }
            });
            while !started.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            // Advance past `grace` without yielding back into the runtime, then quickly acquire
            // and drop so both `changed` and `sleep_until(deadline)` are ready when resumed.
            std::thread::sleep(grace + Duration::from_millis(10));
            let second = clients.acquire();
            let second_exit = tokio::time::Instant::now();
            drop(second);

            let generation = waiter.await.unwrap();
            assert!(
                second_exit.elapsed() >= grace - Duration::from_millis(15),
                "wait_idle returned early via stale select branch: {:?}",
                second_exit.elapsed()
            );
            assert!(clients.take_reclaimable(generation));
        }
    }

    #[tokio::test]
    async fn clients_idle_ignores_expired_timer_when_generation_advances() {
        let grace = Duration::from_millis(80);

        for _ in 0..6 {
            let clients = Arc::new(Clients::new(true));
            let lease = clients.acquire().unwrap();
            clients.mark_seen();
            drop(lease);

            let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let waiter = tokio::spawn({
                let clients = clients.clone();
                let started = started.clone();
                async move {
                    started.store(true, std::sync::atomic::Ordering::Release);
                    clients.idle_for(Duration::from_secs(5), grace).await;
                }
            });
            while !started.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            std::thread::sleep(grace + Duration::from_millis(10));
            let second = clients.acquire().unwrap();
            let second_exit = tokio::time::Instant::now();
            drop(second);

            waiter.await.unwrap();
            assert!(
                second_exit.elapsed() >= grace - Duration::from_millis(15),
                "Clients::idle returned early via stale select branch: {:?}",
                second_exit.elapsed()
            );
            assert!(clients.state.lock().unwrap().stopping);
            assert!(clients.acquire().is_none());
        }
    }

    fn test_workspaces() -> Arc<Workspaces> {
        Arc::new(Workspaces {
            runtimes: Default::default(),
            root_locks: Default::default(),
            closing: Default::default(),
            local_clients: Default::default(),
            browser: None,
        })
    }

    fn test_host(dir: &Path) -> (Arc<Host>, RpcClient) {
        let store = crate::store::Store::open(dir).unwrap();
        let backend = Arc::new(crate::service::WebBackend::new(store.clone()));
        let service = crate::service::Service::new(store, backend).unwrap();
        let host = Host::with_service(service, None, "workspace", Some(Host::lease(dir).unwrap()))
            .unwrap();
        let client = RpcClient::embedded(host.clone(), tokio::runtime::Handle::current()).unwrap();
        (host, client)
    }

    #[tokio::test]
    async fn binding_drops_workspace_lease_before_local_lease() {
        let dir = tempfile::tempdir().unwrap();
        let workspaces = test_workspaces();
        let (host, client) = test_host(dir.path());
        let root = dir.path().to_path_buf();
        workspaces.runtimes.lock().unwrap().insert(
            root.clone(),
            Workspace {
                host: host.clone(),
                client,
                connection: Connection::Local,
                bindings: 1,
            },
        );
        let binding = Binding {
            connection: Connection::Local,
            host: host.clone(),
            workers: None,
            pace: None,
            workspace_lease: WorkspaceLease {
                workspaces: workspaces.clone(),
                root: root.clone(),
                host,
                armed: true,
            },
            _local_lease: Some(workspaces.acquire_local()),
        };

        let runtimes_guard = workspaces.runtimes.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            drop(binding);
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            workspaces.local_clients.state.lock().unwrap().active,
            1,
            "LocalLease dropped before WorkspaceLease"
        );
        drop(runtimes_guard);
        dropper.join().unwrap();

        assert_eq!(workspaces.runtimes.lock().unwrap()[&root].bindings, 0);
        assert_eq!(workspaces.local_clients.state.lock().unwrap().active, 0);
        workspaces.close().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_grpc_binding_cancellation_completes_close_and_serializes_get() {
        let dir = tempfile::tempdir().unwrap();
        let workspaces = test_workspaces();
        let (host, client) = test_host(dir.path());
        let service = host.service.clone();
        let cache_cancel = host.cache_cancel.clone();
        let connection = Connection::Grpc {
            endpoint: format!(
                "https://shutdown-cancel-{}.invalid:50051",
                uuid::Uuid::new_v4().simple()
            ),
            token: "test-token-0123456789abcdef0123456789".into(),
            ca_file: None,
            server_name: None,
            tls_pin: None,
        };
        let root = workspace_root(&connection);
        workspaces.runtimes.lock().unwrap().insert(
            root.clone(),
            Workspace {
                host: host.clone(),
                client: client.clone(),
                connection: connection.clone(),
                bindings: 1,
            },
        );
        let binding = Binding {
            connection,
            host: host.clone(),
            workers: None,
            pace: None,
            workspace_lease: WorkspaceLease {
                workspaces: workspaces.clone(),
                root: root.clone(),
                host,
                armed: true,
            },
            _local_lease: None,
        };

        let mut fut = Box::pin(
            workspaces.shutdown_grpc_binding(binding, prost::Message::encode_to_vec(&pb::Empty {})),
        );
        let poll = fut
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        assert!(poll.is_pending());
        assert!(!workspaces.runtimes.lock().unwrap().contains_key(&root));

        drop(fut);
        assert!(
            workspaces.root_lock(&root).try_lock().is_err(),
            "root_lock must remain held by the spawned shutdown task after RPC cancellation"
        );
        assert_eq!(*workspaces.closing.active.lock().unwrap(), 1);

        workspaces.close().await.unwrap();
        assert_eq!(*workspaces.closing.active.lock().unwrap(), 0);
        assert!(workspaces.root_lock(&root).try_lock().is_ok());
        assert!(cache_cancel.is_cancelled());
        assert_eq!(
            service
                .submit(crate::domain::TurnRequest::text("ping"))
                .unwrap_err()
                .message,
            "host is stopping"
        );
        assert!(client.embedded_host().await.is_none());
        drop(Host::lease(dir.path()).unwrap());
    }

    #[tokio::test]
    async fn local_shutdown_cancellation_still_cancels_daemon_stop() {
        let dir = tempfile::tempdir().unwrap();
        let workspaces = test_workspaces();
        let (host, client) = test_host(dir.path());
        let root = dir.path().to_path_buf();
        workspaces.runtimes.lock().unwrap().insert(
            root.clone(),
            Workspace {
                host: host.clone(),
                client,
                connection: Connection::Local,
                bindings: 1,
            },
        );
        let daemon_stop = CancelToken::new();
        let info = LocalInfo {
            workspaces: workspaces.clone(),
            clients: Arc::new(Clients::new(false)),
            runtime: Arc::new(RuntimeInfo {
                started: Instant::now(),
                automatic: false,
                endpoint: dir.path().join("daemon.sock"),
            }),
            daemon_stop: daemon_stop.clone(),
            selected: Arc::new(tokio::sync::Mutex::new(Some(Binding {
                connection: Connection::Local,
                host: host.clone(),
                workers: None,
                pace: None,
                workspace_lease: WorkspaceLease {
                    workspaces: workspaces.clone(),
                    root,
                    host,
                    armed: true,
                },
                _local_lease: Some(workspaces.acquire_local()),
            }))),
            logged_first: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let mut fut = Box::pin(info.shutdown(prost::Message::encode_to_vec(&pb::Empty {})));
        let poll = fut
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        assert!(poll.is_pending());
        drop(fut);

        tokio::time::timeout(Duration::from_secs(2), daemon_stop.cancelled())
            .await
            .unwrap();
        workspaces.close().await.unwrap();
    }

    #[tokio::test]
    async fn local_info_host_and_shutdown_share_invalid_request_error_when_unbound() {
        let dir = tempfile::tempdir().unwrap();
        let info = LocalInfo {
            workspaces: test_workspaces(),
            clients: Arc::new(Clients::new(true)),
            runtime: Arc::new(RuntimeInfo {
                started: Instant::now(),
                automatic: true,
                endpoint: dir.path().join("daemon.sock"),
            }),
            daemon_stop: CancelToken::new(),
            selected: Default::default(),
            logged_first: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let host_err = info.host().await.err().unwrap();
        let shutdown_err = info.shutdown(Vec::new()).await.err().unwrap();
        assert_eq!(host_err.code, crate::domain::ErrorCode::InvalidRequest);
        assert_eq!(shutdown_err.code, crate::domain::ErrorCode::InvalidRequest);
        assert_eq!(host_err.message, shutdown_err.message);
    }
}
