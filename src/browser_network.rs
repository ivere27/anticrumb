//! Per-context HTTP proxy: resolve and validate destinations once, then connect
//! to the checked socket address. Chrome never resolves the destination itself.
use crate::{
    url_lite::{Host, Url},
    web,
};
use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{oneshot, Semaphore},
};

type BodyError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BodyError>;
const REQUEST_LIMIT: usize = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_LIFETIME: Duration = Duration::from_secs(300);

pub(crate) struct BrowserNetworkProxy {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl BrowserNetworkProxy {
    pub(crate) fn start() -> Result<Self, String> {
        Self::start_with_policy(false)
    }

    fn start_with_policy(allow_private: bool) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let (stop, mut stopped) = oneshot::channel();
        let worker = std::thread::Builder::new().name("browser-network-proxy".into()).spawn(move || {
            runtime.block_on(async move {
                let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
                let permits = Arc::new(Semaphore::new(64));
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        _ = connections.join_next(), if !connections.is_empty() => {},
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                            connections.spawn(async move {
                                let permit = Arc::new(permit);
                                let service = service_fn(move |request| {
                                    let permit = permit.clone();
                                    async move {
                                        Ok::<_, Infallible>(match forward(request, allow_private, permit).await {
                                            Ok(response) => response,
                                            Err((status, message)) => error_response(status, message),
                                        })
                                    }
                                });
                                let mut builder = hyper::server::conn::http1::Builder::new();
                                builder.timer(TokioTimer::new()).header_read_timeout(CONNECT_TIMEOUT)
                                    .max_headers(128).max_buf_size(64 * 1024);
                                let connection = builder.serve_connection(TokioIo::new(stream), service).with_upgrades();
                                let _ = tokio::time::timeout(CONNECTION_LIFETIME, connection).await;
                            });
                        }
                    }
                }
                connections.abort_all();
                while connections.join_next().await.is_some() {}
            });
            // Dropping this private runtime also cancels all upgrade tunnels and
            // upstream connection drivers, closing their owned sockets.
            drop(runtime);
        }).map_err(|e| e.to_string())?;
        Ok(Self {
            address,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }
}
impl Drop for BrowserNetworkProxy {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn error_response(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(
            Full::new(Bytes::from(message))
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

async fn connect_checked(url: &Url, allow_private: bool) -> Result<TcpStream, String> {
    let port = url
        .port_or_known_default()
        .ok_or("missing destination port")?;
    let addresses: Vec<SocketAddr> = match url.host().ok_or("missing destination host")? {
        Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Host::Domain(host) => tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| "destination DNS timed out".to_owned())?
        .map_err(|e| format!("destination DNS failed: {e}"))?
        .collect(),
    };
    if addresses.is_empty() {
        return Err("destination has no addresses".into());
    }
    if !allow_private
        && addresses
            .iter()
            .any(|addr| web::is_disallowed_ip(addr.ip()))
    {
        return Err("blocked non-public destination address".into());
    }
    // Pin each connection to these already-validated addresses. Never hand the
    // hostname to TcpStream::connect or to Chrome's resolver after validation.
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut last = None;
        for address in addresses {
            match TcpStream::connect(address).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last = Some(error),
            }
        }
        Err(format!("destination connection failed: {}", last.unwrap()))
    })
    .await
    .map_err(|_| "destination connection timed out".to_owned())?
}

