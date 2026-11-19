// tonic's public service contract requires Status as the error type.
#![allow(clippy::result_large_err)]
use super::{codec, host::Host, pb, MAX_MESSAGE};
use crate::domain::{Result, ServiceError};
use futures_util::{Stream, StreamExt};
use prost::Message;
use std::{pin::Pin, sync::Arc};
use tonic::{metadata::MetadataMap, transport::Server};

pub type TypedStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, tonic::Status>> + Send>>;

#[derive(Clone)]
pub struct ConnectionContext {
    pub peer: Option<std::net::SocketAddr>,
    pub started: std::time::Instant,
    pub logged_auth_success: Arc<std::sync::atomic::AtomicBool>,
    pub auth_failures: Arc<std::sync::atomic::AtomicUsize>,
}

pub(crate) fn sanitize_header_value(val: &str, max_len: usize) -> String {
    let mut s = String::with_capacity(max_len + 3);
    let mut truncated = false;
    for (count, c) in val.chars().enumerate() {
        if count >= max_len {
            truncated = true;
            break;
        }
        let safe = match c {
            '[' | ']' => '_',
            c if c.is_ascii() && !c.is_ascii_control() => c,
            _ => '_',
        };
        s.push(safe);
    }
    if truncated {
        s.push_str("...");
    }
    s
}

pub fn log_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_of_day = secs % 86400;
    let hours = secs_of_day / 3600;
    let minutes = (secs_of_day % 3600) / 60;
    let seconds = secs_of_day % 60;

    let z = (secs / 86400) as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("[{y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}:{seconds:02} UTC]")
}

pub fn format_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

#[derive(Clone)]
pub struct GrpcApi {
    host: Option<Arc<Host>>,
    pub token: Option<String>,
    local: Option<super::local::LocalInfo>,
    context: Option<ConnectionContext>,
}
impl GrpcApi {
    pub fn new(host: Arc<Host>, token: Option<String>) -> Self {
        Self {
            host: Some(host),
            token,
            local: None,
            context: None,
        }
    }
    fn local() -> Self {
        Self {
            host: None,
            token: None,
            local: None,
            context: None,
        }
    }
    fn for_request(&self, extensions: &http::Extensions) -> Self {
        let mut api = self.clone();
        if api.token.is_none() {
            api.local = extensions.get::<super::local::LocalInfo>().cloned();
        }
        api.context = extensions.get::<ConnectionContext>().cloned();
        api
    }
    async fn selected_host(&self) -> std::result::Result<Arc<Host>, tonic::Status> {
        match (&self.local, &self.host) {
            (Some(local), _) => local.host().await.map_err(codec::status),
            (None, Some(host)) => Ok(host.clone()),
            (None, None) => Err(codec::status(ServiceError::invalid(
                "call WorkspaceService.ConnectClient before workspace operations",
            ))),
        }
    }
    pub fn authorize(&self, metadata: &MetadataMap) -> std::result::Result<(), tonic::Status> {
        let version = metadata
            .get("x-anticrumb-version")
            .and_then(|v| v.to_str().ok())
            .map(|s| sanitize_header_value(s, 128));
        let client_id = metadata
            .get("x-anticrumb-client")
            .and_then(|v| v.to_str().ok())
            .map(|s| sanitize_header_value(s, 128));

        let client_desc = match (version.as_deref(), client_id.as_deref()) {
            (Some(v), Some(c)) => format!("[Client: {v}, {c}]"),
            (Some(v), None) => format!("[Client: {v}]"),
            (None, Some(c)) => format!("[Client: {c}]"),
            (None, None) => "[Client]".into(),
        };

        let Some(expected) = &self.token else {
            if let Some(local) = &self.local {
                if !local.is_automatic()
                    && !local
                        .logged_first
                        .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    eprintln!(
                        "{} client identified: local {}",
                        log_timestamp(),
                        client_desc
                    );
                }
            }
            return Ok(());
        };
        let provided = metadata
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let is_valid = !expected.is_empty() && super::constant_time_eq(provided, expected);

