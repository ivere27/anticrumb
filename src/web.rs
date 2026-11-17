//! Workspace-local HTTP web_fetch tool, plus shared URL/SSRF and single-pass HTML helpers.
use crate::{
    agent::CancelToken,
    url_lite::{Host, Url},
};
use flate2::read::{GzDecoder, ZlibDecoder};
use http_body_util::{BodyExt, Empty};
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::OnceLock,
    time::Duration,
};
use tokio::net::TcpStream;

pub const DEFAULT_WEB_MAX_CHARS: usize = 12_000;
const MAX_WEB_MAX_CHARS: usize = 16_000;
const MIN_WEB_MAX_CHARS: usize = 200;
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 5;

pub fn allow_private_env() -> bool {
    matches!(
        std::env::var("ANTICRUMB_ALLOW_PRIVATE_FETCH").as_deref(),
        Ok("1" | "true" | "yes")
    )
}

fn is_disallowed_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
    let is_benchmark = octets[0] == 198 && (octets[1] == 18 || octets[1] == 19);
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || octets[0] == 0
        || octets[0] >= 240
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || is_cgnat
        || is_benchmark
}

pub fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_disallowed_ipv4(mapped);
            }
            let segments = v6.segments();
            // Allow ordinary global unicast, excluding special-purpose and
            // translation/tunneling prefixes that can encode private IPv4.
            (segments[0] & 0xe000) != 0x2000
                || (segments[0] == 0x2001 && segments[1] < 0x0200)
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || segments[0] == 0x2002
                || (segments[0] == 0x3fff && segments[1] < 0x1000)
        }
    }
}

pub fn validate_http_url(raw: &str, allow_private: bool) -> Result<Url, String> {
    let parsed = Url::parse(raw.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "unsupported URL scheme '{}'; only http and https are allowed",
            parsed.scheme()
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URLs containing credentials are not allowed".into());
    }
    let host = parsed.host().ok_or("URL missing host")?;
    if !allow_private {
        match host {
            Host::Domain(domain) => {
                let lower = domain.trim_end_matches('.').to_ascii_lowercase();
                if lower == "localhost"
                    || lower.ends_with(".localhost")
                    || lower.ends_with(".local")
                {
                    return Err(format!("blocked non-public host '{domain}'"));
                }
            }
            Host::Ipv4(v4) if is_disallowed_ip(IpAddr::V4(v4)) => {
                return Err(format!("blocked non-public IP address {v4}"));
            }
            Host::Ipv6(v6) if is_disallowed_ip(IpAddr::V6(v6)) => {
                return Err(format!("blocked non-public IP address [{v6}]"));
            }
            Host::Ipv4(_) | Host::Ipv6(_) => {}
        }
    }
    Ok(parsed)
}

fn resolve_and_validate_addrs(url: &Url, allow_private: bool) -> Result<Vec<SocketAddr>, String> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL missing port".to_string())?;
    let host_for_resolve = match url.host() {
        Some(Host::Domain(d)) => d.to_string(),
        Some(Host::Ipv4(ip)) => {
            if !allow_private && is_disallowed_ip(IpAddr::V4(ip)) {
                return Err(format!("blocked non-public IP address {ip}"));
            }
            return Ok(vec![SocketAddr::new(IpAddr::V4(ip), port)]);
        }
        Some(Host::Ipv6(ip)) => {
            if !allow_private && is_disallowed_ip(IpAddr::V6(ip)) {
                return Err(format!("blocked non-public IP address [{ip}]"));
            }
            return Ok(vec![SocketAddr::new(IpAddr::V6(ip), port)]);
        }
        None => return Err("URL missing host".into()),
    };

    let addrs: Vec<SocketAddr> = (host_for_resolve.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve host '{host_for_resolve}': {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no addresses resolved for '{host_for_resolve}'"));
    }
    if !allow_private {
        for addr in &addrs {
            if is_disallowed_ip(addr.ip()) {
                return Err(format!(
                    "blocked non-public address {} resolved for '{host_for_resolve}'",
                    addr.ip()
                ));
            }
        }
    }
    Ok(addrs)
}

/// Validate the addresses a browser navigation can resolve, in addition to URL syntax.
/// Browser callers must apply this policy to redirects and subrequests as well.
pub(crate) fn validate_public_url(raw: &str, allow_private: bool) -> Result<Url, String> {
    let url = validate_http_url(raw, allow_private)?;
    if !allow_private {
        resolve_and_validate_addrs(&url, false)?;
    }
    Ok(url)
}

