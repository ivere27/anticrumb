use super::{codec, pb, MAX_MESSAGE};
use crate::domain::{ErrorCode, Result, ServiceError};
use futures_util::{Stream, StreamExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, pin::Pin, sync::Arc, time::Duration};
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
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(45);

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
            return Err(ServiceError::invalid("RPC message exceeds 48 MiB"));
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
        let path = "/anticrumb.v1.TurnService/WatchEvents";
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
        let v = crate::cli::client_version().parse().ok();
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