        if let Some(ctx) = &self.context {
            let peer_str = ctx
                .peer
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into());
            if is_valid {
                if !ctx
                    .logged_auth_success
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    eprintln!(
                        "{} client authenticated: {} {} (auth: verified)",
                        log_timestamp(),
                        peer_str,
                        client_desc
                    );
                }
            } else {
                let fails = ctx
                    .auth_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if fails <= 5 {
                    eprintln!(
                        "{} client auth failed: {} {} (auth: invalid token)",
                        log_timestamp(),
                        peer_str,
                        client_desc
                    );
                } else if fails == 6 {
                    eprintln!(
                        "{} client auth failed: {} {} (further failed attempts on this connection suppressed)",
                        log_timestamp(),
                        peer_str,
                        client_desc
                    );
                }
            }
        }

        if !is_valid {
            Err(tonic::Status::unauthenticated("invalid daemon token"))
        } else {
            Ok(())
        }
    }
    pub async fn call<Req: Message, Resp: Message + Default>(
        &self,
        path: &str,
        request: Req,
    ) -> std::result::Result<Resp, tonic::Status> {
        let mut bytes = if path == "/anticrumb.v1.WorkspaceService/ConnectClient" {
            let local = self.local.as_ref().ok_or_else(|| {
                tonic::Status::failed_precondition(
                    "ConnectClient requires a local socket or named pipe",
                )
            })?;
            let request = pb::ConnectClientRequest::decode(request.encode_to_vec().as_slice())
                .map_err(|_| tonic::Status::invalid_argument("invalid client binding"))?;
            local
                .bind(request)
                .await
                .map_err(codec::status)?
                .encode_to_vec()
        } else if path == "/anticrumb.v1.PlatformService/Shutdown" {
            if let Some(local) = &self.local {
                local
                    .shutdown(request.encode_to_vec())
                    .await
                    .map_err(codec::status)?
            } else {
                self.selected_host()
                    .await?
                    .dispatch(path, request.encode_to_vec())
                    .await
                    .map_err(codec::status)?
            }
        } else {
            self.selected_host()
                .await?
                .dispatch(path, request.encode_to_vec())
                .await
                .map_err(codec::status)?
        };
        if path == "/anticrumb.v1.PlatformService/GetStatus" {
            if let Some(local) = &self.local {
                let mut status = pb::HostStatus::decode(bytes.as_slice())
                    .map_err(|_| tonic::Status::internal("invalid host status"))?;
                status.runtime = Some(local.status().map_err(codec::status)?);
                bytes = status.encode_to_vec();
            }
        }
        Resp::decode(bytes.as_slice()).map_err(|_| tonic::Status::internal("invalid host response"))
    }
    async fn stream<Req: ServerStreamReq>(
        &self,
        _path: &str,
        request: Req,
    ) -> std::result::Result<tonic::Response<TypedStream<Req::Item>>, tonic::Status> {
        let host = self.selected_host().await?;
        let rx = request.open_stream(host).await.map_err(codec::status)?;
        Ok(tonic::Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx).map(|v| v.map_err(codec::status)),
        )))
    }
    async fn client_stream<Req: ClientStreamReq>(
        &self,
        _path: &str,
        stream: tonic::Streaming<Req>,
    ) -> std::result::Result<Req::Resp, tonic::Status> {
        let host = self.selected_host().await?;
        Req::handle_client_stream(host, stream)
            .await
            .map_err(codec::status)
    }
}

#[tonic::async_trait]
trait ServerStreamReq: Sized {
    type Item: Send + 'static;
    async fn open_stream(
        self,
        host: Arc<Host>,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<Self::Item>>>;
}

#[tonic::async_trait]
impl ServerStreamReq for pb::EventCursor {
    type Item = pb::Event;
    async fn open_stream(
        self,
        host: Arc<Host>,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<Self::Item>>> {
        host.watch(self).await
    }
}

#[tonic::async_trait]
impl ServerStreamReq for pb::ArtifactRef {
    type Item = pb::GetArtifactChunk;
    async fn open_stream(
        self,
        host: Arc<Host>,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<Self::Item>>> {
        host.get_artifact_stream(self).await
    }
}

#[tonic::async_trait]
trait ClientStreamReq: Sized + Send + 'static {
    type Resp: Send + 'static;
    async fn handle_client_stream(
        host: Arc<Host>,
        stream: tonic::Streaming<Self>,
    ) -> Result<Self::Resp>;
}

#[tonic::async_trait]
impl ClientStreamReq for pb::PutArtifactChunk {
    type Resp = pb::Artifact;
    async fn handle_client_stream(
        host: Arc<Host>,
        stream: tonic::Streaming<Self>,
    ) -> Result<Self::Resp> {
        host.put_artifact_stream(stream.map(|item| item.map_err(codec::from_status)))
            .await
    }
}
include!(concat!(env!("OUT_DIR"), "/grpc_bindings.rs"));

#[derive(Debug)]
struct TlsIdentityState {
    certificate: Vec<u8>,
    key: Vec<u8>,
    certified_key: Option<Arc<rustls::sign::CertifiedKey>>,
}

#[derive(Clone, Debug)]
pub struct TlsIdentity {
    state: Arc<std::sync::RwLock<TlsIdentityState>>,
}
impl TlsIdentity {
    pub fn from_pem(certificate: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            state: Arc::new(std::sync::RwLock::new(TlsIdentityState {
                certificate,
                key,
                certified_key: None,
            })),
        }
    }
    fn parse_certified_key(
        certificate: &[u8],
        key: &[u8],
        provider: &rustls::crypto::CryptoProvider,
    ) -> Result<Arc<rustls::sign::CertifiedKey>> {
        let certificates = rustls_pemfile::certs(&mut &certificate[..])
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(ServiceError::storage)?;
        if certificates.is_empty() {
            return Err(ServiceError::invalid("TLS certificate is missing"));
        }
        let key = rustls_pemfile::private_key(&mut &key[..])
            .map_err(ServiceError::storage)?
            .ok_or_else(|| ServiceError::invalid("TLS private key is missing"))?;
        rustls::sign::CertifiedKey::from_der(certificates, key, provider)
            .map(Arc::new)
            .map_err(ServiceError::storage)
    }
    pub fn replace_pem(&self, certificate: Vec<u8>, key: Vec<u8>) -> Result<()> {
        let provider = rustls::crypto::ring::default_provider();
        let certified_key = Self::parse_certified_key(&certificate, &key, &provider)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| ServiceError::invalid("TLS identity lock poisoned"))?;
        state.certificate = certificate;
        state.key = key;
        state.certified_key = Some(certified_key);
        Ok(())
    }
    pub(crate) fn acceptor(&self) -> Result<tokio_rustls::TlsAcceptor> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| ServiceError::invalid("TLS identity lock poisoned"))?;
            let certified_key =
                Self::parse_certified_key(&state.certificate, &state.key, &provider)?;
            state.certified_key = Some(certified_key);
        }
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(ServiceError::storage)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(self.clone()));
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }
}
impl rustls::server::ResolvesServerCert for TlsIdentity {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.certified_key.clone())
    }
}

