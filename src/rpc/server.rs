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
pub struct GrpcApi {
    pub host: Arc<Host>,
    pub token: Option<String>,
    local: Option<super::local::LocalInfo>,
}
impl GrpcApi {
    pub fn new(host: Arc<Host>, token: Option<String>) -> Self {
        Self {
            host,
            token,
            local: None,
        }
    }
    fn for_request(&self, extensions: &http::Extensions) -> Self {
        let mut api = self.clone();
        if api.token.is_none() {
            api.local = extensions.get::<super::local::LocalInfo>().cloned();
        }
        api
    }
    async fn selected_host(&self) -> std::result::Result<Arc<Host>, tonic::Status> {
        match &self.local {
            Some(local) => local.host().await.map_err(codec::status),
            None => Ok(self.host.clone()),
        }
    }
    pub fn authorize(&self, metadata: &MetadataMap) -> std::result::Result<(), tonic::Status> {
        let Some(expected) = &self.token else {
            return Ok(());
        };
        let provided = metadata
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if expected.is_empty() || !super::constant_time_eq(provided, expected) {
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
    pub async fn stream(
        &self,
        _path: &str,
        request: pb::EventCursor,
    ) -> std::result::Result<tonic::Response<TypedStream<pb::Event>>, tonic::Status> {
        let rx = self
            .selected_host()
            .await?
            .watch(request)
            .await
            .map_err(codec::status)?;
        Ok(tonic::Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx).map(|v| v.map_err(codec::status)),
        )))
    }
}
include!(concat!(env!("OUT_DIR"), "/grpc_bindings.rs"));

#[derive(Clone)]
pub struct TlsIdentity {
    certificate: Vec<u8>,
    key: Vec<u8>,
}
impl TlsIdentity {
    pub fn from_pem(certificate: Vec<u8>, key: Vec<u8>) -> Self {
        Self { certificate, key }
    }
    pub(crate) fn acceptor(&self) -> Result<tokio_rustls::TlsAcceptor> {
        let certificates = rustls_pemfile::certs(&mut &self.certificate[..])
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(ServiceError::storage)?;
        let key = rustls_pemfile::private_key(&mut &self.key[..])
            .map_err(ServiceError::storage)?
            .ok_or_else(|| ServiceError::invalid("TLS private key is missing"))?;
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(ServiceError::storage)?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(ServiceError::storage)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }
}

pub async fn serve<I, IO>(
    host: Arc<Host>,
    token: Option<String>,
    incoming: I,
    tls: Option<TlsIdentity>,
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
    if let Some(identity) = tls {
        let acceptor = identity.acceptor()?;
        let secure = incoming
            .map(move |socket| {
                let acceptor = acceptor.clone();
                async move {
                    let socket = socket.ok()?;
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        acceptor.accept(socket),
                    )
                    .await
                    {
                        Ok(Ok(socket)) => Some(Ok(TlsIo(socket))),
                        _ => None,
                    }
                }
            })
            .buffer_unordered(64)
            .filter_map(|socket| async move { socket });
        let shutdown = shutdown(host.clone());
        serve_routes(host, token, secure, shutdown).await
    } else {
        let shutdown = shutdown(host.clone());
        serve_routes(host, token, incoming, shutdown).await
    }
}

async fn shutdown(host: Arc<Host>) {
    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=host.service.shutdown.notified()=>{}}
    host.shutdown();
}

pub(crate) async fn serve_local<I, IO>(
    host: Arc<Host>,
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
    serve_routes(host, None, incoming, shutdown).await
}

async fn serve_routes<I, IO>(
    host: Arc<Host>,
    token: Option<String>,
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
    let api = GrpcApi::new(host.clone(), token.clone());
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
    if host.role == "workspace" {
        routes = routes.add_service(service!(workspace_service_server, WorkspaceServiceServer));
    }
    if let Some(token) = token {
        let provider = crate::http::router(host.service.clone(), token)?;
        routes = routes.into_axum_router().fallback_service(provider).into();
    }
    builder
        .add_routes(routes)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(ServiceError::storage)
}

struct TlsIo<T>(T);
impl<T> tonic::transport::server::Connected for TlsIo<T> {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}
impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for TlsIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buffer)
    }
}
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for TlsIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
