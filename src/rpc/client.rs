use super::{
    codec, pb, stream_chunks_to_sink, ArtifactChunkSink, ChunkPayload, ChunkStreamSpec,
    ARTIFACT_CHUNK_BYTES, MAX_ARTIFACT, MAX_MESSAGE, MAX_MESSAGE_LABEL,
};
use crate::domain::{
    digest, is_valid_artifact_name, is_valid_sha256_hex, ErrorCode, Result, ServiceError,
    MAX_ARTIFACT_LABEL,
};
use futures_util::{Stream, StreamExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Connection {
    Grpc {
        endpoint: String,
        token: String,
        ca_file: Option<PathBuf>,
        server_name: Option<String>,
        tls_pin: Option<String>,
    },
    Local,
}
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description())
    }
}
impl Connection {
    pub(super) fn description(&self) -> String {
        match self {
            Self::Grpc { endpoint, .. } => {
                format!("gRPC transport endpoint={}", endpoint_description(endpoint))
            }
            Self::Local => "local daemon".into(),
        }
    }
}

// Connection diagnostics are local, but URLs may still contain credentials or
// signed query parameters. Only the origin is useful for identifying the server.
pub(crate) fn endpoint_description(endpoint: &str) -> String {
    match crate::url_lite::Url::parse(endpoint) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {
            url.origin().ascii_serialization()
        }
        _ => "<invalid endpoint>".into(),
    }
}

fn connection_error(mut error: ServiceError, description: &str) -> ServiceError {
    error.message = format!("{description}: {}", error.message);
    error
}

pub(super) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let detail = cause.to_string();
        if !message.ends_with(&detail) {
            message.push_str(": ");
            message.push_str(&detail);
        }
        source = cause.source();
    }
    message
}

enum Transport {
    Grpc {
        channel: Channel,
        token: Option<String>,
        local: Option<Box<super::local::ClientConnection>>,
    },
    Embedded {
        module: synurang_host::ModuleHost,
        host: tokio::sync::Mutex<Option<Arc<super::host::Host>>>,
    },
}
#[derive(Clone)]
pub struct RpcClient {
    transport: Arc<Transport>,
}
pub(crate) struct WeakRpcClient(std::sync::Weak<Transport>);
impl WeakRpcClient {
    pub(crate) fn upgrade(&self) -> Option<RpcClient> {
        self.0.upgrade().map(|transport| RpcClient { transport })
    }
}
pub type EventStream = Pin<Box<dyn Stream<Item = Result<pb::Event>> + Send>>;
pub type ArtifactStream = Pin<Box<dyn Stream<Item = Result<pb::GetArtifactChunk>> + Send>>;
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactBytes {
    pub artifact: pb::Artifact,
    pub data: Vec<u8>,
}
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(45);
const ARTIFACT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

fn unary_timeout(path: &str) -> Duration {
    match path {
        // Deletion includes a fresh complete history read to reject optimistic
        // UI removals and partial pagination, plus browser/account-gate time.
        "/anticrumb.v1.SessionService/DeleteConversation" => Duration::from_secs(180),
        // Include bounded browser startup, account-gate wait, and tab cleanup.
        "/anticrumb.v1.ModelService/RefreshCatalog" => {
            crate::account::MODEL_DISCOVERY_TIMEOUT + Duration::from_secs(30)
        }
        "/anticrumb.v1.AccountService/GetWebUsage" => {
            crate::account::WEB_USAGE_TIMEOUT + Duration::from_secs(30)
        }
        "/anticrumb.v1.AccountService/BrowserFetch"
        | "/anticrumb.v1.AccountService/BrowserControl" => {
            crate::account::BROWSER_FETCH_TIMEOUT + Duration::from_secs(30)
        }
        _ => DEFAULT_RPC_TIMEOUT,
    }
}

impl RpcClient {
    pub async fn connect(connection: &Connection) -> Result<Self> {
        Self::connect_inner(connection)
            .await
            .map_err(|error| connection_error(error, &connection.description()))
    }

    async fn connect_inner(connection: &Connection) -> Result<Self> {
        match connection {
            Connection::Grpc {
                endpoint,
                token,
                ca_file,
                server_name,
                tls_pin,
            } => {
                let url = crate::url_lite::Url::parse(endpoint).map_err(|_| {
                    ServiceError::invalid("remote gRPC endpoint must be an https URL")
                })?;
                if url.scheme() != "https" {
                    return Err(ServiceError::invalid(
                        "remote gRPC connections require TLS; use an https:// endpoint",
                    ));
                }
                Self::network_inner(
                    endpoint,
                    token,
                    ca_file.as_deref(),
                    server_name.as_deref(),
                    tls_pin.as_deref(),
                )
                .await
            }
            Connection::Local => super::local::connect(connection).await,
        }
    }
    pub fn embedded(host: Arc<super::host::Host>, runtime: tokio::runtime::Handle) -> Result<Self> {
        let module = super::ffi::embedded(host.clone(), runtime.clone())?;
        let client = Self {
            transport: Arc::new(Transport::Embedded {
                module,
                host: tokio::sync::Mutex::new(Some(host.clone())),
            }),
        };
        if host.role == "workspace" {
            super::workspace_client::register(host.service.store.root(), &client, runtime)?;
        }
        Ok(client)
    }
    pub(crate) fn downgrade(&self) -> WeakRpcClient {
        WeakRpcClient(Arc::downgrade(&self.transport))
    }
    pub(crate) fn local(channel: Channel, connection: super::local::ClientConnection) -> Self {
        Self {
            transport: Arc::new(Transport::Grpc {
                channel,
                token: None,
                local: Some(Box::new(connection)),
            }),
        }
    }
    pub(crate) async fn set_pace(&self, pace: crate::gemini::HumanPace) -> Result<()> {
        let Transport::Grpc {
            channel,
            local: Some(local),
            ..
        } = self.transport.as_ref()
        else {
            return Err(ServiceError::invalid(
                "runtime settings require a local daemon connection",
            ));
        };
        local
            .configuration
            .lock()
            .map_err(ServiceError::storage)?
            .pace = Some(pace.as_str().into());
        prepare_local(channel, Some(local)).await
    }

