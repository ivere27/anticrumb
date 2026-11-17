//! Minimal URL parser replacing `url` + `idna` + `icu_*` Unicode normalization tables.
//! Supports `http`, `https`, `ws`, `wss` (and authority URLs) used for Gemini Web,
//! Chrome CDP endpoints, gRPC origins, and release downloads.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(&'static str);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host<'a> {
    Domain(&'a str),
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

pub struct Origin<'a>(&'a Url);

impl<'a> Origin<'a> {
    pub fn ascii_serialization(&self) -> String {
        self.0.origin_ascii()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    scheme: String,
    username: String,
    password: Option<String>,
    host: Option<OwnedHost>,
    port: Option<u16>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
    serialized: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedHost {
    Domain(String),
    Ipv4(Ipv4Addr, String),
    Ipv6(Ipv6Addr, String),
}

fn known_default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

impl Url {
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let input = raw.trim_matches(|c: char| c.is_ascii_control() || c == ' ');
        let (scheme_raw, after_colon) = input
            .split_once(':')
            .ok_or(ParseError("missing URL scheme"))?;
        if scheme_raw.is_empty()
            || !scheme_raw
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            || !scheme_raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            return Err(ParseError("invalid URL scheme"));
        }
        let scheme = scheme_raw.to_ascii_lowercase();

        let rest = after_colon
            .strip_prefix("//")
            .ok_or(ParseError("URL missing authority"))?;

        let (rest_before_frag, fragment) = match rest.split_once('#') {
            Some((before, frag)) => (before, Some(frag.to_string())),
            None => (rest, None),
        };
        let (authority_and_path, query) = match rest_before_frag.split_once('?') {
            Some((before, q)) => (before, Some(q.to_string())),
            None => (rest_before_frag, None),
        };

        let (authority, raw_path) = match authority_and_path.find(['/', '\\']) {
            Some(idx) => {
                let (auth, path) = authority_and_path.split_at(idx);
                if path.starts_with('\\') {
                    return Err(ParseError("invalid URL path separator"));
                }
                (auth, path)
            }
            None => (authority_and_path, ""),
        };

        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((ui, hp)) => (Some(ui), hp),
            None => (None, authority),
        };

