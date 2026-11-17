use super::{failure, LATEST_RELEASE_URL};
use crate::domain::{ErrorCode, Result, ServiceError};
use std::time::Duration;

pub(super) struct UpdateClient {
    runtime: tokio::runtime::Runtime,
    connector: tokio_rustls::TlsConnector,
}

pub(super) fn network_client() -> Result<UpdateClient> {
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(loaded.certs);
    if roots.is_empty() {
        return Err(failure(
            "update_download",
            "cannot initialize HTTPS update client",
        ));
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| failure("update_download", "cannot initialize HTTPS update client"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| failure("update_download", "cannot initialize HTTPS update client"))?;
    Ok(UpdateClient {
        runtime,
        connector: tokio_rustls::TlsConnector::from(std::sync::Arc::new(config)),
    })
}

fn transport_error(error: &(dyn std::error::Error + 'static)) -> ServiceError {
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
    ServiceError::new(ErrorCode::TransportDisconnected, "update_download", message)
}

pub(super) fn validate_https_url(url: &crate::url_lite::Url, redirect_hops: usize) -> Result<()> {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(ServiceError::new(
            ErrorCode::TransportDisconnected,
            "update_download",
            if redirect_hops == 0 {
                "release request requires an HTTPS URL without credentials"
            } else {
                "release redirect is outside trusted HTTPS hosts"
            },
        ));
    }
    if redirect_hops > 0 {
        let trusted_host = matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        );
        if redirect_hops > 5 || !trusted_host {
            return Err(ServiceError::new(
                ErrorCode::TransportDisconnected,
                "update_download",
                "release redirect is outside trusted HTTPS hosts",
            ));
        }
    }
    Ok(())
}

fn resolve_redirect(base: &crate::url_lite::Url, location: &str) -> Result<crate::url_lite::Url> {
    if location.starts_with('/') {
        crate::url_lite::Url::parse(&format!("{}{location}", base.origin_ascii()))
            .map_err(|_| failure("update_download", "invalid redirect URL"))
    } else {
        crate::url_lite::Url::parse(location)
            .map_err(|_| failure("update_download", "invalid redirect URL"))
    }
}

pub(super) fn http_failure(
    status: http::StatusCode,
    headers: &http::HeaderMap,
    release_metadata: bool,
) -> ServiceError {
    let number = |name: &str| headers.get(name)?.to_str().ok()?.parse::<u64>().ok();
    if status == http::StatusCode::TOO_MANY_REQUESTS
        || (status == http::StatusCode::FORBIDDEN
            && (number("x-ratelimit-remaining") == Some(0) || number("retry-after").is_some()))
    {
        let retry = number("retry-after")
            .map(|seconds| format!("Retry after {seconds} seconds."))
            .unwrap_or_else(|| "Retry later after GitHub's rate limit resets.".into());
        return ServiceError::new(
            ErrorCode::RateLimited,
            "update_download",
            format!("GitHub API rate limit exceeded (HTTP {status}). {retry}"),
        );
    }
    let message = match status {
        http::StatusCode::NOT_FOUND if release_metadata => {
            "No public stable anticrumb release is available (GitHub HTTP 404). The repository may still be private or its first release is not published yet."
                .into()
        }
        http::StatusCode::FORBIDDEN => {
            "GitHub denied the release request (HTTP 403). Public requests may be temporarily rate-limited; retry later."
                .into()
        }
        http::StatusCode::NOT_FOUND => {
            "Release asset was not found (HTTP 404); it may have been removed after the version check. Retry the update."
                .into()
        }
        _ => format!("GitHub release request failed with HTTP {status}; nothing was installed"),
    };
    failure("update_download", message)
}