pub trait PeerAddress: Send {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        None
    }
}

impl PeerAddress for tokio::net::TcpStream {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.peer_addr().ok()
    }
}

impl PeerAddress for tokio::io::DuplexStream {}

#[cfg(unix)]
impl PeerAddress for tokio::net::UnixStream {}

pub async fn serve<I, IO>(
    host: Arc<Host>,
    token: Option<String>,
    incoming: I,
    tls: Option<TlsIdentity>,
) -> Result<()>
where
    I: Stream<Item = std::io::Result<IO>> + Send + 'static,
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + PeerAddress + Unpin + Send + 'static,
{
    let active_clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    if let Some(identity) = tls {
        let acceptor = identity.acceptor()?;
        let active = active_clients.clone();
        let secure = incoming
            .map(move |socket| {
                let acceptor = acceptor.clone();
                let active = active.clone();
                async move {
                    let Ok(socket) = socket else {
                        return Ok(None);
                    };
                    let peer = socket.peer_addr();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        acceptor.accept(socket),
                    )
                    .await
                    {
                        Ok(Ok(socket)) => {
                            let count =
                                active.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            let peer_str = peer
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "unknown".into());
                            eprintln!(
                                "{} client connected: {} [active: {}]",
                                log_timestamp(),
                                peer_str,
                                count
                            );
                            let context = ConnectionContext {
                                peer,
                                started: std::time::Instant::now(),
                                logged_auth_success: Arc::new(std::sync::atomic::AtomicBool::new(
                                    false,
                                )),
                                auth_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                            };
                            Ok(Some(ConnectionIo {
                                inner: socket,
                                context,
                                active_clients: active,
                            }))
                        }
                        Ok(Err(_)) | Err(_) => Ok(None),
                    }
                }
            })
            .buffer_unordered(64)
            .filter_map(|res: std::io::Result<Option<ConnectionIo<_>>>| async move {
                match res {
                    Ok(Some(io)) => Some(Ok(io)),
                    _ => None,
                }
            });
        let shutdown = shutdown(host.clone());
        serve_routes(GrpcApi::new(host, token), secure, shutdown).await
    } else {
        let shutdown = shutdown(host.clone());
        let active = active_clients.clone();
        let logged = incoming.filter_map(move |res| {
            let active = active.clone();
            async move {
                let Ok(socket) = res else {
                    return None;
                };
                let peer = socket.peer_addr();
                let count = active.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let peer_str = peer
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".into());
                eprintln!(
                    "{} client connected: {} [active: {}]",
                    log_timestamp(),
                    peer_str,
                    count
                );
                let context = ConnectionContext {
                    peer,
                    started: std::time::Instant::now(),
                    logged_auth_success: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    auth_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                };
                Some(Ok(ConnectionIo {
                    inner: socket,
                    context,
                    active_clients: active,
                }))
            }
        });
        serve_routes(GrpcApi::new(host, token), logged, shutdown).await
    }
}