fn remove_hop_headers(headers: &mut header::HeaderMap, upgrade: bool) {
    let connection_headers = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_owned())
        .collect::<Vec<_>>();
    for name in connection_headers {
        if !(upgrade && name.eq_ignore_ascii_case("upgrade")) {
            headers.remove(name);
        }
    }
    for name in [
        "proxy-authorization",
        "proxy-authenticate",
        "proxy-connection",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if !upgrade {
        headers.remove(header::CONNECTION);
        headers.remove(header::UPGRADE);
    } else {
        headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
}

async fn forward(
    mut request: Request<Incoming>,
    allow_private: bool,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let is_connect = request.method() == Method::CONNECT;
    let destination = if is_connect {
        let authority = request.uri().authority().ok_or((
            StatusCode::BAD_REQUEST,
            "CONNECT requires a host and port".into(),
        ))?;
        if authority.port_u16().is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                "CONNECT requires an explicit valid port".into(),
            ));
        }
        format!("https://{authority}/")
    } else {
        request.uri().to_string()
    };
    let url = web::validate_http_url(&destination, allow_private)
        .map_err(|e| (StatusCode::FORBIDDEN, e))?;
    if !is_connect && url.scheme() != "http" {
        return Err((StatusCode::BAD_REQUEST, "HTTPS requires CONNECT".into()));
    }
    let stream = connect_checked(&url, allow_private)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, e))?;
    if is_connect {
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(CONNECTION_LIFETIME, async move {
                if let Ok(upgraded) = upgrade.await {
                    let mut downstream = TokioIo::new(upgraded);
                    let mut upstream = stream;
                    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                }
            })
            .await;
        });
        return Ok(error_response(StatusCode::OK, String::new()));
    }
    let wants_upgrade = request.headers().contains_key(header::UPGRADE);
    let downstream_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut request));
    remove_hop_headers(request.headers_mut(), wants_upgrade);
    let authority = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap()),
        None => url.host_str().unwrap().to_owned(),
    };
    request.headers_mut().insert(
        header::HOST,
        authority
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid destination host".into()))?,
    );
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    *request.uri_mut() = target
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid destination path".into()))?;
    let request = request.map(|body| Limited::new(body, REQUEST_LIMIT).boxed_unsync());
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = tokio::time::timeout(Duration::from_secs(60), sender.send_request(request))
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                "destination response timed out".into(),
            )
        })?
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let switched = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    if switched {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        if let Some(downstream_upgrade) = downstream_upgrade {
            tokio::spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(CONNECTION_LIFETIME, async move {
                    if let (Ok(down), Ok(up)) = tokio::join!(downstream_upgrade, upstream_upgrade) {
                        let _ = tokio::io::copy_bidirectional(
                            &mut TokioIo::new(down),
                            &mut TokioIo::new(up),
                        )
                        .await;
                    }
                })
                .await;
            });
        }
    }
    remove_hop_headers(response.headers_mut(), switched);
    Ok(response.map(|body| {
        body.map_err(|error| -> BodyError { Box::new(error) })
            .boxed_unsync()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream as BlockingStream},
        thread,
    };

    fn request_headers(stream: &mut BlockingStream) -> String {
        let mut data = Vec::new();
        while !data.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            data.push(byte[0]);
            assert!(data.len() <= 8192);
        }
        String::from_utf8(data).unwrap()
    }
    fn connect(address: SocketAddr) -> BlockingStream {
        let stream = BlockingStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }
    fn accept(listener: TcpListener) -> BlockingStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok((stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "upstream was never contacted"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn proxy_rejects_private_http_and_connect_aliases_before_connecting() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let port = destination.local_addr().unwrap().port();
        let proxy = BrowserNetworkProxy::start().unwrap();
        for host in [
            "127.0.0.1",
            "127.1",
            "2130706433",
            "0x7f000001",
            "localhost.",
            "[::1]",
        ] {
            for method in ["GET", "CONNECT"] {
                let mut client = connect(proxy.address());
                let target = if method == "CONNECT" {
                    format!("{host}:{port}")
                } else {
                    format!("http://{host}:{port}/private")
                };
                write!(client, "{method} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n").unwrap();
                let headers = request_headers(&mut client);
                assert!(
                    headers.starts_with("HTTP/1.1 403"),
                    "{method} {host}: {headers}"
                );
            }
        }
        assert!(
            matches!(destination.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn http_proxy_preserves_body_and_pins_host_to_destination() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = thread::spawn(move || {
            let mut stream = accept(listener);
            let headers = request_headers(&mut stream);
            assert!(
                headers.starts_with("POST /upload?value=1 HTTP/1.1"),
                "{headers}"
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains(&format!("host: {address}\r\n")),
                "{headers}"
            );
            assert!(!headers.to_ascii_lowercase().contains("proxy-authorization"));
            let mut body = [0; 4];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"body");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let proxy = BrowserNetworkProxy::start_with_policy(true).unwrap();
        let mut client = connect(proxy.address());
        write!(client, "POST http://{address}/upload?value=1 HTTP/1.1\r\nHost: wrong.example\r\nProxy-Authorization: secret\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody").unwrap();
        assert!(request_headers(&mut client).starts_with("HTTP/1.1 200"));
        let mut body = [0; 2];
        client.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"ok");
        upstream.join().unwrap();
    }

    #[test]
    fn connect_tunnel_streams_and_shutdown_closes_active_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = thread::spawn(move || {
            let mut stream = accept(listener);
            let mut body = [0; 4];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(&body).unwrap();
            assert_eq!(stream.read(&mut body).unwrap(), 0);
        });
        let proxy = BrowserNetworkProxy::start_with_policy(true).unwrap();
        let mut client = connect(proxy.address());
        write!(
            client,
            "CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n"
        )
        .unwrap();
        assert!(request_headers(&mut client).starts_with("HTTP/1.1 200"));
        client.write_all(b"ping").unwrap();
        let mut echo = [0; 4];
        client.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"ping");
        drop(proxy);
        assert_eq!(client.read(&mut echo).unwrap(), 0);
        upstream.join().unwrap();
    }

    #[test]
    fn http_upgrade_keeps_bidirectional_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = thread::spawn(move || {
            let mut stream = accept(listener);
            let headers = request_headers(&mut stream);
            assert!(
                headers.to_ascii_lowercase().contains("upgrade: fixture"),
                "{headers}"
            );
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: fixture\r\n\r\n").unwrap();
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).unwrap();
            stream.write_all(&bytes).unwrap();
        });
        let proxy = BrowserNetworkProxy::start_with_policy(true).unwrap();
        let mut client = connect(proxy.address());
        write!(client, "GET http://{address}/socket HTTP/1.1\r\nHost: {address}\r\nConnection: upgrade\r\nUpgrade: fixture\r\n\r\n").unwrap();
        assert!(request_headers(&mut client).starts_with("HTTP/1.1 101"));
        client.write_all(b"ping").unwrap();
        let mut echo = [0; 4];
        client.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"ping");
        upstream.join().unwrap();
    }

    #[test]
    fn upgraded_tunnels_keep_connection_permits_until_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop, stopped) = std::sync::mpsc::channel();
        let upstream = thread::spawn(move || {
            let mut sockets = Vec::new();
            while matches!(
                stopped.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ) {
                if let Ok((socket, _)) = listener.accept() {
                    sockets.push(socket);
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        let proxy = BrowserNetworkProxy::start_with_policy(true).unwrap();
        let mut clients = Vec::new();
        for _ in 0..64 {
            let mut client = connect(proxy.address());
            write!(
                client,
                "CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n"
            )
            .unwrap();
            assert!(request_headers(&mut client).starts_with("HTTP/1.1 200"));
            clients.push(client);
        }
        let mut overflow = connect(proxy.address());
        let _ = write!(
            overflow,
            "CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n"
        );
        let result = overflow.read(&mut [0]);
        assert!(
            matches!(result, Ok(0))
                || matches!(result, Err(ref error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted)),
            "{result:?}"
        );
        drop(clients);
        drop(proxy);
        stop.send(()).unwrap();
        upstream.join().unwrap();
    }
}