/// Resolve intercepted browser requests without blocking the CDP event loop.
pub(crate) async fn validate_public_request(raw: &str) -> Result<(), String> {
    let url = validate_http_url(raw, false)?;
    let Some(Host::Domain(host)) = url.host() else {
        // Literal IP addresses were already checked by validate_http_url.
        return Ok(());
    };
    let port = url.port_or_known_default().ok_or("URL missing port")?;
    let mut addrs = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| format!("DNS lookup timed out for '{host}'"))?
    .map_err(|e| format!("cannot resolve host '{host}': {e}"))?
    .peekable();
    if addrs.peek().is_none() {
        return Err(format!("no addresses resolved for '{host}'"));
    }
    for addr in addrs {
        if is_disallowed_ip(addr.ip()) {
            return Err(format!(
                "blocked non-public address {} resolved for '{host}'",
                addr.ip()
            ));
        }
    }
    Ok(())
}

fn tls_connector() -> Result<tokio_rustls::TlsConnector, String> {
    static CONNECTOR: OnceLock<Result<tokio_rustls::TlsConnector, String>> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            let loaded = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(loaded.certs);
            if roots.is_empty() {
                return Err("cannot load system TLS root certificates".into());
            }
            let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("cannot initialize TLS client: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(
                config,
            )))
        })
        .clone()
}

fn resolve_redirect(base: &Url, location: &str) -> Result<Url, String> {
    let loc = location.trim();
    if loc.starts_with("//") {
        return Url::parse(&format!("{}:{loc}", base.scheme()))
            .map_err(|e| format!("invalid redirect URL: {e}"));
    }
    if loc.starts_with('/') {
        return Url::parse(&format!("{}{loc}", base.origin_ascii()))
            .map_err(|e| format!("invalid redirect URL: {e}"));
    }
    if loc.contains("://") {
        return Url::parse(loc).map_err(|e| format!("invalid redirect URL: {e}"));
    }
    let base_dir = match base.path().rfind('/') {
        Some(idx) => &base.path()[..=idx],
        None => "/",
    };
    Url::parse(&format!("{}{base_dir}{loc}", base.origin_ascii()))
        .map_err(|e| format!("invalid redirect URL: {e}"))
}

fn web_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static WEB_RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    WEB_RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("anticrumb-web")
                .build()
                .map_err(|e| format!("runtime init failed: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn run_blocking_async<F, T>(cancel: &CancelToken, fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send,
    T: Send,
{
    let exec = || -> Result<T, String> {
        web_runtime()?.block_on(async move {
            tokio::select! {
                res = fut => res,
                _ = cancel.cancelled() => Err("cancelled".to_string()),
                _ = tokio::time::sleep(FETCH_TIMEOUT) => Err(format!("web request timed out after {}s", FETCH_TIMEOUT.as_secs())),
            }
        })
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| s.spawn(exec).join())
            .map_err(|_| "web fetch worker thread panicked".to_string())?
    } else {
        exec()
    }
}

struct FetchedHttp {
    final_url: String,
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

async fn connect_and_send(
    url: &Url,
    addrs: &[SocketAddr],
    accept_header: &str,
) -> Result<http::Response<hyper::body::Incoming>, String> {
    let mut last_err = None;
    let mut connected = None;
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(*addr)).await {
            Ok(Ok(stream)) => {
                connected = Some(stream);
                break;
            }
            Ok(Err(e)) => last_err = Some(e.to_string()),
            Err(_) => last_err = Some("connection timed out".into()),
        }
    }
    let tcp = connected.ok_or_else(|| {
        format!(
            "cannot connect to {}: {}",
            url.host_str().unwrap_or_default(),
            last_err.unwrap_or_else(|| "unknown error".into())
        )
    })?;

    let target = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let host_header = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(target)
        .header(http::header::HOST, host_header)
        .header(
            http::header::USER_AGENT,
            concat!("anticrumb/", env!("CARGO_PKG_VERSION")),
        )
        .header(http::header::ACCEPT, accept_header)
        .header(http::header::ACCEPT_ENCODING, "gzip, deflate")
        .header(http::header::CONNECTION, "close")
        .body(Empty::<bytes::Bytes>::new())
        .map_err(|e| format!("invalid HTTP request: {e}"))?;

    if url.scheme() == "https" {
        let host_name = match url.host() {
            Some(Host::Domain(d)) => d.to_string(),
            Some(Host::Ipv4(ip)) => ip.to_string(),
            Some(Host::Ipv6(ip)) => ip.to_string(),
            None => return Err("URL missing host".into()),
        };
        let server_name = rustls::pki_types::ServerName::try_from(host_name)
            .map_err(|_| "invalid TLS server name".to_string())?;
        let connector = tls_connector()?;
        let tls_stream = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
            .await
            .map_err(|_| "TLS handshake timed out".to_string())?
            .map_err(|e| format!("TLS handshake failed: {e}"))?;
        send_http1(tls_stream, request).await
    } else {
        send_http1(tcp, request).await
    }
}