async fn shutdown(host: Arc<Host>) {
    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=host.service.shutdown.notified()=>{}}
    host.shutdown();
}

pub(crate) async fn serve_local<I, IO>(
    incoming: I,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()>
where
    I: Stream<Item = std::io::Result<IO>> + Send + 'static,
    IO: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + tonic::transport::server::Connected
        + Unpin
        + Send
        + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
{
    serve_routes(GrpcApi::local(), incoming, shutdown).await
}

async fn serve_routes<I, IO>(
    api: GrpcApi,
    incoming: I,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()>
where
    I: Stream<Item = std::io::Result<IO>> + Send + 'static,
    IO: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + tonic::transport::server::Connected
        + Unpin
        + Send
        + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
{
    let mut builder = Server::builder()
        .accept_http1(true)
        .concurrency_limit_per_connection(64);
    macro_rules! service {
        ($module:ident,$name:ident) => {
            pb::$module::$name::new(api.clone())
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE)
        };
    }
    let mut routes =
        tonic::service::Routes::new(service!(platform_service_server, PlatformServiceServer))
            .add_service(service!(provider_service_server, ProviderServiceServer))
            .add_service(service!(account_service_server, AccountServiceServer))
            .add_service(service!(model_service_server, ModelServiceServer))
            .add_service(service!(turn_service_server, TurnServiceServer))
            .add_service(service!(session_service_server, SessionServiceServer))
            .add_service(service!(artifact_service_server, ArtifactServiceServer));
    if api
        .host
        .as_ref()
        .is_none_or(|host| host.role == "workspace")
    {
        routes = routes.add_service(service!(workspace_service_server, WorkspaceServiceServer));
    }
    if let (Some(token), Some(host)) = (api.token.as_ref(), api.host.as_ref()) {
        let provider = crate::http::router(host.service.clone(), token.clone())?;
        routes = routes.into_axum_router().fallback_service(provider).into();
    }
    builder
        .add_routes(routes)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(ServiceError::storage)
}

struct ConnectionIo<T> {
    inner: T,
    context: ConnectionContext,
    active_clients: Arc<std::sync::atomic::AtomicUsize>,
}
impl<T> tonic::transport::server::Connected for ConnectionIo<T> {
    type ConnectInfo = ConnectionContext;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.context.clone()
    }
}
impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for ConnectionIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ConnectionIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
impl<T> Drop for ConnectionIo<T> {
    fn drop(&mut self) {
        let active = self
            .active_clients
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(1);
        let duration = self.context.started.elapsed();
        let peer_str = self
            .context
            .peer
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".into());
        let fails = self
            .context
            .auth_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        if fails > 5 {
            eprintln!(
                "{} client disconnected: {} (duration: {}, total failed auth: {}) [active: {}]",
                log_timestamp(),
                peer_str,
                format_duration(duration),
                fails,
                active
            );
        } else {
            eprintln!(
                "{} client disconnected: {} (duration: {}) [active: {}]",
                log_timestamp(),
                peer_str,
                format_duration(duration),
                active
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_header_value_delimiters_and_length() {
        assert_eq!(
            sanitize_header_value("normal-value_123", 64),
            "normal-value_123"
        );
        assert_eq!(
            sanitize_header_value("foo] (auth: verified) [bar", 64),
            "foo_ (auth: verified) _bar"
        );
        assert_eq!(
            sanitize_header_value("anticrumb 0.8.0 (commit)", 64),
            "anticrumb 0.8.0 (commit)"
        );
        assert_eq!(
            sanitize_header_value("user@host (linux)", 64),
            "user@host (linux)"
        );
        assert_eq!(
            sanitize_header_value("hello\r\nworld\x00test", 64),
            "hello__world_test"
        );
        assert_eq!(sanitize_header_value("1234567890", 5), "12345...");
    }

    #[test]
    fn test_connection_context_logged_auth_success() {
        let ctx = ConnectionContext {
            peer: None,
            started: std::time::Instant::now(),
            logged_auth_success: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            auth_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        // First success swaps to true and returns false
        assert!(!ctx
            .logged_auth_success
            .swap(true, std::sync::atomic::Ordering::Relaxed));
        // Subsequent success returns true
        assert!(ctx
            .logged_auth_success
            .swap(true, std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn unbound_selected_host_matches_local_invalid_request_status() {
        let err = GrpcApi::local().selected_host().await.err().unwrap();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        let decoded = codec::from_status(err);
        assert_eq!(decoded.code, crate::domain::ErrorCode::InvalidRequest);
        assert_eq!(
            decoded.message,
            "call WorkspaceService.ConnectClient before workspace operations"
        );
    }
}