    pub async fn network(
        endpoint: &str,
        token: &str,
        ca_file: Option<&std::path::Path>,
        server_name: Option<&str>,
    ) -> Result<Self> {
        Self::network_pinned(endpoint, token, ca_file, server_name, None).await
    }

    pub async fn network_pinned(
        endpoint: &str,
        token: &str,
        ca_file: Option<&std::path::Path>,
        server_name: Option<&str>,
        tls_pin: Option<&str>,
    ) -> Result<Self> {
        Self::network_inner(endpoint, token, ca_file, server_name, tls_pin)
            .await
            .map_err(|error| {
                connection_error(
                    error,
                    &format!("gRPC transport endpoint={}", endpoint_description(endpoint)),
                )
            })
    }

    async fn network_inner(
        endpoint: &str,
        token: &str,
        ca_file: Option<&std::path::Path>,
        server_name: Option<&str>,
        tls_pin: Option<&str>,
    ) -> Result<Self> {
        if token.is_empty() {
            return Err(ServiceError::invalid("network token must not be empty"));
        }
        let url = crate::url_lite::Url::parse(endpoint).map_err(ServiceError::storage)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ServiceError::invalid(
                "gRPC endpoint must be an http(s) URL without credentials",
            ));
        }
        if url.scheme() == "http"
            && (ca_file.is_some() || server_name.is_some() || tls_pin.is_some())
        {
            return Err(ServiceError::invalid(
                "TLS settings require an https endpoint",
            ));
        }
        if let Some(pin) = tls_pin {
            let channel = super::tls::connect(&url, pin, ca_file, server_name).await?;
            return Ok(Self {
                transport: Arc::new(Transport::Grpc {
                    channel,
                    token: Some(token.into()),
                    local: None,
                }),
            });
        }
        let mut builder = Endpoint::from_shared(endpoint.to_string())
            .map_err(ServiceError::storage)?
            .connect_timeout(Duration::from_secs(5));
        if url.scheme() == "https" {
            let mut tls = ClientTlsConfig::new().with_native_roots();
            if let Some(path) = ca_file {
                tls = tls.ca_certificate(Certificate::from_pem(
                    std::fs::read(path).map_err(ServiceError::storage)?,
                ));
            }
            if let Some(name) = server_name {
                tls = tls.domain_name(name);
            }
            builder = builder.tls_config(tls).map_err(ServiceError::storage)?;
        }
        let channel = builder.connect().await.map_err(|e| {
            ServiceError::new(ErrorCode::TransportDisconnected, "connect", error_chain(&e))
        })?;
        Ok(Self {
            transport: Arc::new(Transport::Grpc {
                channel,
                token: Some(token.into()),
                local: None,
            }),
        })
    }
    pub(crate) async fn embedded_host(&self) -> Option<Arc<super::host::Host>> {
        match self.transport.as_ref() {
            Transport::Embedded { host, .. } => host.lock().await.clone(),
            Transport::Grpc { .. } => None,
        }
    }
    pub async fn close(&self) -> Result<()> {
        if let Transport::Grpc {
            local: Some(connection),
            ..
        } = self.transport.as_ref()
        {
            connection.cancel.cancel();
        }
        if let Transport::Embedded { module, host } = self.transport.as_ref() {
            // Serialize the entire close, including browser shutdown. Closed
            // client clones must not retain host leases or restart services.
            let mut slot = host.lock().await;
            if let Some(host) = slot.as_ref() {
                host.shutdown();
            }
            let result = module.close().await.map_err(codec::from_module);
            if let Some(browser) = slot.as_ref().and_then(|host| host.remote.as_ref()) {
                // Only embedded modules are stopped; a remote daemon remains available.
                Box::pin(browser.close()).await?;
            }
            slot.take();
            result?;
        }
        Ok(())
    }
    pub async fn unary<
        Req: Message + Default + Send + Sync + 'static,
        Resp: Message + Default + Send + Sync + 'static,
    >(
        &self,
        path: &str,
        value: Req,
    ) -> Result<Resp> {
        if value.encoded_len() > MAX_MESSAGE {
            return Err(ServiceError::invalid(format!(
                "RPC message exceeds {MAX_MESSAGE_LABEL}"
            )));
        }
        let timeout = unary_timeout(path);
        match self.transport.as_ref() {
            Transport::Embedded { module: host, .. } => {
                let bytes = host
                    .unary(
                        path,
                        &value.encode_to_vec(),
                        synurang_host::ModuleCallOptions {
                            timeout: Some(timeout),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(codec::from_module)?;
                Resp::decode(bytes.as_slice()).map_err(ServiceError::storage)
            }
            Transport::Grpc {
                channel,
                token,
                local,
            } => {
                if path != "/anticrumb.v1.WorkspaceService/ConnectClient" {
                    prepare_local(channel, local.as_deref()).await?;
                }
                grpc_unary(channel, token.as_deref(), path, value).await
            }
        }
    }
    pub async fn watch_events(&self, cursor: pb::EventCursor) -> Result<EventStream> {
        self.watch_event_stream("/anticrumb.v1.TurnService/WatchEvents", cursor)
            .await
    }
    pub async fn watch_agent_events(&self, cursor: pb::EventCursor) -> Result<EventStream> {
        self.watch_event_stream("/anticrumb.v1.WorkspaceService/WatchAgentEvents", cursor)
            .await
    }
    async fn watch_event_stream(
        &self,
        path: &'static str,
        cursor: pb::EventCursor,
    ) -> Result<EventStream> {
        match self.transport.as_ref() {
            Transport::Embedded { module: host, .. } => {
                let call = host
                    .server_stream(path, &cursor.encode_to_vec(), Default::default())
                    .await
                    .map_err(codec::from_module)?;
                Ok(Box::pin(call.responses().map(|value| {
                    value.map_err(codec::from_module).and_then(|bytes| {
                        pb::Event::decode(bytes.as_slice()).map_err(ServiceError::storage)
                    })
                })))
            }
            Transport::Grpc {
                channel,
                token,
                local,
            } => {
                prepare_local(channel, local.as_deref()).await?;
                let mut client = tonic::client::Grpc::new(channel.clone())
                    .max_decoding_message_size(MAX_MESSAGE);
                client.ready().await.map_err(|e| {
                    ServiceError::new(ErrorCode::TransportDisconnected, "rpc", e.to_string())
                })?;
                let mut request = tonic::Request::new(cursor);
                attach_client_headers(&mut request, token.as_deref())?;
                // Limit establishing the stream without adding a gRPC deadline
                // that would cut off a long-running turn after it starts.
                let response = tokio::time::timeout(
                    DEFAULT_RPC_TIMEOUT,
                    client.server_streaming(
                        request,
                        tonic::codegen::http::uri::PathAndQuery::from_static(path),
                        tonic::codec::ProstCodec::default(),
                    ),
                )
                .await
                .map_err(|_| {
                    ServiceError::new(
                        ErrorCode::DeadlineExceeded,
                        "rpc",
                        "event stream did not start within 45 seconds",
                    )
                })?
                .map_err(codec::from_status)?;
                Ok(Box::pin(
                    response.into_inner().map(|v| v.map_err(codec::from_status)),
                ))
            }
        }
    }
    pub async fn put_artifact_stream<S>(&self, chunks: S) -> Result<pb::Artifact>
    where
        S: Stream<Item = pb::PutArtifactChunk> + Send + 'static,
    {
        let path = "/anticrumb.v1.ArtifactService/PutArtifact";
        match self.transport.as_ref() {
            Transport::Embedded { module: host, .. } => {
                let encoded = chunks.map(|chunk| chunk.encode_to_vec());
                let bytes = host
                    .client_stream(
                        path,
                        encoded,
                        synurang_host::ModuleCallOptions {
                            timeout: Some(ARTIFACT_TRANSFER_TIMEOUT),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(codec::from_module)?;
                pb::Artifact::decode(bytes.as_slice()).map_err(ServiceError::storage)
            }
            Transport::Grpc {
                channel,
                token,
                local,
            } => {
                prepare_local(channel, local.as_deref()).await?;
                let mut client = tonic::client::Grpc::new(channel.clone())
                    .max_decoding_message_size(MAX_MESSAGE)
                    .max_encoding_message_size(MAX_MESSAGE);
                client.ready().await.map_err(|e| {
                    ServiceError::new(ErrorCode::TransportDisconnected, "rpc", e.to_string())
                })?;
                let mut request = tonic::Request::new(chunks);
                request.set_timeout(ARTIFACT_TRANSFER_TIMEOUT);
                attach_client_headers(&mut request, token.as_deref())?;
                let response = client
                    .client_streaming(
                        request,
                        tonic::codegen::http::uri::PathAndQuery::from_static(path),
                        tonic::codec::ProstCodec::default(),
                    )
                    .await
                    .map_err(codec::from_status)?;
                Ok(response.into_inner())
            }
        }
    }
    pub async fn get_artifact_stream(&self, target: pb::ArtifactRef) -> Result<ArtifactStream> {
        let path = "/anticrumb.v1.ArtifactService/GetArtifact";
        match self.transport.as_ref() {
            Transport::Embedded { module: host, .. } => {
                let call = host
                    .server_stream(
                        path,
                        &target.encode_to_vec(),
                        synurang_host::ModuleCallOptions {
                            timeout: Some(ARTIFACT_TRANSFER_TIMEOUT),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(codec::from_module)?;
                Ok(Box::pin(call.responses().map(|value| {
                    value.map_err(codec::from_module).and_then(|bytes| {
                        pb::GetArtifactChunk::decode(bytes.as_slice())
                            .map_err(ServiceError::storage)
                    })
                })))
            }
            Transport::Grpc {
                channel,
                token,
                local,
            } => {
                prepare_local(channel, local.as_deref()).await?;
                let mut client = tonic::client::Grpc::new(channel.clone())
                    .max_decoding_message_size(MAX_MESSAGE)
                    .max_encoding_message_size(MAX_MESSAGE);
                client.ready().await.map_err(|e| {
                    ServiceError::new(ErrorCode::TransportDisconnected, "rpc", e.to_string())
                })?;
                let mut request = tonic::Request::new(target);
                request.set_timeout(ARTIFACT_TRANSFER_TIMEOUT);
                attach_client_headers(&mut request, token.as_deref())?;
                let response = tokio::time::timeout(
                    DEFAULT_RPC_TIMEOUT,
                    client.server_streaming(
                        request,
                        tonic::codegen::http::uri::PathAndQuery::from_static(path),
                        tonic::codec::ProstCodec::default(),
                    ),
                )
                .await
                .map_err(|_| {
                    ServiceError::new(
                        ErrorCode::DeadlineExceeded,
                        "rpc",
                        "artifact stream did not start within 45 seconds",
                    )
                })?
                .map_err(codec::from_status)?;
                Ok(Box::pin(
                    response.into_inner().map(|v| v.map_err(codec::from_status)),
                ))
            }
        }
    }
    pub async fn put_artifact_bytes(
        &self,
        account_id: &str,
        name: &str,
        mime: &str,
        data: Vec<u8>,
    ) -> Result<pb::Artifact> {
        if data.len() > MAX_ARTIFACT {
            return Err(ServiceError::invalid(format!(
                "artifact exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        let meta = pb::PutArtifactChunk {
            payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                account_id: account_id.to_owned(),
                name: name.to_owned(),
                mime: mime.to_owned(),
                sha256: digest(&data),
                size: data.len() as u64,
            })),
        };
        let (tx, rx) = mpsc::channel(4);
        let sender = tokio::spawn(async move {
            if tx.send(meta).await.is_err() {
                return;
            }
            for chunk in data.chunks(ARTIFACT_CHUNK_BYTES) {
                if tx
                    .send(pb::PutArtifactChunk {
                        payload: Some(pb::put_artifact_chunk::Payload::Data(chunk.to_vec())),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let result = self.put_artifact_stream(ReceiverStream::new(rx)).await;
        sender.abort();
        let _ = sender.await;
        result
    }
    pub async fn put_artifact_file(
        &self,
        account_id: &str,
        path: &Path,
        name: &str,
        mime: &str,
    ) -> Result<pb::Artifact> {
        let path_owned = path.to_path_buf();
        let (file, expected_size, expected_sha256) =
            tokio::task::spawn_blocking(move || -> Result<(fs::File, u64, String)> {
                let pre_meta = fs::metadata(&path_owned).map_err(ServiceError::storage)?;
                if !pre_meta.is_file() {
                    return Err(ServiceError::invalid("artifact source is not a file"));
                }
                if pre_meta.len() > MAX_ARTIFACT as u64 {
                    return Err(ServiceError::invalid(format!(
                        "artifact exceeds {MAX_ARTIFACT_LABEL}"
                    )));
                }
                let mut file = fs::File::open(&path_owned).map_err(ServiceError::storage)?;
                let meta = file.metadata().map_err(ServiceError::storage)?;
                if !meta.is_file() {
                    return Err(ServiceError::invalid("artifact source is not a file"));
                }
                if meta.len() > MAX_ARTIFACT as u64 {
                    return Err(ServiceError::invalid(format!(
                        "artifact exceeds {MAX_ARTIFACT_LABEL}"
                    )));
                }
                let mut hasher = Sha256::new();
                let mut total = 0u64;
                let mut buf = vec![0u8; ARTIFACT_CHUNK_BYTES];
                loop {
                    let n = match file.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(ServiceError::storage(e)),
                    };
                    let Some(next) = total.checked_add(n as u64) else {
                        return Err(ServiceError::invalid(format!(
                            "artifact exceeds {MAX_ARTIFACT_LABEL}"
                        )));
                    };
                    if next > MAX_ARTIFACT as u64 {
                        return Err(ServiceError::invalid(format!(
                            "artifact exceeds {MAX_ARTIFACT_LABEL}"
                        )));
                    }
                    hasher.update(&buf[..n]);
                    total = next;
                }
                file.seek(SeekFrom::Start(0))
                    .map_err(ServiceError::storage)?;
                let mut sha = String::with_capacity(64);
                for byte in hasher.finalize() {
                    let _ = write!(&mut sha, "{byte:02x}");
                }
                Ok((file, total, sha))
            })
            .await
            .map_err(ServiceError::storage)??;
        self.put_artifact_verified_file(
            account_id,
            name,
            mime,
            &expected_sha256,
            expected_size,
            file,
        )
        .await
    }
    pub async fn put_artifact_verified_file(
        &self,
        account_id: &str,
        name: &str,
        mime: &str,
        expected_sha256: &str,
        expected_size: u64,
        mut file: fs::File,
    ) -> Result<pb::Artifact> {
        if expected_size > MAX_ARTIFACT as u64 {
            return Err(ServiceError::invalid(format!(
                "artifact exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        if !is_valid_sha256_hex(expected_sha256) {
            return Err(ServiceError::invalid("invalid artifact SHA-256"));
        }
        let meta = pb::PutArtifactChunk {
            payload: Some(pb::put_artifact_chunk::Payload::Meta(pb::PutArtifactMeta {
                account_id: account_id.to_owned(),
                name: name.to_owned(),
                mime: mime.to_owned(),
                sha256: expected_sha256.to_owned(),
                size: expected_size,
            })),
        };
        let expected_sha256_owned = expected_sha256.to_owned();
        let read_error = Arc::new(std::sync::Mutex::new(None::<ServiceError>));
        let read_error_worker = read_error.clone();
        let (tx, rx) = mpsc::channel(4);
        let reader = tokio::task::spawn_blocking(move || {
            fn read_trailing(file: &mut fs::File) -> std::io::Result<usize> {
                let mut extra = [0u8; 1];
                loop {
                    match file.read(&mut extra) {
                        Ok(m) => return Ok(m),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            if expected_size == 0 {
                match read_trailing(&mut file) {
                    Ok(0) => {
                        if digest(&[]) != expected_sha256_owned {
                            *read_error_worker.lock().unwrap() = Some(ServiceError::new(
                                ErrorCode::StorageFailure,
                                "blob",
                                "artifact integrity check failed",
                            ));
                            return;
                        }
                        let _ = tx.blocking_send(meta);
                    }
                    Ok(_) => {
                        *read_error_worker.lock().unwrap() = Some(ServiceError::new(
                            ErrorCode::StorageFailure,
                            "blob",
                            "artifact integrity check failed",
                        ));
                    }
                    Err(e) => {
                        *read_error_worker.lock().unwrap() = Some(ServiceError::storage(e));
                    }
                }
                return;
            }
            if tx.blocking_send(meta).is_err() {
                return;
            }
            let mut hasher = Sha256::new();
            let mut total = 0u64;
            let mut buf = vec![0u8; ARTIFACT_CHUNK_BYTES];
            loop {
                if tx.is_closed() {
                    return;
                }
                let remaining = expected_size - total;
                let limit = remaining.min(buf.len() as u64) as usize;
                let n = match file.read(&mut buf[..limit]) {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        *read_error_worker.lock().unwrap() = Some(ServiceError::storage(e));
                        return;
                    }
                };
                if n == 0 {
                    *read_error_worker.lock().unwrap() = Some(ServiceError::new(
                        ErrorCode::StorageFailure,
                        "blob",
                        "artifact integrity check failed",
                    ));
                    return;
                }
                hasher.update(&buf[..n]);
                total += n as u64;
                if total == expected_size {
                    // Verify no trailing bytes and full digest before sending the final chunk so a
                    // mid-stream append/mutation never commits on the server.
                    match read_trailing(&mut file) {
                        Ok(0) => {
                            let mut actual_hash = String::with_capacity(64);
                            for byte in hasher.finalize() {
                                let _ = write!(&mut actual_hash, "{byte:02x}");
                            }
                            if actual_hash != expected_sha256_owned {
                                *read_error_worker.lock().unwrap() = Some(ServiceError::new(
                                    ErrorCode::StorageFailure,
                                    "blob",
                                    "artifact integrity check failed",
                                ));
                                return;
                            }
                            let _ = tx.blocking_send(pb::PutArtifactChunk {
                                payload: Some(pb::put_artifact_chunk::Payload::Data(
                                    buf[..n].to_vec(),
                                )),
                            });
                            return;
                        }
                        Ok(_) => {
                            *read_error_worker.lock().unwrap() = Some(ServiceError::new(
                                ErrorCode::StorageFailure,
                                "blob",
                                "artifact integrity check failed",
                            ));
                            return;
                        }
                        Err(e) => {
                            *read_error_worker.lock().unwrap() = Some(ServiceError::storage(e));
                            return;
                        }
                    }
                }
                if tx
                    .blocking_send(pb::PutArtifactChunk {
                        payload: Some(pb::put_artifact_chunk::Payload::Data(buf[..n].to_vec())),
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let result = self.put_artifact_stream(ReceiverStream::new(rx)).await;
        let _ = reader.await;
        if let Some(error) = read_error.lock().unwrap().take() {
            return Err(error);
        }
        result
    }
    pub async fn get_artifact_bytes(&self, target: pb::ArtifactRef) -> Result<ArtifactBytes> {
        let mut stream = self.get_artifact_stream(target.clone()).await?;
        let meta = next_artifact_meta(&mut stream, &target).await?;
        let cap = usize::try_from(meta.size)
            .map_err(|_| ServiceError::invalid("artifact exceeds addressable size"))?;
        let mut data = Vec::with_capacity(cap.min(ARTIFACT_CHUNK_BYTES));
        let mut hasher = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            match chunk.payload {
                Some(pb::get_artifact_chunk::Payload::Artifact(_)) => {
                    return Err(ServiceError::invalid("duplicate artifact metadata chunk"));
                }
                None => {
                    return Err(ServiceError::invalid("empty artifact download chunk"));
                }
                Some(pb::get_artifact_chunk::Payload::Data(bytes)) => {
                    if bytes.is_empty() {
                        return Err(ServiceError::invalid("empty artifact download data chunk"));
                    }
                    let Some(next) = (data.len() as u64).checked_add(bytes.len() as u64) else {
                        return Err(ServiceError::invalid(
                            "downloaded artifact integrity mismatch",
                        ));
                    };
                    if next > meta.size {
                        return Err(ServiceError::invalid(
                            "downloaded artifact integrity mismatch",
                        ));
                    }
                    hasher.update(&bytes);
                    data.extend_from_slice(&bytes);
                }
            }
        }
        let mut actual_hash = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(&mut actual_hash, "{byte:02x}");
        }
        if data.len() as u64 != meta.size || actual_hash != meta.hash {
            return Err(ServiceError::invalid(
                "downloaded artifact integrity mismatch",
            ));
        }
        Ok(ArtifactBytes {
            artifact: meta,
            data,
        })
    }
    pub async fn download_artifact_to_store(
        &self,
        store: &crate::store::Store,
        account_id: &str,
        id: &str,
        origin: &str,
    ) -> Result<pb::Artifact> {
        let target = pb::ArtifactRef {
            account_id: account_id.to_owned(),
            id: id.to_owned(),
        };
        let mut stream = self.get_artifact_stream(target.clone()).await?;
        let meta = next_artifact_meta(&mut stream, &target).await?;
        let account = account_id.to_owned();
        let origin_owned = origin.to_owned();
        let meta_for_writer = meta.clone();
        let writer = store
            .blocking(move |s| {
                s.artifact_writer(
                    &account,
                    &meta_for_writer.name,
                    &meta_for_writer.mime,
                    Some(meta_for_writer.size),
                    Some(&meta_for_writer.hash),
                    &origin_owned,
                )
            })
            .await?;

        let saved = stream_chunks_to_sink(
            stream,
            writer,
            ChunkStreamSpec {
                expected_size: meta.size,
                max_chunk_bytes: None,
                duplicate_meta_msg: "duplicate artifact metadata chunk",
                empty_chunk_msg: "empty artifact download chunk",
                empty_data_msg: "empty artifact download data chunk",
                chunk_limit_msg: std::borrow::Cow::Borrowed(""),
                overflow_msg: "downloaded artifact integrity mismatch",
                size_mismatch_msg: "downloaded artifact integrity mismatch",
                aborted_msg: "artifact download aborted",
            },
            |chunk| match chunk.payload {
                Some(pb::get_artifact_chunk::Payload::Artifact(_)) => ChunkPayload::Meta,
                Some(pb::get_artifact_chunk::Payload::Data(bytes)) => ChunkPayload::Data(bytes),
                None => ChunkPayload::Empty,
            },
        )
        .await?;
        Ok(codec::artifact_to(saved))
    }
    pub async fn get_artifact_to_file(
        &self,
        target: pb::ArtifactRef,
        dest: &Path,
    ) -> Result<pb::Artifact> {
        if dest.file_name().is_none()
            || dest
                .as_os_str()
                .to_string_lossy()
                .chars()
                .last()
                .is_some_and(std::path::is_separator)
        {
            return Err(ServiceError::invalid("invalid destination file path"));
        }
        struct TempFileGuard {
            path: PathBuf,
            remove_on_drop: bool,
        }
        impl Drop for TempFileGuard {
            fn drop(&mut self) {
                if self.remove_on_drop {
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
        struct AtomicFileSink {
            dest: PathBuf,
            tmp_path: PathBuf,
            expected_size: u64,
            expected_sha256: String,
            total: u64,
            hasher: Sha256,
            // Declared before `guard` so `writer` closes before `guard` removes `tmp_path` on drop.
            writer: Option<BufWriter<fs::File>>,
            guard: TempFileGuard,
        }
        impl ArtifactChunkSink for AtomicFileSink {
            type Output = ();
            fn write_chunk(&mut self, bytes: &[u8]) -> Result<()> {
                let Some(next) = self.total.checked_add(bytes.len() as u64) else {
                    return Err(ServiceError::invalid(
                        "downloaded artifact integrity mismatch",
                    ));
                };
                let w = self
                    .writer
                    .as_mut()
                    .ok_or_else(|| ServiceError::storage("artifact writer missing"))?;
                w.write_all(bytes).map_err(ServiceError::storage)?;
                self.hasher.update(bytes);
                self.total = next;
                Ok(())
            }
            fn finish(mut self) -> Result<Self::Output> {
                let mut w = self
                    .writer
                    .take()
                    .ok_or_else(|| ServiceError::storage("artifact writer missing"))?;
                w.flush().map_err(ServiceError::storage)?;
                drop(w);
                let mut actual_hash = String::with_capacity(64);
                for byte in self.hasher.clone().finalize() {
                    let _ = write!(&mut actual_hash, "{byte:02x}");
                }
                if self.total != self.expected_size || actual_hash != self.expected_sha256 {
                    return Err(ServiceError::invalid(
                        "downloaded artifact integrity mismatch",
                    ));
                }
                match fs::hard_link(&self.tmp_path, &self.dest) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Err(ServiceError::storage(e))
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                        ) =>
                    {
                        match fs::symlink_metadata(&self.dest) {
                            Ok(_) => Err(ServiceError::storage(std::io::Error::new(
                                std::io::ErrorKind::AlreadyExists,
                                format!("destination already exists: {}", self.dest.display()),
                            ))),
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                                fs::rename(&self.tmp_path, &self.dest)
                                    .map_err(ServiceError::storage)?;
                                self.guard.remove_on_drop = false;
                                Ok(())
                            }
                            Err(err) => Err(ServiceError::storage(err)),
                        }
                    }
                    Err(e) => Err(ServiceError::storage(e)),
                }
            }
        }

        let dest_owned = dest.to_path_buf();
        let mut sink = tokio::task::spawn_blocking(move || -> Result<AtomicFileSink> {
            match fs::symlink_metadata(&dest_owned) {
                Ok(_) => {
                    return Err(ServiceError::storage(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("destination already exists: {}", dest_owned.display()),
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ServiceError::storage(e)),
            }
            let parent = match dest_owned.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            let tmp_path = parent.join(format!(".anticrumb-tmp-{}", uuid::Uuid::new_v4().simple()));
            let guard = TempFileGuard {
                path: tmp_path.clone(),
                remove_on_drop: true,
            };
            let file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp_path)
                .map_err(ServiceError::storage)?;
            Ok(AtomicFileSink {
                dest: dest_owned,
                tmp_path,
                expected_size: 0,
                expected_sha256: String::new(),
                total: 0,
                hasher: Sha256::new(),
                writer: Some(BufWriter::with_capacity(64 * 1024, file)),
                guard,
            })
        })
        .await
        .map_err(ServiceError::storage)??;

        let mut stream = self.get_artifact_stream(target.clone()).await?;
        let meta = next_artifact_meta(&mut stream, &target).await?;
        sink.expected_size = meta.size;
        sink.expected_sha256 = meta.hash.clone();
        stream_chunks_to_sink(
            stream,
            sink,
            ChunkStreamSpec {
                expected_size: meta.size,
                max_chunk_bytes: None,
                duplicate_meta_msg: "duplicate artifact metadata chunk",
                empty_chunk_msg: "empty artifact download chunk",
                empty_data_msg: "empty artifact download data chunk",
                chunk_limit_msg: std::borrow::Cow::Borrowed(""),
                overflow_msg: "downloaded artifact integrity mismatch",
                size_mismatch_msg: "downloaded artifact integrity mismatch",
                aborted_msg: "artifact download aborted",
            },
            |chunk| match chunk.payload {
                Some(pb::get_artifact_chunk::Payload::Artifact(_)) => ChunkPayload::Meta,
                Some(pb::get_artifact_chunk::Payload::Data(bytes)) => ChunkPayload::Data(bytes),
                None => ChunkPayload::Empty,
            },
        )
        .await?;
        Ok(meta)
    }
}

async fn next_artifact_meta(
    stream: &mut ArtifactStream,
    target: &pb::ArtifactRef,
) -> Result<pb::Artifact> {
    let first = stream
        .next()
        .await
        .ok_or_else(|| ServiceError::invalid("missing artifact metadata chunk"))??;
    let Some(pb::get_artifact_chunk::Payload::Artifact(meta)) = first.payload else {
        return Err(ServiceError::invalid(
            "first download chunk must contain artifact metadata",
        ));
    };
    if meta.id != target.id || meta.scope != target.account_id {
        return Err(ServiceError::invalid(
            "downloaded artifact identity mismatch",
        ));
    }
    if !is_valid_artifact_name(&meta.name) {
        return Err(ServiceError::invalid("invalid artifact name"));
    }
    if meta.size > MAX_ARTIFACT as u64 {
        return Err(ServiceError::invalid(format!(
            "artifact exceeds {MAX_ARTIFACT_LABEL}"
        )));
    }
    if !is_valid_sha256_hex(&meta.hash) {
        return Err(ServiceError::invalid("invalid artifact SHA-256"));
    }
    Ok(meta)
}

include!(concat!(env!("OUT_DIR"), "/client_bindings.rs"));

async fn prepare_local(
    channel: &Channel,
    local: Option<&super::local::ClientConnection>,
) -> Result<()> {
    if let Some(local) = local {
        if local.cancel.is_cancelled() {
            return Err(ServiceError::invalid("local client is closed"));
        }
        // Idempotent connection binding also covers HTTP/2 reconnects, without
        // inventing a preamble or replaying any application operation.
        let configuration = local
            .configuration
            .lock()
            .map_err(ServiceError::storage)?
            .clone();
        let _: pb::HostInfo = grpc_unary(
            channel,
            None,
            "/anticrumb.v1.WorkspaceService/ConnectClient",
            configuration,
        )
        .await?;
    }
    Ok(())
}
async fn grpc_unary<Req, Resp>(
    channel: &Channel,
    token: Option<&str>,
    path: &str,
    value: Req,
) -> Result<Resp>
where
    Req: Message + Default + Send + Sync + 'static,
    Resp: Message + Default + Send + Sync + 'static,
{
    let timeout = unary_timeout(path);
    let mut client = tonic::client::Grpc::new(channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    client
        .ready()
        .await
        .map_err(|e| ServiceError::new(ErrorCode::TransportDisconnected, "rpc", e.to_string()))?;
    let mut request = tonic::Request::new(value);
    request.set_timeout(timeout);
    attach_client_headers(&mut request, token)?;
    let path = path
        .parse::<tonic::codegen::http::uri::PathAndQuery>()
        .map_err(ServiceError::storage)?;
    client
        .unary(request, path, tonic::codec::ProstCodec::default())
        .await
        .map(|r| r.into_inner())
        .map_err(codec::from_status)
}

fn attach_client_headers<T>(request: &mut tonic::Request<T>, token: Option<&str>) -> Result<()> {
    if let Some(token) = token {
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(ServiceError::storage)?,
        );
    }
    type AsciiHeader = Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>;
    static CLIENT_HEADERS: std::sync::OnceLock<(AsciiHeader, AsciiHeader)> =
        std::sync::OnceLock::new();
    let (version, client_id) = CLIENT_HEADERS.get_or_init(|| {
        let v = crate::build_info::client_version().parse().ok();
        let c = crate::cli::client_identity().parse().ok();
        (v, c)
    });
    if let Some(v) = version.clone() {
        request.metadata_mut().insert("x-anticrumb-version", v);
    }
    if let Some(c) = client_id.clone() {
        request.metadata_mut().insert("x-anticrumb-client", c);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TimeoutEcho;
    #[tonic::async_trait]
    impl pb::model_service_server::ModelService for TimeoutEcho {
        async fn get_catalog(
            &self,
            request: tonic::Request<pb::AccountRef>,
        ) -> std::result::Result<tonic::Response<pb::ModelCatalog>, tonic::Status> {
            Ok(tonic::Response::new(pb::ModelCatalog {
                server_id: request
                    .metadata()
                    .get("grpc-timeout")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
                ..Default::default()
            }))
        }

        async fn refresh_catalog(
            &self,
            request: tonic::Request<pb::AccountRef>,
        ) -> std::result::Result<tonic::Response<pb::ModelCatalog>, tonic::Status> {
            self.get_catalog(request).await
        }
    }

    #[tokio::test]
    async fn model_refresh_sends_a_longer_deadline_without_extending_cached_reads() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(pb::model_service_server::ModelServiceServer::new(
                    TimeoutEcho,
                ))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = stopped.await;
                    },
                )
                .await
                .unwrap();
        });
        let client = RpcClient::network(&endpoint, "fixture-token", None, None)
            .await
            .unwrap();
        let cached = client
            .get_catalog(pb::AccountRef {
                account_id: "fixture".into(),
            })
            .await
            .unwrap();
        let refreshed = client
            .refresh_catalog(pb::AccountRef {
                account_id: "fixture".into(),
            })
            .await
            .unwrap();
        // These are real request headers observed by the service, not a mock
        // timeout policy. tonic represents 45s in microseconds and 150s in ms.
        assert_eq!(cached.server_id, "45000000u");
        assert_eq!(refreshed.server_id, "150000m");
        drop(client);
        stop.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn remote_connection_rejects_http_before_connecting_or_sending_credentials() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let connection = Connection::Grpc {
            endpoint: format!("http://{address}/private-path?token=private-query"),
            token: "private-token".into(),
            ca_file: None,
            server_name: None,
            tls_pin: None,
        };
        let error = RpcClient::connect(&connection).await.err().unwrap();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("require TLS"));
        assert!(error.message.contains("https://"));
        assert!(!error.message.contains("private-"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn invalid_endpoint_identifies_transport_and_redacts_secrets() {
        let connection = Connection::Grpc {
            endpoint: "https://private-user:private-password@example.invalid:8443/private-path?token=private-query#private-fragment".into(),
            token: "private-token".into(),
            ca_file: None,
            server_name: None,
            tls_pin: None,
        };
        assert!(!format!("{connection:?}").contains("private-"));
        let error = RpcClient::connect(&connection).await.err().unwrap();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.stage, "validate");
        assert!(error.message.contains("gRPC transport"));
        assert!(error.message.contains("https://example.invalid:8443"));
        assert!(!error.message.contains("private-"));
    }

    #[tokio::test]
    async fn refused_endpoint_includes_the_underlying_connection_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let origin = format!("http://{address}");
        let endpoint = format!("{origin}/private-path?token=private-query");
        let error = RpcClient::network(&endpoint, "private-token", None, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::TransportDisconnected);
        assert_eq!(error.stage, "connect");
        assert!(error.message.contains(&origin));
        assert!(error.message.contains("gRPC transport"));
        assert!(error.message.contains("transport error:"), "{error}");
        assert!(error.message.to_lowercase().contains("refused"), "{error}");
        assert!(!error.message.contains("private-"));
    }
}