async fn send_http1<S>(
    stream: S,
    request: http::Request<Empty<bytes::Bytes>>,
) -> Result<http::Response<hyper::body::Incoming>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| format!("HTTP handshake failed: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
        .send_request(request)
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))
}

async fn fetch_http_async(
    initial_url: &str,
    accept_header: &str,
    allow_private: bool,
) -> Result<FetchedHttp, String> {
    let mut current_url = validate_http_url(initial_url, allow_private)?;
    for hop in 0..=MAX_REDIRECTS {
        validate_http_url(current_url.as_str(), allow_private)?;
        let addrs = resolve_and_validate_addrs(&current_url, allow_private)?;
        let response = connect_and_send(&current_url, &addrs, accept_header).await?;
        let status = response.status();
        if matches!(
            status,
            http::StatusCode::MOVED_PERMANENTLY
                | http::StatusCode::FOUND
                | http::StatusCode::SEE_OTHER
                | http::StatusCode::TEMPORARY_REDIRECT
                | http::StatusCode::PERMANENT_REDIRECT
        ) {
            if hop == MAX_REDIRECTS {
                return Err(format!("exceeded {MAX_REDIRECTS} HTTP redirects"));
            }
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "HTTP redirect missing Location header".to_string())?;
            current_url = resolve_redirect(&current_url, location)?;
            continue;
        }

        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let content_encoding = response
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        if !status.is_success() {
            return Err(format!("HTTP {status} from {}", current_url.as_str()));
        }

        if response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(|len| len > MAX_FETCH_BYTES)
        {
            return Err(format!(
                "response body exceeds {MAX_FETCH_BYTES} byte limit"
            ));
        }

        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| format!("response read interrupted: {e}"))?;
            if let Some(chunk) = frame.data_ref() {
                if bytes.len().saturating_add(chunk.len()) > MAX_FETCH_BYTES {
                    return Err(format!(
                        "response body exceeds {MAX_FETCH_BYTES} byte limit"
                    ));
                }
                bytes.extend_from_slice(chunk);
            }
        }
        let body = decompress_body(bytes, &content_encoding)?;
        return Ok(FetchedHttp {
            final_url: current_url.as_str().to_string(),
            status: status.as_u16(),
            content_type,
            body,
        });
    }
    Err("too many redirects".into())
}

fn decompress_body(bytes: Vec<u8>, content_encoding: &str) -> Result<Vec<u8>, String> {
    let enc = content_encoding
        .split(',')
        .next()
        .map(str::trim)
        .unwrap_or("");
    if enc.is_empty() || enc == "identity" {
        return Ok(bytes);
    }
    let mut out = Vec::with_capacity((bytes.len() * 2).min(MAX_FETCH_BYTES));
    match enc {
        "gzip" | "x-gzip" => {
            GzDecoder::new(bytes.as_slice())
                .take((MAX_FETCH_BYTES + 1) as u64)
                .read_to_end(&mut out)
                .map_err(|e| format!("gzip decompression failed: {e}"))?;
        }
        "deflate" => {
            ZlibDecoder::new(bytes.as_slice())
                .take((MAX_FETCH_BYTES + 1) as u64)
                .read_to_end(&mut out)
                .map_err(|e| format!("deflate decompression failed: {e}"))?;
        }
        _ => return Ok(bytes),
    }
    if out.len() > MAX_FETCH_BYTES {
        return Err(format!(
            "decompressed response body exceeds {MAX_FETCH_BYTES} byte limit"
        ));
    }
    Ok(out)
}

pub fn clamp_max_chars(requested: usize) -> usize {
    if requested == 0 {
        DEFAULT_WEB_MAX_CHARS
    } else {
        requested.clamp(MIN_WEB_MAX_CHARS, MAX_WEB_MAX_CHARS)
    }
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(&format!("\n... [truncated {} chars]", total - max_chars));
    out
}