async fn fetch_async(
    connector: &tokio_rustls::TlsConnector,
    initial_url: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    use http_body_util::{BodyExt, Empty};

    let release_metadata = initial_url == LATEST_RELEASE_URL;
    let mut url = crate::url_lite::Url::parse(initial_url)
        .map_err(|_| failure("update_download", "invalid release URL"))?;

    for hops in 0..=5 {
        validate_https_url(&url, hops)?;
        let connect_host = match url.host() {
            Some(crate::url_lite::Host::Domain(host)) => host.to_owned(),
            Some(crate::url_lite::Host::Ipv4(ip)) => ip.to_string(),
            Some(crate::url_lite::Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(failure("update_download", "release URL missing host")),
        };
        let port = url.port_or_known_default().unwrap_or(443);
        let server_name = rustls::pki_types::ServerName::try_from(connect_host.clone())
            .map_err(|_| failure("update_download", "invalid HTTPS server name"))?;

        let tls_stream = tokio::time::timeout(Duration::from_secs(10), async {
            let tcp = tokio::net::TcpStream::connect((connect_host.as_str(), port)).await?;
            connector.connect(server_name, tcp).await
        })
        .await
        .map_err(|_| {
            ServiceError::new(
                ErrorCode::TransportDisconnected,
                "update_download",
                "connection timed out",
            )
        })?
        .map_err(|e| transport_error(&e))?;

        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls_stream))
                .await
                .map_err(|e| transport_error(&e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let target = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let host_header = match url.port() {
            Some(p) => format!("{}:{p}", url.host_str().unwrap_or(connect_host.as_str())),
            None => url.host_str().unwrap_or(connect_host.as_str()).to_owned(),
        };

        let mut builder = http::Request::builder()
            .method(http::Method::GET)
            .uri(target)
            .header(http::header::HOST, host_header)
            .header(
                http::header::USER_AGENT,
                concat!("anticrumb/", env!("CARGO_PKG_VERSION")),
            )
            .header(http::header::CONNECTION, "close");
        if url.as_str() == LATEST_RELEASE_URL {
            builder = builder
                .header(http::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2026-03-10");
        }
        let request = builder
            .body(Empty::<bytes::Bytes>::new())
            .map_err(|e| transport_error(&e))?;

        let response = sender
            .send_request(request)
            .await
            .map_err(|e| transport_error(&e))?;

        if matches!(
            response.status(),
            http::StatusCode::MOVED_PERMANENTLY
                | http::StatusCode::FOUND
                | http::StatusCode::SEE_OTHER
                | http::StatusCode::TEMPORARY_REDIRECT
                | http::StatusCode::PERMANENT_REDIRECT
        ) {
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorCode::TransportDisconnected,
                        "update_download",
                        "release redirect missing Location header",
                    )
                })?;
            url = resolve_redirect(&url, location)?;
            continue;
        }

        if !response.status().is_success() {
            return Err(http_failure(
                response.status(),
                response.headers(),
                release_metadata,
            ));
        }

        if response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|length| length > limit)
        {
            return Err(failure(
                "update_download",
                "release response exceeds its size limit",
            ));
        }

        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame
                .map_err(|_| failure("update_download", "release download was interrupted"))?;
            if let Some(chunk) = frame.data_ref() {
                if (bytes.len() as u64).saturating_add(chunk.len() as u64) > limit {
                    return Err(failure(
                        "update_download",
                        "release response exceeds its size limit",
                    ));
                }
                bytes.extend_from_slice(chunk);
            }
        }
        return Ok(bytes);
    }

    Err(ServiceError::new(
        ErrorCode::TransportDisconnected,
        "update_download",
        "release redirect is outside trusted HTTPS hosts",
    ))
}

pub(super) fn fetch(client: &UpdateClient, url: &str, limit: u64) -> Result<Vec<u8>> {
    client
        .runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(180),
                fetch_async(&client.connector, url, limit),
            )
            .await
        })
        .map_err(|_| {
            ServiceError::new(
                ErrorCode::TransportDisconnected,
                "update_download",
                "release download timed out",
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_does_not_panic_without_ambient_tokio_runtime() {
        if let Ok(client) = network_client() {
            let result = fetch(&client, "https://127.0.0.1:1/invalid", 1024);
            assert!(result.is_err());
        }
    }
}