        let (username, password) = match userinfo {
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (u.to_string(), Some(p.to_string())),
                None => (ui.to_string(), None),
            },
            None => (String::new(), None),
        };

        let (host_str, port_str) = if hostport.starts_with('[') {
            let end_bracket = hostport
                .find(']')
                .ok_or(ParseError("unclosed IPv6 host bracket"))?;
            let host_slice = &hostport[..=end_bracket];
            let after = &hostport[end_bracket + 1..];
            let port_slice = if let Some(p) = after.strip_prefix(':') {
                Some(p)
            } else if after.is_empty() {
                None
            } else {
                return Err(ParseError("invalid characters after IPv6 host"));
            };
            (host_slice, port_slice)
        } else {
            match hostport.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            }
        };

        if host_str.is_empty() {
            return Err(ParseError("URL missing host"));
        }

        let host = parse_host(host_str)?;
        let port = match port_str {
            None | Some("") => None,
            Some(p) => {
                let parsed = p
                    .parse::<u16>()
                    .map_err(|_| ParseError("invalid URL port"))?;
                if Some(parsed) == known_default_port(&scheme) {
                    None
                } else {
                    Some(parsed)
                }
            }
        };

        let path = if raw_path.is_empty() {
            "/".to_string()
        } else if raw_path.starts_with('/') {
            raw_path.to_string()
        } else {
            format!("/{raw_path}")
        };

        let mut url = Self {
            scheme,
            username,
            password,
            host: Some(host),
            port,
            path,
            query,
            fragment,
            serialized: String::new(),
        };
        url.rebuild_serialized();
        Ok(url)
    }

    fn rebuild_serialized(&mut self) {
        let mut out = String::new();
        out.push_str(&self.scheme);
        out.push_str("://");
        if !self.username.is_empty() || self.password.is_some() {
            out.push_str(&self.username);
            if let Some(pass) = &self.password {
                out.push(':');
                out.push_str(pass);
            }
            out.push('@');
        }
        if let Some(host_str) = self.host_str() {
            out.push_str(host_str);
        }
        if let Some(port) = self.port {
            if Some(port) != known_default_port(&self.scheme) {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if let Some(fragment) = &self.fragment {
            out.push('#');
            out.push_str(fragment);
        }
        self.serialized = out;
    }

    pub fn as_str(&self) -> &str {
        &self.serialized
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    pub fn host(&self) -> Option<Host<'_>> {
        self.host.as_ref().map(|h| match h {
            OwnedHost::Domain(d) => Host::Domain(d.as_str()),
            OwnedHost::Ipv4(ip, _) => Host::Ipv4(*ip),
            OwnedHost::Ipv6(ip, _) => Host::Ipv6(*ip),
        })
    }

    pub fn host_str(&self) -> Option<&str> {
        self.host.as_ref().map(|h| match h {
            OwnedHost::Domain(d) | OwnedHost::Ipv4(_, d) | OwnedHost::Ipv6(_, d) => d.as_str(),
        })
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn port_or_known_default(&self) -> Option<u16> {
        self.port.or_else(|| known_default_port(&self.scheme))
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn path_segments(&self) -> Option<std::str::Split<'_, char>> {
        self.host.as_ref()?;
        let trimmed = self.path.strip_prefix('/').unwrap_or(&self.path);
        Some(trimmed.split('/'))
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub fn query_pairs(&self) -> form_urlencoded::Parse<'_> {
        form_urlencoded::parse(self.query.as_deref().unwrap_or("").as_bytes())
    }

    pub fn fragment(&self) -> Option<&str> {
        self.fragment.as_deref()
    }

    pub fn origin(&self) -> Origin<'_> {
        Origin(self)
    }

    pub fn origin_ascii(&self) -> String {
        let Some(host_str) = self.host_str() else {
            return "null".to_string();
        };
        let mut out = format!("{}://{}", self.scheme, host_str);
        if let Some(port) = self.port {
            if Some(port) != known_default_port(&self.scheme) {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        out
    }

    pub fn set_path(&mut self, path: &str) {
        self.path = if path.is_empty() {
            "/".to_string()
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        self.rebuild_serialized();
    }

    pub fn set_query(&mut self, query: Option<&str>) {
        self.query = query.map(str::to_string);
        self.rebuild_serialized();
    }

    pub fn set_fragment(&mut self, fragment: Option<&str>) {
        self.fragment = fragment.map(str::to_string);
        self.rebuild_serialized();
    }

    pub fn query_pairs_mut(&mut self) -> QueryMut<'_> {
        QueryMut { url: self }
    }
}

pub struct QueryMut<'a> {
    url: &'a mut Url,
}

impl<'a> QueryMut<'a> {
    pub fn append_pair(&mut self, name: &str, value: &str) -> &mut Self {
        let encoded = form_urlencoded::Serializer::new(self.url.query.take().unwrap_or_default())
            .append_pair(name, value)
            .finish();
        self.url.query = Some(encoded);
        self.url.rebuild_serialized();
        self
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.serialized)
    }
}

fn parse_host(raw: &str) -> Result<OwnedHost, ParseError> {
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        let ip = inner
            .parse::<Ipv6Addr>()
            .map_err(|_| ParseError("invalid IPv6 host"))?;
        let text = format!("[{ip}]");
        return Ok(OwnedHost::Ipv6(ip, text));
    }
    if raw.contains(['[', ']', '@', '/', '\\', '?', '#'])
        || raw.bytes().any(|b| b <= b' ' || b == 0x7f)
    {
        return Err(ParseError("invalid host characters"));
    }
    if !raw.is_ascii() {
        return Err(ParseError("non-ASCII host is not supported"));
    }
    let lower = raw.to_ascii_lowercase();
    if let Ok(ip) = lower.parse::<Ipv4Addr>() {
        let text = ip.to_string();
        return Ok(OwnedHost::Ipv4(ip, text));
    }
    Ok(OwnedHost::Domain(lower))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_ipv4_ipv6_credentials_and_mutations() {
        let url = Url::parse("https://Example.COM:443/u/1/app?authuser=1#frag").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("example.com"));
        assert_eq!(url.host(), Some(Host::Domain("example.com")));
        assert_eq!(url.port(), None);
        assert_eq!(url.port_or_known_default(), Some(443));
        assert_eq!(url.path(), "/u/1/app");
        assert_eq!(
            url.path_segments().unwrap().collect::<Vec<_>>(),
            vec!["u", "1", "app"]
        );
        assert_eq!(url.query(), Some("authuser=1"));
        assert_eq!(url.fragment(), Some("frag"));
        assert_eq!(url.origin().ascii_serialization(), "https://example.com");
        assert_eq!(url.as_str(), "https://example.com/u/1/app?authuser=1#frag");

        let mut mutated = url.clone();
        mutated.set_path("/app/xyz");
        mutated.set_fragment(None);
        mutated.set_query(None);
        mutated.query_pairs_mut().append_pair("authuser", "2");
        assert_eq!(mutated.as_str(), "https://example.com/app/xyz?authuser=2");

        let v6 = Url::parse("https://[::1]:8443").unwrap();
        assert_eq!(v6.host_str(), Some("[::1]"));
        assert_eq!(v6.host(), Some(Host::Ipv6(Ipv6Addr::LOCALHOST)));
        assert_eq!(v6.port_or_known_default(), Some(8443));
        assert_eq!(v6.path(), "/");
        assert_eq!(v6.origin().ascii_serialization(), "https://[::1]:8443");

        let creds = Url::parse("https://user:secret-pass@host.invalid/?token=secret#f").unwrap();
        assert_eq!(creds.username(), "user");
        assert_eq!(creds.password(), Some("secret-pass"));
        assert_eq!(creds.origin().ascii_serialization(), "https://host.invalid");
        assert!(Url::parse("not a url secret").is_err());
        assert!(!Url::parse("not-a-url-private-token")
            .unwrap_err()
            .to_string()
            .contains("private-token"));
    }
}