fn decode_entity_at(input: &str) -> Option<(char, usize)> {
    let after = input.strip_prefix('&')?;
    let limit = after.len().min(15);
    let bytes = after.as_bytes();
    let semi = (0..limit).find(|&idx| bytes[idx] == b';')?;
    let entity = &after[..semi];
    let ch = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" | "#x27" | "#X27" => '\'',
        "nbsp" => ' ',
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            u32::from_str_radix(&entity[2..], 16)
                .ok()
                .and_then(char::from_u32)?
        }
        _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32)?,
        _ => return None,
    };
    Some((ch, 1 + semi + 1))
}

pub fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    while let Some(rel) = input[cursor..].find('&') {
        out.push_str(&input[cursor..cursor + rel]);
        let amp = cursor + rel;
        if let Some((ch, consumed)) = decode_entity_at(&input[amp..]) {
            out.push(ch);
            cursor = amp + consumed;
        } else {
            out.push('&');
            cursor = amp + 1;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return Some(0);
    }
    if n.len() > h.len() {
        return None;
    }
    let first_lo = n[0].to_ascii_lowercase();
    let first_hi = n[0].to_ascii_uppercase();
    let last_start = h.len() - n.len();
    let mut i = 0usize;
    while i <= last_start {
        let b = h[i];
        if (b == first_lo || b == first_hi) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut quote: u8 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if quote != 0 {
            if b == quote {
                quote = 0;
            }
        } else if b == b'"' || b == b'\'' {
            quote = b;
        } else if b == b'>' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_skip_block_close(html: &str, from: usize, tag: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let tag_bytes = tag.as_bytes();
    let mut pos = from;
    while let Some(rel) = bytes[pos..].iter().position(|&b| b == b'<') {
        let lt = pos + rel;
        let after = lt + 1;
        if bytes.get(after) == Some(&b'/') {
            let name_start = after + 1;
            let name_end = name_start + tag_bytes.len();
            if name_end <= bytes.len()
                && bytes[name_start..name_end].eq_ignore_ascii_case(tag_bytes)
            {
                let mut rest = name_end;
                while rest < bytes.len() && bytes[rest].is_ascii_whitespace() {
                    rest += 1;
                }
                if bytes.get(rest) == Some(&b'>') {
                    return Some(rest + 1);
                }
            }
        }
        pos = after;
    }
    None
}

fn extract_attr_inline(tag_raw: &str, attr: &str) -> Option<String> {
    let bytes = tag_raw.as_bytes();
    let attr_bytes = attr.as_bytes();
    let mut i = 0usize;
    while i + attr_bytes.len() < bytes.len() {
        let prev_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if prev_ok && bytes[i..i + attr_bytes.len()].eq_ignore_ascii_case(attr_bytes) {
            let mut j = i + attr_bytes.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'=') {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= bytes.len() {
                    return None;
                }
                let q = bytes[j];
                if q == b'"' || q == b'\'' {
                    let start = j + 1;
                    let end_rel = bytes[start..].iter().position(|&b| b == q)?;
                    return Some(decode_html_entities(&tag_raw[start..start + end_rel]));
                }
                let end_rel = bytes[j..]
                    .iter()
                    .position(|b| b.is_ascii_whitespace() || *b == b'>')
                    .unwrap_or(bytes.len() - j);
                return Some(decode_html_entities(&tag_raw[j..j + end_rel]));
            }
        }
        i += 1;
    }
    None
}

pub fn extract_title(html: &str) -> Option<String> {
    let start = find_ascii_ci(html, "<title")?;
    let after = &html[start..];
    let gt = after.find('>')?;
    let body = &after[gt + 1..];
    let end = find_ascii_ci(body, "</title>")?;
    let cleaned = decode_html_entities(body[..end].trim())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Single-pass HTML-to-Markdown/text converter optimized for coding-agent web_fetch:
/// skips comments/script/style/svg/template in one linear scan without full-buffer copies,
/// preserves headings, `<pre><code>` blocks, lists, and HTTP(S) links, and normalizes whitespace.
pub fn html_to_text(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity((html.len() / 2).max(64));
    let mut i = 0usize;
    let mut in_pre = false;
    let mut in_inline_code = false;
    let mut pending_space = false;
    let mut newline_run = 2usize; // suppress leading blank lines
    let mut link_href: Option<String> = None;
    let mut link_start_len = 0usize;

    let push_newline = |out: &mut String,
                        pending_space: &mut bool,
                        newline_run: &mut usize,
                        max_newlines: usize| {
        *pending_space = false;
        while out.ends_with(' ') {
            out.pop();
        }
        if *newline_run < max_newlines {
            out.push('\n');
            *newline_run += 1;
        }
    };

    let ensure_blank_line =
        |out: &mut String, pending_space: &mut bool, newline_run: &mut usize| {
            *pending_space = false;
            while out.ends_with(' ') {
                out.pop();
            }
            if out.is_empty() {
                *newline_run = 2;
                return;
            }
            while *newline_run < 2 {
                out.push('\n');
                *newline_run += 1;
            }
        };

    let push_ch = |out: &mut String,
                   pending_space: &mut bool,
                   newline_run: &mut usize,
                   ch: char,
                   in_pre: bool| {
        if in_pre {
            out.push(ch);
            *newline_run = if ch == '\n' { *newline_run + 1 } else { 0 };
            return;
        }
        if ch == '\n' || ch == '\r' || ch == ' ' || ch == '\t' || ch == '\u{00a0}' {
            if *newline_run == 0 {
                *pending_space = true;
            }
            return;
        }
        if *pending_space && *newline_run == 0 && !out.is_empty() {
            out.push(' ');
        }
        *pending_space = false;
        *newline_run = 0;
        out.push(ch);
    };

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            // HTML comment: <!-- ... -->
            if bytes[i..].starts_with(b"<!--") {
                if let Some(end_rel) = html[i + 4..].find("-->") {
                    i = i + 4 + end_rel + 3;
                    continue;
                }
                break;
            }
            let Some(gt) = find_tag_end(bytes, i + 1) else {
                break;
            };
            let tag_raw = &html[i + 1..gt];
            i = gt + 1;

            let trimmed = tag_raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
                continue;
            }
            let is_closing = trimmed.starts_with('/');
            let is_self_closing = trimmed.ends_with('/');
            let name_slice = trimmed
                .trim_start_matches('/')
                .split(|c: char| c.is_ascii_whitespace() || c == '/')
                .next()
                .unwrap_or("");
            let mut name_buf = [0u8; 16];
            let name_len = name_slice.len().min(name_buf.len());
            for (idx, &ch) in name_slice.as_bytes()[..name_len].iter().enumerate() {
                name_buf[idx] = ch.to_ascii_lowercase();
            }
            let tag = std::str::from_utf8(&name_buf[..name_len]).unwrap_or("");

            if !is_closing
                && !is_self_closing
                && matches!(tag, "script" | "style" | "noscript" | "svg" | "template")
            {
                if let Some(next_i) = find_skip_block_close(html, i, tag) {
                    i = next_i;
                    continue;
                }
                break;
            }

            match tag {
                "pre" => {
                    if is_closing {
                        if in_pre {
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            out.push_str("```\n\n");
                            in_pre = false;
                            pending_space = false;
                            newline_run = 2;
                        }
                    } else if !is_self_closing {
                        ensure_blank_line(&mut out, &mut pending_space, &mut newline_run);
                        out.push_str("```\n");
                        in_pre = true;
                        newline_run = 1;
                    }
                }
                "code" if !in_pre => {
                    if is_closing {
                        if in_inline_code {
                            out.push('`');
                            in_inline_code = false;
                            newline_run = 0;
                        }
                    } else if !is_self_closing {
                        if pending_space && newline_run == 0 && !out.is_empty() {
                            out.push(' ');
                        }
                        pending_space = false;
                        out.push('`');
                        in_inline_code = true;
                        newline_run = 0;
                    }
                }
                "br" => push_newline(&mut out, &mut pending_space, &mut newline_run, 2),
                "hr" => {
                    ensure_blank_line(&mut out, &mut pending_space, &mut newline_run);
                    out.push_str("---\n\n");
                    newline_run = 2;
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    if is_closing {
                        ensure_blank_line(&mut out, &mut pending_space, &mut newline_run);
                    } else {
                        ensure_blank_line(&mut out, &mut pending_space, &mut newline_run);
                        let level = (tag.as_bytes()[1] - b'0') as usize;
                        for _ in 0..level {
                            out.push('#');
                        }
                        out.push(' ');
                        newline_run = 0;
                    }
                }
                "li" => {
                    if is_closing {
                        push_newline(&mut out, &mut pending_space, &mut newline_run, 1);
                    } else {
                        push_newline(&mut out, &mut pending_space, &mut newline_run, 1);
                        out.push_str("- ");
                        newline_run = 0;
                    }
                }
                "a" if !in_pre => {
                    if is_closing {
                        if let Some(href) = link_href.take() {
                            let label = out[link_start_len..].trim().to_owned();
                            if !label.is_empty()
                                && label != href
                                && !label.contains('\n')
                                && label.len() <= 160
                            {
                                out.truncate(link_start_len);
                                out.push('[');
                                out.push_str(&label);
                                out.push_str("](");
                                out.push_str(&href);
                                out.push(')');
                                newline_run = 0;
                            }
                        }
                    } else if !is_self_closing && link_href.is_none() {
                        if let Some(href) = extract_attr_inline(tag_raw, "href") {
                            let href = href.trim().to_owned();
                            if href.starts_with("http://") || href.starts_with("https://") {
                                if pending_space && newline_run == 0 && !out.is_empty() {
                                    out.push(' ');
                                }
                                pending_space = false;
                                link_start_len = out.len();
                                link_href = Some(href);
                            }
                        }
                    }
                }
                "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "aside"
                | "nav" | "ul" | "ol" | "tr" | "table" | "blockquote" => {
                    link_href = None;
                    let max_nl = if matches!(tag, "p" | "blockquote" | "table") {
                        2
                    } else {
                        1
                    };
                    push_newline(&mut out, &mut pending_space, &mut newline_run, max_nl);
                }
                "td" | "th" if is_closing && newline_run == 0 => pending_space = true,
                _ => {}
            }
            continue;
        }

        if b == b'&' {
            if let Some((ch, consumed)) = decode_entity_at(&html[i..]) {
                push_ch(&mut out, &mut pending_space, &mut newline_run, ch, in_pre);
                i += consumed;
                continue;
            }
            push_ch(&mut out, &mut pending_space, &mut newline_run, '&', in_pre);
            i += 1;
            continue;
        }

        // Fast-path plain text run until next '<' or '&'
        let next_special = bytes[i..]
            .iter()
            .position(|&c| c == b'<' || c == b'&')
            .map_or(bytes.len(), |rel| i + rel);
        let chunk = &html[i..next_special];
        if in_pre {
            out.push_str(chunk);
            if !chunk.is_empty() {
                newline_run = if chunk.ends_with('\n') { 1 } else { 0 };
            }
        } else {
            for ch in chunk.chars() {
                push_ch(&mut out, &mut pending_space, &mut newline_run, ch, false);
            }
        }
        i = next_special;
    }

    while out.ends_with(['\n', ' ']) {
        out.pop();
    }
    out
}

pub fn format_untrusted_web_page(
    url: &str,
    status: Option<u16>,
    title: Option<&str>,
    body: &str,
    max_chars: usize,
) -> String {
    let limit = clamp_max_chars(max_chars);
    let truncated = truncate_chars(body.trim(), limit);
    let mut header = format!("URL: {url}");
    if let Some(code) = status {
        header.push_str(&format!("\nStatus: {code}"));
    }
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        header.push_str(&format!("\nTitle: {t}"));
    }
    format!(
        "{header}\n\n[UNTRUSTED_WEB_CONTENT source=\"{url}\"]\n{truncated}\n[/UNTRUSTED_WEB_CONTENT]"
    )
}

pub fn contains_bot_challenge(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("unfortunately, bots use duckduckgo")
        || lower.contains("cf-challenge")
        || lower.contains("challenge-platform")
        || lower.contains("g-recaptcha")
        || lower.contains("hcaptcha")
        || lower.contains("verify you are human")
        || (lower.contains("detected unusual traffic") && lower.contains("not a robot"))
}

fn looks_like_js_shell(raw_html: &str, extracted_text: &str) -> bool {
    if extracted_text.chars().count() >= 400 || raw_html.len() < 400 {
        return false;
    }
    let lower = raw_html.to_ascii_lowercase();
    extracted_text.trim().chars().count() < 160
        || lower.contains("enable javascript")
        || lower.contains("javascript is required")
        || lower.contains("id=\"__next\"")
        || lower.contains("id=\"root\"></div>")
        || lower.contains("id=\"app\"></div>")
}

pub fn web_fetch(
    raw_url: &str,
    format: &str,
    max_chars: usize,
    cancel: &CancelToken,
) -> Result<String, String> {
    web_fetch_with_policy(raw_url, format, max_chars, cancel, allow_private_env())
}

fn web_fetch_with_policy(
    raw_url: &str,
    format: &str,
    max_chars: usize,
    cancel: &CancelToken,
    allow_private: bool,
) -> Result<String, String> {
    if !matches!(format, "text" | "raw") {
        return Err("format must be 'text' or 'raw'".into());
    }
    let accept = "text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.8";
    let fetched = run_blocking_async(cancel, fetch_http_async(raw_url, accept, allow_private))?;
    let raw_text = String::from_utf8_lossy(&fetched.body);
    let looks_html =
        fetched.content_type.contains("html") || raw_text.trim_start().starts_with('<');
    let title = if looks_html {
        extract_title(&raw_text)
    } else {
        None
    };
    let extracted = if format == "text" && looks_html {
        let mut text = html_to_text(&raw_text);
        if looks_like_js_shell(&raw_text, &text) {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(
                "[Hint: This page appears to require JavaScript rendering or returned minimal static content. Use browser_fetch (or browser_control) if content is missing.]",
            );
        }
        text
    } else {
        raw_text.into_owned()
    };
    Ok(format_untrusted_web_page(
        &fetched.final_url,
        Some(fetched.status),
        title.as_deref(),
        &extracted,
        max_chars,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn ssrf_blocks_loopback_private_link_local_and_non_http_schemes() {
        for address in [
            "0.1.2.3",
            "240.0.0.1",
            "192.0.0.8",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "2001:db8::1",
        ] {
            assert!(is_disallowed_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(!is_disallowed_ip(address.parse().unwrap()), "{address}");
        }
        assert!(validate_http_url("file:///etc/passwd", false).is_err());
        assert!(validate_http_url("javascript:alert(1)", false).is_err());
        assert!(validate_http_url("https://user:pass@example.com/", false).is_err());
        assert!(validate_http_url("http://127.0.0.1:9222/json/list", false).is_err());
        assert!(validate_http_url("http://localhost:50051/", false).is_err());
        assert!(validate_http_url("http://169.254.169.254/latest/meta-data/", false).is_err());
        assert!(validate_http_url("http://10.0.0.1/internal", false).is_err());
        assert!(validate_http_url("http://192.168.1.1/", false).is_err());
        assert!(validate_http_url("http://[::1]:8080/", false).is_err());
        assert!(validate_http_url("http://[::ffff:127.0.0.1]/", false).is_err());
        assert!(validate_http_url("https://example.com/docs?a=1", false).is_ok());
    }

    #[test]
    fn public_url_validation_checks_resolved_addresses() {
        for host in ["127.0.0.1", "127.1", "0x7f000001", "localhost."] {
            assert!(
                validate_public_url(&format!("http://{host}/"), false).is_err(),
                "{host}"
            );
        }
        assert!(validate_public_url("http://localhost/", true).is_ok());
        assert!(validate_public_url("https://8.8.8.8/", false).is_ok());
    }

    #[tokio::test]
    async fn intercepted_requests_reject_private_hosts_and_accept_public_ips() {
        for host in ["127.0.0.1", "127.1", "0x7f000001", "localhost.", "[::1]"] {
            assert!(
                validate_public_request(&format!("http://{host}/"))
                    .await
                    .is_err(),
                "{host}"
            );
        }
        assert!(validate_public_request("https://8.8.8.8/").await.is_ok());
    }

    #[test]
    fn deflate_decoding_rejects_corruption_and_expansion_beyond_limit() {
        use flate2::{write::ZlibEncoder, Compression};
        use std::io::Write as _;
        let compressed = |bytes: &[u8]| {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        };
        assert_eq!(
            decompress_body(compressed(b"standard deflate"), "deflate").unwrap(),
            b"standard deflate"
        );
        let mut corrupt = compressed(b"standard deflate");
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        assert!(decompress_body(corrupt, "deflate").is_err());
        assert!(
            decompress_body(compressed(&vec![b'x'; MAX_FETCH_BYTES + 1]), "deflate")
                .unwrap_err()
                .contains("exceeds")
        );
    }

    #[test]
    fn html_to_text_single_pass_strips_noise_and_preserves_code_and_links() {
        let html = r#"
        <html>
          <head><title>Rust &amp; Docs</title><style>.x { color: red; }</style></head>
          <body>
            <!-- hidden comment -->
            <script>console.log("secret()");</script>
            <svg><path d="M0 0"/></svg>
            <h1>Hello &lt;World&gt;</h1>
            <p>Line&nbsp;one with <a href="https://example.org/docs">Docs</a> and <code>foo()</code>.</p>
            <pre>fn main() {
    println!("hi &amp; bye");
}</pre>
            <ul><li>First</li><li>Second</li></ul>
            <p>Line &#39;two&#39; &amp; &#x21;</p>
          </body>
        </html>"#;
        assert_eq!(extract_title(html).as_deref(), Some("Rust & Docs"));
        let text = html_to_text(html);
        assert!(text.contains("# Hello <World>"), "{text}");
        assert!(text.contains("[Docs](https://example.org/docs)"), "{text}");
        assert!(text.contains("`foo()`"), "{text}");
        assert!(
            text.contains("```\nfn main() {\n    println!(\"hi & bye\");\n}\n```"),
            "{text}"
        );
        assert!(text.contains("- First"), "{text}");
        assert!(text.contains("- Second"), "{text}");
        assert!(text.contains("Line 'two' & !"), "{text}");
        assert!(!text.contains("secret()"));
        assert!(!text.contains("color: red"));
        assert!(!text.contains("hidden comment"));
    }

    #[test]
    fn web_fetch_supports_redirects_gzip_deflate_and_js_shell_hint() {
        use flate2::{
            write::{GzEncoder, ZlibEncoder},
            Compression,
        };
        use std::io::Write as _;

        let (port_tx, port_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                port_tx.send(port).unwrap();
                tokio::pin!(stop_rx);
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        accepted = listener.accept() => {
                            let Ok((mut sock, _)) = accepted else {
                                break;
                            };
                            tokio::spawn(async move {
                                let mut buf = [0u8; 2048];
                                let n = sock.read(&mut buf).await.unwrap_or(0);
                                let req = String::from_utf8_lossy(&buf[..n]);
                                let path = req
                                    .lines()
                                    .next()
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .unwrap_or("/");
                                let resp_bytes: Vec<u8> = if path == "/old" {
                                    b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                                } else if path == "/network-path" {
                                    format!("HTTP/1.1 302 Found\r\nLocation: //127.0.0.1:{port}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
                                } else if path == "/final" {
                                    let body = "<html><head><title>Final Page</title></head><body><script>bad()</script><p>Rendered OK</p></body></html>";
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                        body.len(),
                                        body
                                    )
                                    .into_bytes()
                                } else if path == "/gzip" || path == "/deflate" {
                                    let raw = "<html><head><title>Gzip Page</title></head><body><p>Compressed OK</p></body></html>";
                                    let (encoding, compressed) = if path == "/deflate" {
                                        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
                                        enc.write_all(raw.as_bytes()).unwrap();
                                        ("deflate", enc.finish().unwrap())
                                    } else {
                                        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
                                        enc.write_all(raw.as_bytes()).unwrap();
                                        ("gzip", enc.finish().unwrap())
                                    };
                                    let mut hdr = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        compressed.len()
                                    )
                                    .into_bytes();
                                    hdr.extend_from_slice(&compressed);
                                    hdr
                                } else if path == "/spa" {
                                    let padding = "x".repeat(450);
                                    let body = format!(
                                        "<html><head><title>SPA</title><script>/* {padding} */</script></head><body><div id=\"root\"></div><noscript>You need to enable JavaScript to run this app.</noscript></body></html>"
                                    );
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                        body.len(),
                                        body
                                    )
                                    .into_bytes()
                                } else {
                                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                                };
                                let _ = sock.write_all(&resp_bytes).await;
                                let _ = sock.shutdown().await;
                            });
                        }
                    }
                }
            });
        });
        let port = port_rx.recv().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let cancel = CancelToken::new();

        let fetched = web_fetch_with_policy(
            &format!("http://127.0.0.1:{port}/old"),
            "text",
            1000,
            &cancel,
            true,
        )
        .unwrap();
        assert!(fetched.contains("Title: Final Page"));
        assert!(fetched.contains("Rendered OK"));
        assert!(!fetched.contains("bad()"));
        assert!(fetched.contains("[UNTRUSTED_WEB_CONTENT"));

        let redirected = web_fetch_with_policy(
            &format!("http://127.0.0.1:{port}/network-path"),
            "text",
            1000,
            &cancel,
            true,
        )
        .unwrap();
        assert!(redirected.contains("Title: Final Page"));
        assert!(redirected.contains(&format!("URL: http://127.0.0.1:{port}/final")));

        let deflated = web_fetch_with_policy(
            &format!("http://127.0.0.1:{port}/deflate"),
            "text",
            1000,
            &cancel,
            true,
        )
        .unwrap();
        assert!(deflated.contains("Compressed OK"));

        let gzipped = web_fetch_with_policy(
            &format!("http://127.0.0.1:{port}/gzip"),
            "text",
            1000,
            &cancel,
            true,
        )
        .unwrap();
        assert!(gzipped.contains("Title: Gzip Page"));
        assert!(gzipped.contains("Compressed OK"));

        let spa = web_fetch_with_policy(
            &format!("http://127.0.0.1:{port}/spa"),
            "text",
            1000,
            &cancel,
            true,
        )
        .unwrap();
        assert!(spa.contains("Use browser_fetch"), "{spa}");
        assert!(contains_bot_challenge(
            "Our systems have detected unusual traffic from your computer network... and not a robot."
        ));

        let _ = stop_tx.send(());
        let _ = server.join();
    }
}
