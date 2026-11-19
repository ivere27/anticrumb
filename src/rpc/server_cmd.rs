pub use super::private_write;
use super::server::TlsIdentity;
use super::{host::Host, server, tls, validate_token};
use crate::domain::{Result, ServiceError};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn state_dir() -> PathBuf {
    crate::cli::app_dir().join("server")
}

pub struct Options {
    pub root: PathBuf,
    pub listen: String,
    pub token: Option<String>,
    pub regenerate_token: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub self_signed: bool,
    pub regenerate_tls: bool,
    pub names: Vec<String>,
    pub workers: Option<usize>,
    pub pace: Option<crate::gemini::HumanPace>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            listen: "0.0.0.0:0".into(),
            token: None,
            regenerate_token: false,
            tls_cert: None,
            tls_key: None,
            self_signed: false,
            regenerate_tls: false,
            names: Vec::new(),
            workers: None,
            pace: None,
        }
    }
}

fn normalize_listen_address(raw: &str) -> Result<String> {
    if let Some(port) = raw.strip_prefix(':') {
        if port.parse::<u16>().is_ok() {
            return Ok(format!("0.0.0.0:{port}"));
        }
    }
    raw.parse::<std::net::SocketAddr>()
        .map(|_| raw.to_owned())
        .map_err(|_| {
            ServiceError::invalid(
                "invalid server listen address; use `anticrumb server`, `anticrumb server :PORT`, or `anticrumb server IP:PORT`, such as 0.0.0.0:50051",
            )
        })
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        Self::parse_with_token(args, || std::env::var("ANTICRUMB_TOKEN").ok())
    }
    fn parse_with_token(
        args: &[String],
        inherited_token: impl FnOnce() -> Option<String>,
    ) -> Result<Self> {
        let mut options = Self {
            root: state_dir(),
            listen: "0.0.0.0:0".into(),
            ..Default::default()
        };
        let mut listen_set = false;
        let mut args = args.iter().peekable();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::iter::Peekable<std::slice::Iter<'_, String>>| {
                args.next()
                    .cloned()
                    .ok_or_else(|| ServiceError::invalid(format!("missing value for {arg}")))
            };
            match arg.as_str() {
                "--state-dir" => options.root = value(&mut args)?.into(),
                "--listen" => {
                    if listen_set {
                        return Err(ServiceError::invalid(
                            "server listen address may only be specified once",
                        ));
                    }
                    listen_set = true;
                    if args.peek().is_none_or(|next| next.starts_with('-')) {
                        options.listen = "0.0.0.0:0".into();
                    } else {
                        let raw = value(&mut args)?;
                        options.listen = normalize_listen_address(&raw)?;
                    }
                }
                "--token" => options.token = Some(value(&mut args)?),
                "--regenerate-token" => options.regenerate_token = true,
                "--tls-cert" => options.tls_cert = Some(value(&mut args)?.into()),
                "--tls-key" => options.tls_key = Some(value(&mut args)?.into()),
                "--tls-self-signed" => options.self_signed = true,
                "--regenerate-tls" => options.regenerate_tls = true,
                "--tls-san" => options.names.push(value(&mut args)?),
                "--workers" => {
                    let workers: usize = value(&mut args)?
                        .parse()
                        .map_err(|_| ServiceError::invalid("--workers must be 1..16"))?;
                    if !(1..=16).contains(&workers) {
                        return Err(ServiceError::invalid("--workers must be 1..16"));
                    }
                    options.workers = Some(workers);
                }
                "--pace" => {
                    let raw = value(&mut args)?;
                    let pace = crate::gemini::HumanPace::parse(&raw).ok_or_else(|| {
                        ServiceError::invalid("--pace must be fast, low, or high")
                    })?;
                    options.pace = Some(pace);
                }
                s if !s.starts_with('-') => {
                    if listen_set {
                        return Err(ServiceError::invalid(
                            "server listen address may only be specified once",
                        ));
                    }
                    listen_set = true;
                    options.listen = normalize_listen_address(s)?;
                }
                _ => {
                    return Err(ServiceError::invalid(
                        "unknown server option; use `anticrumb server --help`",
                    ))
                }
            }
        }
        if options.tls_cert.is_some() != options.tls_key.is_some() {
            return Err(ServiceError::invalid(
                "--tls-cert and --tls-key must be supplied together",
            ));
        }
        if options.self_signed && options.tls_cert.is_some() {
            return Err(ServiceError::invalid(
                "choose self-signed or supplied TLS identity",
            ));
        }
        if options.tls_cert.is_none() {
            options.self_signed = true;
        }
        if options.regenerate_tls && options.tls_cert.is_some() {
            return Err(ServiceError::invalid(
                "--regenerate-tls only replaces the generated self-signed TLS identity",
            ));
        }
        options.token = options.token.or_else(inherited_token);
        if options.regenerate_token && options.token.is_some() {
            return Err(ServiceError::invalid(
                "--regenerate-token only replaces the generated server token; omit --token and unset ANTICRUMB_TOKEN",
            ));
        }
        if let Some(token) = &options.token {
            validate_token(token)?;
        }
        Ok(options)
    }
    pub fn network_token(&self) -> Result<String> {
        if self.regenerate_token && self.token.is_some() {
            return Err(ServiceError::invalid(
                "--regenerate-token only replaces the generated server token",
            ));
        }
        let value = match &self.token {
            Some(token) => token.clone(),
            None if self.regenerate_token => crate::cli::regenerate_token(&self.root)?,
            None => crate::cli::token(&self.root)?,
        };
        validate_token(&value)?;
        Ok(value)
    }
    fn desired_names(&self) -> Result<(std::net::SocketAddr, Vec<String>)> {
        let address: std::net::SocketAddr = self.listen.parse().map_err(ServiceError::storage)?;
        let mut names = self.names.clone();
        names.push("localhost".into());
        if address.ip().is_unspecified() {
            if address.is_ipv4() {
                names.push("127.0.0.1".into());
            } else {
                names.push("::1".into());
            }
            if let Some(host) = valid_dns_hostname() {
                names.push(host);
            }
            for ip in local_interface_ips() {
                if ip.is_ipv4() == address.is_ipv4() {
                    names.push(ip.to_string());
                }
            }
        } else {
            names.extend(["127.0.0.1".into(), "::1".into(), address.ip().to_string()]);
        }
        names.sort();
        names.dedup();
        Ok((address, names))
    }
    pub fn identity(&self) -> Result<Option<TlsIdentity>> {
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            return Err(ServiceError::invalid(
                "--tls-cert and --tls-key must be supplied together",
            ));
        }
        if self.tls_cert.is_some() && (self.self_signed || self.regenerate_tls) {
            return Err(ServiceError::invalid(
                "choose generated or supplied TLS identity",
            ));
        }
        let (cert_pem, key_pem) = if self.tls_cert.is_none() {
            let directory = self.root.join("tls");
            let ca_cert = directory.join("ca-certificate.pem");
            let ca_key = directory.join("ca-private-key.pem");
            let cert = directory.join("certificate.pem");
            let key = directory.join("private-key.pem");
            let manifest = directory.join("names.json");
            let (address, full) = self.desired_names()?;
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if !self.regenerate_tls && directory.exists() {
                let ca_cert_bytes = fs::read(&ca_cert).map_err(|_| {
                    ServiceError::invalid(
                        "TLS CA identity is missing or incomplete; restart with --regenerate-tls to regenerate it",
                    )
                })?;
                let ca_key_bytes = fs::read(&ca_key).map_err(|_| {
                    ServiceError::invalid(
                        "TLS CA identity is missing or incomplete; restart with --regenerate-tls to regenerate it",
                    )
                })?;
                let ca_key_valid = std::str::from_utf8(&ca_key_bytes)
                    .ok()
                    .and_then(|pem| {
                        rcgen::KeyPair::from_pem_and_sign_algo(pem, &rcgen::PKCS_ED25519).ok()
                    })
                    .is_some();
                let ca_valid = ca_key_valid
                    && certificate_der(&ca_cert_bytes)
                        .ok()
                        .and_then(|der| parse_certificate_summary(&der))
                        .and_then(|summary| summary.not_after_epoch)
                        .is_some_and(|not_after| now_secs + LEAF_RENEW_BEFORE_SECS < not_after);
                if !ca_valid {
                    return Err(ServiceError::invalid(
                        "TLS CA identity is invalid or expired; restart with --regenerate-tls to regenerate it",
                    ));
                }
                let previous: Vec<String> = serde_json::from_slice(
                    &fs::read(&manifest).map_err(|_| {
                        ServiceError::invalid(
                            "TLS leaf manifest is missing; restart with --regenerate-tls to regenerate it",
                        )
                    })?,
                )
                .map_err(|_| {
                    ServiceError::invalid(
                        "TLS leaf manifest is invalid; restart with --regenerate-tls to regenerate it",
                    )
                })?;
                let compatible = if address.ip().is_unspecified() {
                    full.iter().all(|name| previous.contains(name))
                } else {
                    previous == full
                };
                let leaf_valid = fs::read(&cert)
                    .ok()
                    .and_then(|pem| certificate_der(&pem).ok())
                    .and_then(|der| parse_certificate_summary(&der))
                    .and_then(|summary| summary.not_after_epoch)
                    .is_some_and(|not_after| now_secs + LEAF_RENEW_BEFORE_SECS < not_after)
                    && key.exists();
                if !(compatible && leaf_valid) {
                    install_generated_identity(
                        &directory,
                        &full,
                        Some((ca_cert_bytes, ca_key_bytes)),
                    )?;
                }
            } else {
                install_generated_identity(&directory, &full, None)?;
            }
            let mut chain_pem = fs::read(&cert).map_err(ServiceError::storage)?;
            if !chain_pem.ends_with(b"\n") {
                chain_pem.push(b'\n');
            }
            chain_pem.extend_from_slice(&fs::read(&ca_cert).map_err(ServiceError::storage)?);
            let key_pem = fs::read(&key).map_err(ServiceError::storage)?;
            (chain_pem, key_pem)
        } else if let (Some(cert), Some(key)) = (&self.tls_cert, &self.tls_key) {
            (
                fs::read(cert).map_err(ServiceError::storage)?,
                fs::read(key).map_err(ServiceError::storage)?,
            )
        } else {
            return Err(ServiceError::invalid(
                "network server requires a TLS identity",
            ));
        };
        Ok(Some(TlsIdentity::from_pem(cert_pem, key_pem)))
    }
    pub fn rotate_leaf(&self, tls: &TlsIdentity) -> Result<()> {
        if self.tls_cert.is_some() {
            return Err(ServiceError::invalid(
                "leaf rotation requires a generated TLS identity",
            ));
        }
        let directory = self.root.join("tls");
        let ca_cert =
            fs::read(directory.join("ca-certificate.pem")).map_err(ServiceError::storage)?;
        let ca_key =
            fs::read(directory.join("ca-private-key.pem")).map_err(ServiceError::storage)?;
        let (_, full) = self.desired_names()?;
        install_generated_identity(&directory, &full, Some((ca_cert.clone(), ca_key)))?;
        let mut chain_pem =
            fs::read(directory.join("certificate.pem")).map_err(ServiceError::storage)?;
        if !chain_pem.ends_with(b"\n") {
            chain_pem.push(b'\n');
        }
        chain_pem.extend_from_slice(&ca_cert);
        let key_pem = fs::read(directory.join("private-key.pem")).map_err(ServiceError::storage)?;
        tls.replace_pem(chain_pem, key_pem)
    }
}

fn valid_dns_hostname() -> Option<String> {
    let raw = crate::cli::local_hostname()?;
    let candidate = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if candidate.is_empty() || candidate.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    rcgen::Ia5String::try_from(candidate.as_str()).ok()?;
    rustls::pki_types::ServerName::try_from(candidate.as_str()).ok()?;
    Some(candidate)
}

fn is_usable_interface_ip(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        std::net::IpAddr::V4(v4) => !v4.is_link_local(),
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

#[cfg(unix)]
fn local_interface_ips() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut ips = Vec::new();
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
        return ips;
    }
    let mut cur = addrs;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        let flags = ifa.ifa_flags as u32;
        if !ifa.ifa_addr.is_null()
            && (flags & libc::IFF_UP as u32) != 0
            && (flags & libc::IFF_LOOPBACK as u32) == 0
        {
            let family = unsafe { std::ptr::read_unaligned(ifa.ifa_addr).sa_family as i32 };
            if family == libc::AF_INET {
                let sin =
                    unsafe { std::ptr::read_unaligned(ifa.ifa_addr as *const libc::sockaddr_in) };
                let ip = IpAddr::V4(Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()));
                if is_usable_interface_ip(ip) {
                    ips.push(ip);
                }
            } else if family == libc::AF_INET6 {
                let sin6 =
                    unsafe { std::ptr::read_unaligned(ifa.ifa_addr as *const libc::sockaddr_in6) };
                let ip = IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr));
                if is_usable_interface_ip(ip) {
                    ips.push(ip);
                }
            }
        }
        cur = ifa.ifa_next;
    }
    unsafe { libc::freeifaddrs(addrs) };
    ips
}

#[cfg(windows)]
fn local_interface_ips() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use windows_sys::Win32::{
        Foundation::ERROR_BUFFER_OVERFLOW,
        NetworkManagement::{
            IpHelper::{GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH},
            Ndis::IfOperStatusUp,
        },
        Networking::WinSock::{
            IpDadStatePreferred, AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
        },
    };
    let mut ips = Vec::new();
    let mut size: u32 = 16 * 1024;
    let mut buffer = vec![0u8; size as usize];
    let mut status = unsafe {
        GetAdaptersAddresses(
            AF_UNSPEC as u32,
            0,
            std::ptr::null(),
            buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        )
    };
    if status == ERROR_BUFFER_OVERFLOW && size > 0 {
        buffer.resize(size as usize, 0);
        status = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                0,
                std::ptr::null(),
                buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            )
        };
    }
    if status != 0 || size == 0 {
        return ips;
    }
    let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !adapter.is_null() {
        let current = unsafe { &*adapter };
        if current.OperStatus == IfOperStatusUp {
            let mut unicast = current.FirstUnicastAddress;
            while !unicast.is_null() {
                let addr = unsafe { &*unicast };
                if addr.DadState == IpDadStatePreferred {
                    let sa = addr.Address.lpSockaddr;
                    let len = addr.Address.iSockaddrLength;
                    if !sa.is_null() && len >= std::mem::size_of::<u16>() as i32 {
                        let family = unsafe { std::ptr::read_unaligned(sa).sa_family };
                        if family == AF_INET && len >= std::mem::size_of::<SOCKADDR_IN>() as i32 {
                            let sin = unsafe { std::ptr::read_unaligned(sa as *const SOCKADDR_IN) };
                            let b = unsafe { sin.sin_addr.S_un.S_un_b };
                            let ip = IpAddr::V4(Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4));
                            if is_usable_interface_ip(ip) {
                                ips.push(ip);
                            }
                        } else if family == AF_INET6
                            && len >= std::mem::size_of::<SOCKADDR_IN6>() as i32
                        {
                            let sin6 =
                                unsafe { std::ptr::read_unaligned(sa as *const SOCKADDR_IN6) };
                            let ip = IpAddr::V6(Ipv6Addr::from(unsafe { sin6.sin6_addr.u.Byte }));
                            if is_usable_interface_ip(ip) {
                                ips.push(ip);
                            }
                        }
                    }
                }
                unicast = addr.Next;
            }
        }
        adapter = current.Next;
    }
    ips
}

#[cfg(not(any(unix, windows)))]
fn local_interface_ips() -> Vec<std::net::IpAddr> {
    Vec::new()
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            if leap {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn utc_parts(epoch_secs: u64) -> (i32, u8, u8, u64) {
    let secs_of_day = epoch_secs % 86400;
    let z = (epoch_secs / 86400) as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u8, d as u8, secs_of_day)
}

fn add_months(year: i32, month: u8, day: u8, months: u8) -> (i32, u8, u8) {
    let total = i32::from(month - 1) + i32::from(months);
    let next_year = year + total / 12;
    let next_month = (total % 12 + 1) as u8;
    let next_day = day.min(days_in_month(next_year, next_month));
    (next_year, next_month, next_day)
}

fn epoch_from_utc(year: i32, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> Option<u64> {
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || min > 59
        || sec > 59
    {
        return None;
    }
    let y = i64::from(year) - if month <= 2 { 1 } else { 0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + (doe as i64) - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86400 + u64::from(hour) * 3600 + u64::from(min) * 60 + u64::from(sec))
}

#[cfg(test)]
fn format_utc(epoch_secs: u64) -> String {
    let (y, m, d, secs_of_day) = utc_parts(epoch_secs);
    let h = secs_of_day / 3600;
    let min = (secs_of_day % 3600) / 60;
    let sec = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{sec:02} UTC")
}

#[derive(Debug, PartialEq, Eq)]
struct CertificateSummary {
    subject_cn: Option<String>,
    not_before: String,
    not_before_epoch: Option<u64>,
    not_after: String,
    not_after_epoch: Option<u64>,
    sans: Vec<String>,
}

pub(super) fn certificate_validity_epochs(der: &[u8]) -> Option<(u64, u64)> {
    let summary = parse_certificate_summary(der)?;
    Some((summary.not_before_epoch?, summary.not_after_epoch?))
}

fn read_der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let &tag = input.first()?;
    if (tag & 0x1f) == 0x1f {
        return None;
    }
    let &len_byte = input.get(1)?;
    let (len, header) = if len_byte < 0x80 {
        (usize::from(len_byte), 2usize)
    } else {
        let count = usize::from(len_byte & 0x7f);
        if count == 0 || count > 4 || input.len() < 2 + count {
            return None;
        }
        let mut len = 0usize;
        for &b in &input[2..2 + count] {
            len = (len << 8) | usize::from(b);
        }
        (len, 2 + count)
    };
    let end = header.checked_add(len)?;
    if input.len() < end {
        return None;
    }
    Some((tag, &input[header..end], &input[end..]))
}

fn parse_asn1_time(tag: u8, bytes: &[u8]) -> Option<(String, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (year, rest) = match tag {
        0x17 if text.len() == 13 && text.ends_with('Z') => {
            let yy: i32 = text[0..2].parse().ok()?;
            let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
            (year, &text[2..12])
        }
        0x18 if text.len() == 15 && text.ends_with('Z') => {
            let year: i32 = text[0..4].parse().ok()?;
            (year, &text[4..14])
        }
        _ => return None,
    };
    let month: u8 = rest[0..2].parse().ok()?;
    let day: u8 = rest[2..4].parse().ok()?;
    let hour: u8 = rest[4..6].parse().ok()?;
    let min: u8 = rest[6..8].parse().ok()?;
    let sec: u8 = rest[8..10].parse().ok()?;
    let epoch = epoch_from_utc(year, month, day, hour, min, sec)?;
    Some((
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC"),
        epoch,
    ))
}

fn parse_certificate_summary(der: &[u8]) -> Option<CertificateSummary> {
    let (tag, cert_seq, _) = read_der_tlv(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, mut tbs, _) = read_der_tlv(cert_seq)?;
    if tag != 0x30 {
        return None;
    }
    let (first_tag, first_val, rest) = read_der_tlv(tbs)?;
    tbs = if first_tag == 0xA0 {
        rest
    } else if first_tag == 0x02 {
        // v1 certificate without explicit version field: already consumed serialNumber
        let (_, _, after_sig) = read_der_tlv(rest)?;
        let (_, _, after_issuer) = read_der_tlv(after_sig)?;
        return parse_tbs_from_validity(after_issuer);
    } else {
        let _ = first_val;
        return None;
    };
    // serialNumber, signature, issuer
    let (tag, _, tbs) = read_der_tlv(tbs)?;
    if tag != 0x02 {
        return None;
    }
    let (tag, _, tbs) = read_der_tlv(tbs)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, _, tbs) = read_der_tlv(tbs)?;
    if tag != 0x30 {
        return None;
    }
    parse_tbs_from_validity(tbs)
}

fn parse_tbs_from_validity(tbs: &[u8]) -> Option<CertificateSummary> {
    // validity
    let (tag, validity_bytes, tbs) = read_der_tlv(tbs)?;
    if tag != 0x30 {
        return None;
    }
    let (nb_tag, nb_val, val_rest) = read_der_tlv(validity_bytes)?;
    let (na_tag, na_val, _) = read_der_tlv(val_rest)?;
    let (not_before, not_before_epoch) = parse_asn1_time(nb_tag, nb_val)?;
    let (not_after, not_after_epoch) = parse_asn1_time(na_tag, na_val)?;

    // subject
    let (tag, subject_bytes, tbs) = read_der_tlv(tbs)?;
    if tag != 0x30 {
        return None;
    }
    let mut subject_cn = None;
    let mut subj = subject_bytes;
    while !subj.is_empty() {
        let (rdn_tag, mut rdn, next_subj) = read_der_tlv(subj)?;
        subj = next_subj;
        if rdn_tag != 0x31 {
            continue;
        }
        while !rdn.is_empty() {
            let (atv_tag, atv, next_rdn) = read_der_tlv(rdn)?;
            rdn = next_rdn;
            if atv_tag != 0x30 {
                continue;
            }
            if let Some((oid_tag, oid_val, val_rest)) = read_der_tlv(atv) {
                if oid_tag == 0x06 && oid_val == [0x55, 0x04, 0x03] {
                    if let Some((_, cn_val, _)) = read_der_tlv(val_rest) {
                        if let Ok(cn) = std::str::from_utf8(cn_val) {
                            subject_cn = Some(cn.to_owned());
                        }
                    }
                }
            }
        }
    }

    // subjectPublicKeyInfo
    let (tag, _, mut rest) = read_der_tlv(tbs)?;
    if tag != 0x30 {
        return None;
    }

    // optional fields until extensions [3] (0xA3)
    let mut sans = Vec::new();
    while !rest.is_empty() {
        let (ext_tag, ext_val, next_rest) = read_der_tlv(rest)?;
        rest = next_rest;
        if ext_tag != 0xA3 {
            continue;
        }
        let (seq_tag, mut exts, _) = read_der_tlv(ext_val)?;
        if seq_tag != 0x30 {
            break;
        }
        while !exts.is_empty() {
            let (e_tag, e_val, next_exts) = read_der_tlv(exts)?;
            exts = next_exts;
            if e_tag != 0x30 {
                continue;
            }
            let Some((oid_tag, oid_val, e_rest)) = read_der_tlv(e_val) else {
                continue;
            };
            if oid_tag != 0x06 || oid_val != [0x55, 0x1d, 0x11] {
                continue;
            }
            let Some((next_tag, next_val, after_crit)) = read_der_tlv(e_rest) else {
                continue;
            };
            let octets = if next_tag == 0x01 {
                let Some((oct_tag, oct_val, _)) = read_der_tlv(after_crit) else {
                    continue;
                };
                if oct_tag != 0x04 {
                    continue;
                }
                oct_val
            } else if next_tag == 0x04 {
                next_val
            } else {
                continue;
            };
            let Some((san_seq_tag, mut san_seq, _)) = read_der_tlv(octets) else {
                continue;
            };
            if san_seq_tag != 0x30 {
                continue;
            }
            while !san_seq.is_empty() {
                let Some((gn_tag, gn_val, next_san)) = read_der_tlv(san_seq) else {
                    break;
                };
                san_seq = next_san;
                match gn_tag {
                    0x82 => {
                        if let Ok(dns) = std::str::from_utf8(gn_val) {
                            sans.push(dns.to_owned());
                        }
                    }
                    0x87 => {
                        if let Ok(bytes) = <[u8; 4]>::try_from(gn_val) {
                            sans.push(
                                std::net::IpAddr::V4(std::net::Ipv4Addr::from(bytes)).to_string(),
                            );
                        } else if let Ok(bytes) = <[u8; 16]>::try_from(gn_val) {
                            sans.push(
                                std::net::IpAddr::V6(std::net::Ipv6Addr::from(bytes)).to_string(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Some(CertificateSummary {
        subject_cn,
        not_before,
        not_before_epoch: Some(not_before_epoch),
        not_after,
        not_after_epoch: Some(not_after_epoch),
        sans,
    })
}

const CA_VALIDITY_MONTHS: u8 = 60;
const LEAF_VALIDITY_MONTHS: u8 = 3;
const LEAF_RENEW_BEFORE_SECS: u64 = 3 * 86_400;

fn build_ca_params(now_secs: u64) -> Result<rcgen::CertificateParams> {
    let mut params = rcgen::CertificateParams::new(Vec::new()).map_err(ServiceError::storage)?;
    let (y, m, d, secs_of_day) = utc_parts(now_secs);
    let (ey, em, ed) = add_months(y, m, d, CA_VALIDITY_MONTHS);
    params.not_before = rcgen::date_time_ymd(y, m, d) + std::time::Duration::from_secs(secs_of_day);
    params.not_after =
        rcgen::date_time_ymd(ey, em, ed) + std::time::Duration::from_secs(secs_of_day);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "anticrumb Root CA");
    Ok(params)
}

fn build_leaf_identity(
    names: &[String],
    ca_key: &rcgen::KeyPair,
    now_secs: u64,
) -> Result<(rcgen::Certificate, rcgen::KeyPair)> {
    let ca_issuer = build_ca_params(now_secs)?
        .self_signed(ca_key)
        .map_err(ServiceError::storage)?;
    let mut params =
        rcgen::CertificateParams::new(names.to_vec()).map_err(ServiceError::storage)?;
    let (y, m, d, secs_of_day) = utc_parts(now_secs);
    let (ey, em, ed) = add_months(y, m, d, LEAF_VALIDITY_MONTHS);
    params.not_before = rcgen::date_time_ymd(y, m, d) + std::time::Duration::from_secs(secs_of_day);
    params.not_after =
        rcgen::date_time_ymd(ey, em, ed) + std::time::Duration::from_secs(secs_of_day);
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "anticrumb");
    let leaf_key =
        rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(ServiceError::storage)?;
    let leaf_cert = params
        .signed_by(&leaf_key, &ca_issuer, ca_key)
        .map_err(ServiceError::storage)?;
    Ok((leaf_cert, leaf_key))
}

fn install_generated_identity(
    directory: &Path,
    names: &[String],
    reuse_ca: Option<(Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (ca_cert_pem, ca_key_pem, ca_key) = match reuse_ca {
        Some((ca_cert_pem, ca_key_pem)) => {
            let pem_str = std::str::from_utf8(&ca_key_pem).map_err(ServiceError::storage)?;
            let ca_key = rcgen::KeyPair::from_pem_and_sign_algo(pem_str, &rcgen::PKCS_ED25519)
                .map_err(ServiceError::storage)?;
            (ca_cert_pem, ca_key_pem, ca_key)
        }
        None => {
            let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
                .map_err(ServiceError::storage)?;
            let ca_cert = build_ca_params(now_secs)?
                .self_signed(&ca_key)
                .map_err(ServiceError::storage)?;
            (
                ca_cert.pem().into_bytes(),
                ca_key.serialize_pem().into_bytes(),
                ca_key,
            )
        }
    };
    let (leaf_cert, leaf_key) = build_leaf_identity(names, &ca_key, now_secs)?;
    let parent = directory
        .parent()
        .ok_or_else(|| ServiceError::invalid("TLS identity directory has no parent"))?;
    fs::create_dir_all(parent).map_err(ServiceError::storage)?;
    let staged = parent.join(format!(".tls-new-{}", uuid::Uuid::new_v4().simple()));
    let previous = parent.join(format!(".tls-old-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&staged).map_err(ServiceError::storage)?;
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o700))
                .map_err(ServiceError::storage)?;
        }
        private_write(&staged.join("ca-private-key.pem"), &ca_key_pem)?;
        private_write(&staged.join("ca-certificate.pem"), &ca_cert_pem)?;
        private_write(
            &staged.join("private-key.pem"),
            leaf_key.serialize_pem().as_bytes(),
        )?;
        private_write(&staged.join("certificate.pem"), leaf_cert.pem().as_bytes())?;
        private_write(
            &staged.join("names.json"),
            &serde_json::to_vec(names).map_err(ServiceError::storage)?,
        )?;
        // The server owns the state lease while replacing the complete identity.
        // Stage all files before touching the old key/certificate pair.
        let replacing = directory.exists();
        if replacing {
            fs::rename(directory, &previous).map_err(ServiceError::storage)?;
        }
        if let Err(error) = fs::rename(&staged, directory) {
            if replacing {
                fs::rename(&previous, directory).map_err(ServiceError::storage)?;
            }
            return Err(ServiceError::storage(error));
        }
        if replacing {
            fs::remove_dir_all(&previous).map_err(ServiceError::storage)?;
        }
        Ok(())
    })();
    if staged.exists() {
        let _ = fs::remove_dir_all(staged);
    }
    result
}

async fn auto_renew_leaf(options: Options, tls: TlsIdentity, host: std::sync::Arc<Host>) {
    let cert_path = options.root.join("tls/certificate.pem");
    loop {
        let wait = match fs::read(&cert_path)
            .ok()
            .and_then(|pem| certificate_der(&pem).ok())
            .and_then(|der| parse_certificate_summary(&der))
            .and_then(|summary| summary.not_after_epoch)
        {
            Some(not_after) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let renew_at = not_after.saturating_sub(LEAF_RENEW_BEFORE_SECS);
                if now >= renew_at {
                    std::time::Duration::ZERO
                } else {
                    std::time::Duration::from_secs((renew_at - now).min(3600))
                }
            }
            None => std::time::Duration::from_secs(3600),
        };
        if !wait.is_zero() {
            tokio::select! {
                _ = host.service.shutdown.notified() => return,
                _ = tokio::time::sleep(wait) => continue,
            }
        }
        match options.rotate_leaf(&tls) {
            Ok(()) => {
                if let Ok(pem) = fs::read(&cert_path) {
                    if let Ok(der) = certificate_der(&pem) {
                        if let Some(summary) = parse_certificate_summary(&der) {
                            eprintln!(
                                "{} TLS leaf certificate auto-renewed: valid {} .. {}",
                                server::log_timestamp(),
                                summary.not_before,
                                summary.not_after
                            );
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "{} Warning: TLS leaf certificate auto-renewal failed: {}",
                    server::log_timestamp(),
                    error.message
                );
                tokio::select! {
                    _ = host.service.shutdown.notified() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
                }
            }
        }
    }
}

/// SHA-256 of the first certificate's DER, independent of PEM whitespace.
pub fn certificate_fingerprint(pem: &[u8]) -> Result<String> {
    Ok(tls::certificate_fingerprint(&certificate_der(pem)?))
}

fn certificate_der(pem: &[u8]) -> Result<Vec<u8>> {
    let certificate = rustls_pemfile::certs(&mut &pem[..])
        .next()
        .transpose()
        .map_err(|_| ServiceError::invalid("invalid TLS certificate PEM"))?
        .ok_or_else(|| ServiceError::invalid("TLS certificate PEM contains no certificate"))?;
    Ok(certificate.to_vec())
}

#[cfg(any(windows, test))]
fn normalize_windows_exe_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    let stripped = trimmed
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| trimmed.strip_prefix(r"\\?\").map(str::to_owned))
        .unwrap_or_else(|| trimmed.to_owned());
    stripped.replace('/', r"\")
}

#[cfg(any(windows, test))]
fn detected_vpn_cidr(ips: &[std::net::IpAddr]) -> &'static str {
    for ip in ips {
        if let std::net::IpAddr::V4(v4) = ip {
            let [a, b, _, _] = v4.octets();
            if a == 100 && (b & 0xc0) == 64 {
                return "100.64.0.0/10";
            }
        }
    }
    for ip in ips {
        if let std::net::IpAddr::V4(v4) = ip {
            let [a, b, _, _] = v4.octets();
            if a == 10 {
                return "10.0.0.0/8";
            }
            if a == 172 && (b & 0xf0) == 16 {
                return "172.16.0.0/12";
            }
        }
    }
    "100.64.0.0/10"
}

#[cfg(any(windows, test))]
fn evaluate_windows_firewall_rules(
    reg_output: &str,
    exe_path: &str,
    port: u16,
    vpn_cidr: &str,
) -> Option<String> {
    let exe_norm = normalize_windows_exe_path(exe_path);
    if exe_norm.is_empty() {
        return None;
    }
    let port_str = port.to_string();
    let mut blocked_profiles = std::collections::BTreeSet::new();
    let mut allowed_profiles = std::collections::BTreeSet::new();

    for line in reg_output.lines() {
        let Some(pos) = line.find("v2.") else {
            continue;
        };
        let rule = &line[pos..];
        let mut active = false;
        let mut dir_in = false;
        let mut action = None;
        let mut proto_ok = true;
        let mut app_matches = false;
        let mut port_ok = true;
        let mut profiles = Vec::new();

        for part in rule.split('|') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("Active") {
                active = v.eq_ignore_ascii_case("TRUE");
            } else if k.eq_ignore_ascii_case("Dir") {
                dir_in = v.eq_ignore_ascii_case("In");
            } else if k.eq_ignore_ascii_case("Action") {
                if v.eq_ignore_ascii_case("Block") {
                    action = Some("Block");
                } else if v.eq_ignore_ascii_case("Allow") {
                    action = Some("Allow");
                }
            } else if k.eq_ignore_ascii_case("Protocol") {
                proto_ok =
                    v == "6" || v.eq_ignore_ascii_case("TCP") || v.eq_ignore_ascii_case("Any");
            } else if k.eq_ignore_ascii_case("App") {
                let candidate = normalize_windows_exe_path(v);
                app_matches = candidate.eq_ignore_ascii_case(&exe_norm);
            } else if k.eq_ignore_ascii_case("LPort") {
                port_ok =
                    v.eq_ignore_ascii_case("Any") || v.split(',').any(|p| p.trim() == port_str);
            } else if k.eq_ignore_ascii_case("Profile") {
                for prof in v.split(',') {
                    let prof = prof.trim();
                    if prof.eq_ignore_ascii_case("Domain") {
                        profiles.push("Domain");
                    } else if prof.eq_ignore_ascii_case("Private") {
                        profiles.push("Private");
                    } else if prof.eq_ignore_ascii_case("Public") {
                        profiles.push("Public");
                    }
                }
            }
        }

        if !active || !dir_in || !proto_ok || !app_matches || !port_ok {
            continue;
        }
        let Some(action) = action else {
            continue;
        };
        if profiles.is_empty() {
            profiles.extend(["Domain", "Private", "Public"]);
        }
        for prof in profiles {
            if action == "Block" {
                blocked_profiles.insert(prof);
            } else if action == "Allow" {
                allowed_profiles.insert(prof);
            }
        }
    }

    let ps_escaped = exe_norm.replace('\'', "''");
    if !blocked_profiles.is_empty() {
        let effective_allowed: Vec<&str> = allowed_profiles
            .difference(&blocked_profiles)
            .copied()
            .collect();
        if effective_allowed.is_empty() {
            return Some(format!(
                "Warning: Windows Firewall has an active inbound BLOCK rule for {exe_norm} (likely created when the firewall prompt was cancelled).\n\
                 Remote connections will time out. To allow inbound access in Administrator PowerShell:\n  \
                 Get-NetFirewallApplicationFilter -Program '{ps_escaped}' -ErrorAction SilentlyContinue | Get-NetFirewallRule | Remove-NetFirewallRule\n  \
                 # Allow VPN/virtual private subnet only ({vpn_cidr}), or omit -LocalAddress/-RemoteAddress for all networks:\n  \
                 New-NetFirewallRule -DisplayName 'anticrumb server (VPN)' -Direction Inbound -Program '{ps_escaped}' -LocalAddress {vpn_cidr} -RemoteAddress {vpn_cidr} -Action Allow -Profile Any"
            ));
        }
        let blocked_list = blocked_profiles.into_iter().collect::<Vec<_>>().join(", ");
        let allowed_list = effective_allowed.join(", ");
        return Some(format!(
            "Warning: Windows Firewall blocks inbound connections for {exe_norm} on profile(s): {blocked_list} (allowed only on: {allowed_list}).\n\
             Connections over VPN or public networks may time out. To allow inbound access in Administrator PowerShell:\n  \
             Get-NetFirewallApplicationFilter -Program '{ps_escaped}' -ErrorAction SilentlyContinue | Get-NetFirewallRule | Remove-NetFirewallRule\n  \
             # Allow VPN/virtual private subnet only ({vpn_cidr}), or omit -LocalAddress/-RemoteAddress for all networks:\n  \
             New-NetFirewallRule -DisplayName 'anticrumb server (VPN)' -Direction Inbound -Program '{ps_escaped}' -LocalAddress {vpn_cidr} -RemoteAddress {vpn_cidr} -Action Allow -Profile Any"
        ));
    }

    if !allowed_profiles.is_empty() && !allowed_profiles.contains("Public") {
        let allowed_list = allowed_profiles.into_iter().collect::<Vec<_>>().join(", ");
        return Some(format!(
            "Warning: Windows Firewall only allows inbound connections for {exe_norm} on profile(s): {allowed_list} (Public profile is not allowed).\n\
             Connections over VPN or public networks may time out. To allow VPN/virtual private subnet ({vpn_cidr}) in Administrator PowerShell:\n  \
             Get-NetFirewallApplicationFilter -Program '{ps_escaped}' -ErrorAction SilentlyContinue | Get-NetFirewallRule | Remove-NetFirewallRule\n  \
             # Allow VPN/virtual private subnet only ({vpn_cidr}), or omit -LocalAddress/-RemoteAddress for all networks:\n  \
             New-NetFirewallRule -DisplayName 'anticrumb server (VPN)' -Direction Inbound -Program '{ps_escaped}' -LocalAddress {vpn_cidr} -RemoteAddress {vpn_cidr} -Action Allow -Profile Any"
        ));
    }

    None
}

#[cfg(any(target_os = "macos", test))]
fn evaluate_macos_firewall_status(
    global_state_output: &str,
    app_blocked_output: &str,
    exe_path: &str,
) -> Option<String> {
    let global_lower = global_state_output.to_ascii_lowercase();
    let enabled = global_lower.contains("enabled")
        || global_lower.contains("state = 1")
        || global_lower.contains("state = 2");
    if !enabled {
        return None;
    }
    let blocked_lower = app_blocked_output.to_ascii_lowercase();
    if blocked_lower.contains("is blocked") || blocked_lower.contains("block incoming") {
        let sh_escaped = exe_path.replace('\'', r"'\''");
        return Some(format!(
            "Warning: macOS Application Firewall is blocking incoming connections for {exe_path}.\n\
             Remote connections will time out. Allow it in System Settings > Network > Firewall, or run:\n  \
             sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp '{sh_escaped}'"
        ));
    }
    None
}

fn check_inbound_firewall_warning(address: &std::net::SocketAddr) -> Option<String> {
    if address.ip().is_loopback() {
        return None;
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().ok()?;
        let exe_norm = normalize_windows_exe_path(&exe.to_string_lossy());
        let output = std::process::Command::new("reg.exe")
            .args([
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules",
                "/f",
                &exe_norm,
                "/d",
            ])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let vpn_cidr = detected_vpn_cidr(&local_interface_ips());
        evaluate_windows_firewall_rules(&stdout, &exe_norm, address.port(), vpn_cidr)
    }
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().ok()?;
        let exe_str = exe.to_string_lossy();
        let fw = "/usr/libexec/ApplicationFirewall/socketfilterfw";
        let global = std::process::Command::new(fw)
            .arg("--getglobalstate")
            .output()
            .ok()?;
        let blocked = std::process::Command::new(fw)
            .args(["--getappblocked", exe_str.as_ref()])
            .output()
            .ok()?;
        evaluate_macos_firewall_status(
            &String::from_utf8_lossy(&global.stdout),
            &String::from_utf8_lossy(&blocked.stdout),
            exe_str.as_ref(),
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

pub fn run(args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        println!("Expose a browser server to another machine or API client:\n  anticrumb server\n  anticrumb server :50051\n  anticrumb server 192.0.2.10:50051\n\nTLS and a bearer token are created automatically under ~/.anticrumb/server/.\nWithout an address, binds all IPv4 interfaces on a random port (0.0.0.0:0) and\nincludes the hostname and local interface IPs in the certificate SANs.\nCopy the printed connect command to the other machine and compare fingerprints.\nThe HTTPS port serves browser RPC and provider APIs (/v1 and /v1beta).\nWorkspace files and shell tools always execute on the TUI machine.\n\nOptions:\n  --workers N                        TempChat tabs per account (default 2)\n  --pace LEVEL                       fast, low (default), high\n  --token STRING                     remote token override (or ANTICRUMB_TOKEN)\n  --regenerate-token                 replace the generated connection token\n  --state-dir PATH                   remote browser server state directory\n  --tls-san NAME                     add a hostname/IP to the certificate\n  --tls-self-signed                  generate/reuse a self-signed identity\n  --regenerate-tls                   replace the certificate\n  --tls-cert PATH --tls-key PATH     use a supplied PEM identity\n\nTo run a local-only IPC daemon without opening a network port, use `anticrumb daemon`.");
        return Ok(());
    }
    let options = Options::parse(args)?;
    if let Some(workers) = options.workers {
        crate::tab_pool::set_worker_tabs(None, workers);
    }
    if let Some(pace) = options.pace {
        crate::gemini::set_human_pace(None, pace);
    }
    tokio::runtime::Runtime::new()
        .map_err(ServiceError::storage)?
        .block_on(async move {
            let host = Host::browser(&options.root)?;
            if let Some(workers) = options.workers {
                crate::tab_pool::set_worker_tabs(Some(&host.service.store), workers);
            }
            if let Some(pace) = options.pace {
                crate::gemini::set_human_pace(Some(&host.service.store), pace);
            }
            let listener = tokio::net::TcpListener::bind(&options.listen)
                .await
                .map_err(|error| {
                    ServiceError::new(
                        crate::domain::ErrorCode::TransportDisconnected,
                        "server_listen",
                        format!(
                            "cannot bind browser server at {}: {error}; check for an existing service on this port, stop the conflicting instance or choose another address with `anticrumb server IP:PORT`",
                            options.listen
                        ),
                    )
                })?;
            let tls = options.identity()?.ok_or_else(|| {
                ServiceError::invalid("network server requires a TLS identity")
            })?;
            // Validate the key/certificate pair before generating or displaying
            // a token. The serving path uses this exact in-memory identity.
            let _ = tls.acceptor()?;
            let certificate = options
                .tls_cert
                .clone()
                .unwrap_or_else(|| options.root.join("tls/certificate.pem"));
            let der = certificate_der(&fs::read(certificate).map_err(ServiceError::storage)?)?;
            let anchor_path = options
                .tls_cert
                .clone()
                .unwrap_or_else(|| options.root.join("tls/ca-certificate.pem"));
            let anchor_der =
                certificate_der(&fs::read(anchor_path).map_err(ServiceError::storage)?)?;
            let token = options.network_token()?;
            let address = listener.local_addr().map_err(ServiceError::storage)?;
            eprintln!(
                "{} {}",
                server::log_timestamp(),
                crate::build_info::help_banner()
            );
            eprintln!("anticrumb server: https://{address}");
            eprintln!("Provider API: https://{address}/v1 (same TLS certificate and token)");
            let fingerprint = tls::certificate_fingerprint(&anchor_der);
            eprintln!(
                "TLS certificate mode: {}",
                if options.tls_cert.is_some() {
                    "supplied PEM"
                } else {
                    "self-signed"
                }
            );
            if let Some(summary) = parse_certificate_summary(&der) {
                if let Some(cn) = &summary.subject_cn {
                    eprintln!("TLS certificate subject: CN={cn}");
                }
                eprintln!(
                    "TLS certificate validity: {} .. {}",
                    summary.not_before, summary.not_after
                );
                if !summary.sans.is_empty() {
                    eprintln!("TLS certificate SANs: {}", summary.sans.join(", "));
                }
                if options.tls_cert.is_some() {
                    if let Some(not_after_epoch) = summary.not_after_epoch {
                        let now_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        if now_secs >= not_after_epoch {
                            eprintln!(
                                "Warning: supplied TLS certificate expired at {}.",
                                summary.not_after
                            );
                        }
                    }
                }
            }
            eprintln!("TLS certificate SHA256: {fingerprint}");
            eprintln!(
                "TLS certificate emoji: {}",
                super::fingerprint::emoji_fingerprint(&anchor_der)
            );
            if options.token.is_none() {
                eprintln!("Connection token: {token}");
            }
            if let Some(warning) = check_inbound_firewall_warning(&address) {
                eprintln!("{warning}");
            }
            let connect_address = if address.ip().is_unspecified() {
                format!("HOST:{}", address.port())
            } else {
                address.to_string()
            };
            // Generated tokens are hexadecimal and safe to copy across shells.
            // Explicit credentials must never be echoed, even in usage examples.
            let connect_token = if options.token.is_none()
                && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                &token
            } else {
                "YOUR_TOKEN"
            };
            eprintln!(
                "Connect: anticrumb connect 'https://{connect_address}' --token '{connect_token}'"
            );
            if address.ip().is_unspecified() {
                eprintln!("Replace HOST with a hostname or IP from TLS certificate SANs (or add another via --tls-san).");
            }
            if options.tls_cert.is_none() {
                tokio::spawn(auto_renew_leaf(options, tls.clone(), host.clone()));
            }
            tokio::try_join!(
                server::serve(
                    host.clone(),
                    Some(token),
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    Some(tls),
                ),
                super::local::run(false, Some(host))
            )?;
            Ok(())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::digest;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn parse(args: &[&str], inherited: Option<&str>) -> Result<Options> {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        Options::parse_with_token(&args, || inherited.map(str::to_owned))
    }

    #[test]
    fn network_defaults_to_tls_and_accepts_a_string_token_override() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("not-created-by-parsing");
        let options = parse(
            &["127.0.0.1:0", "--state-dir", state.to_str().unwrap()],
            None,
        )
        .unwrap();
        assert!(options.self_signed);
        assert!(options.token.is_none());
        assert!(!state.exists());
        let options = parse(&["127.0.0.1:0"], Some(TOKEN)).unwrap();
        assert!(options.self_signed);
        assert_eq!(options.network_token().unwrap(), TOKEN);
        let options = parse(
            &["127.0.0.1:0", "--token", TOKEN],
            Some("invalid inherited token"),
        )
        .unwrap();
        assert_eq!(options.network_token().unwrap(), TOKEN);
        let error = parse(&["--token", "private-short-token"], None)
            .err()
            .unwrap();
        assert!(!error.message.contains("private-short-token"));
        let error = parse(&["--token=private-inline-token"], None)
            .err()
            .unwrap();
        assert!(!error.message.contains("private-inline-token"));
        assert!(parse(&["--token-file", "removed-input"], None).is_err());
    }

    #[test]
    fn removed_ipc_mode_is_rejected() {
        assert!(parse(&["--ipc"], None).is_err());
    }

    #[test]
    fn plaintext_is_unavailable_and_regeneration_requires_generated_tls() {
        for args in [
            vec!["--insecure-http"],
            vec!["127.0.0.1:50051", "--insecure-http"],
            vec!["[::1]:50051", "--insecure-http"],
            vec!["0.0.0.0:50051", "--insecure-http"],
            vec!["--insecure-http", "--tls-self-signed"],
            vec!["--insecure-http", "--regenerate-tls"],
            vec!["--tls-cert", "cert", "--tls-key", "key", "--regenerate-tls"],
        ] {
            assert!(parse(&args, Some(TOKEN)).is_err());
        }
        let options = parse(&["127.0.0.1:0", "--regenerate-tls"], Some(TOKEN)).unwrap();
        assert!(options.self_signed && options.regenerate_tls);
    }

    #[test]
    fn generated_token_is_persistent_and_overrides_do_not_replace_it() {
        let root = tempfile::tempdir().unwrap();
        let mut options = Options {
            root: root.path().into(),
            ..Default::default()
        };
        let generated = options.network_token().unwrap();
        assert_eq!(generated.len(), 64);
        assert!(generated.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(options.network_token().unwrap() == generated);
        options.token = Some(TOKEN.into());
        assert_eq!(options.network_token().unwrap(), TOKEN);
        assert!(fs::read_to_string(root.path().join("token")).unwrap() == generated);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("token"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn certificate_fingerprint_hashes_der_and_ignores_pem_whitespace() {
        let params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();
        let expected = digest(cert.der().as_ref());
        assert_eq!(certificate_fingerprint(pem.as_bytes()).unwrap(), expected);
        assert_eq!(
            certificate_fingerprint(format!("\n{}\n", pem.replace('\n', "\r\n")).as_bytes())
                .unwrap(),
            expected
        );
        assert!(certificate_fingerprint(b"not a certificate").is_err());
    }

    #[test]
    fn generated_identity_is_reused_until_explicit_regeneration() {
        let root = tempfile::tempdir().unwrap();
        let mut options = Options {
            root: root.path().into(),
            listen: "127.0.0.1:50051".into(),
            token: Some(TOKEN.into()),
            ..Default::default()
        };
        options.identity().unwrap().unwrap();
        let ca_cert_path = root.path().join("tls/ca-certificate.pem");
        let ca_key_path = root.path().join("tls/ca-private-key.pem");
        let cert_path = root.path().join("tls/certificate.pem");
        let key_path = root.path().join("tls/private-key.pem");
        let first_ca_cert = fs::read(&ca_cert_path).unwrap();
        let first_ca_key = fs::read(&ca_key_path).unwrap();
        let first_ca_pin = certificate_fingerprint(&first_ca_cert).unwrap();
        let first_cert = fs::read(&cert_path).unwrap();
        let first_key = fs::read(&key_path).unwrap();
        let first_leaf_pin = certificate_fingerprint(&first_cert).unwrap();
        options.identity().unwrap().unwrap();
        assert!(fs::read(&ca_cert_path).unwrap() == first_ca_cert);
        assert!(fs::read(&ca_key_path).unwrap() == first_ca_key);
        assert!(fs::read(&cert_path).unwrap() == first_cert);
        assert!(fs::read(&key_path).unwrap() == first_key);

        // Adding a SAN automatically re-issues the leaf certificate signed by
        // the existing Root CA so client CA pins remain valid.
        options.names.push("browser.example".into());
        options.identity().unwrap().unwrap();
        assert!(fs::read(&ca_cert_path).unwrap() == first_ca_cert);
        assert!(fs::read(&ca_key_path).unwrap() == first_ca_key);
        assert_ne!(
            certificate_fingerprint(&fs::read(&cert_path).unwrap()).unwrap(),
            first_leaf_pin
        );
        assert!(fs::read(&key_path).unwrap() != first_key);

        // Missing or corrupted CA files require explicit --regenerate-tls instead of silent CA replacement.
        fs::remove_file(&ca_cert_path).unwrap();
        let err = options.identity().err().unwrap();
        assert!(err.message.contains("--regenerate-tls"));

        // Explicit --regenerate-tls replaces the Root CA and leaf identity.
        options.regenerate_tls = true;
        options.identity().unwrap().unwrap();
        assert_ne!(
            certificate_fingerprint(&fs::read(&ca_cert_path).unwrap()).unwrap(),
            first_ca_pin
        );
        assert!(fs::read(&ca_key_path).unwrap() != first_ca_key);
        options.regenerate_tls = false;
        options.identity().unwrap().unwrap();
        let renewed_ca_cert = fs::read(&ca_cert_path).unwrap();
        let renewed_cert = fs::read(&cert_path).unwrap();
        let renewed_key = fs::read(&key_path).unwrap();
        options.regenerate_tls = true;
        options.names.push("non-ascii-☃".into());
        assert!(options.identity().is_err());
        assert!(fs::read(&ca_cert_path).unwrap() == renewed_ca_cert);
        assert!(fs::read(&cert_path).unwrap() == renewed_cert);
        assert!(fs::read(&key_path).unwrap() == renewed_key);
        assert!(!root.path().join("token").exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(ca_key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn default_listen_and_port_shorthand_default_to_unspecified_address_and_auto_sans() {
        let bare = parse(&[], None).unwrap();
        assert_eq!(bare.listen, "0.0.0.0:0");
        assert!(bare.self_signed);

        let before_flag = parse(&["--workers", "3", "--regenerate-token"], None).unwrap();
        assert_eq!(before_flag.listen, "0.0.0.0:0");
        assert_eq!(before_flag.workers, Some(3));
        assert!(before_flag.regenerate_token);

        let port_only = parse(&[":50051"], None).unwrap();
        assert_eq!(port_only.listen, "0.0.0.0:50051");

        assert!(parse(&["not-an-address"], None).is_err());
        assert!(parse(&["127.0.0.1:50051", "127.0.0.1:50052"], None).is_err());

        let root = tempfile::tempdir().unwrap();
        let mut options = Options {
            root: root.path().into(),
            listen: "0.0.0.0:0".into(),
            ..Default::default()
        };
        options.identity().unwrap().unwrap();
        let initial_ca = fs::read(root.path().join("tls/ca-certificate.pem")).unwrap();
        let sans: Vec<String> =
            serde_json::from_slice(&fs::read(root.path().join("tls/names.json")).unwrap()).unwrap();
        assert!(sans.contains(&"localhost".to_string()));
        assert!(sans.contains(&"127.0.0.1".to_string()));
        // IPv4 unspecified binding must not advertise unreachable IPv6 addresses.
        assert!(!sans.iter().any(|s| s.parse::<std::net::Ipv6Addr>().is_ok()));
        if let Some(host) = valid_dns_hostname() {
            assert!(sans.contains(&host));
        }

        // Dynamic interface IP changes do not break certificate reuse when baseline SANs match.
        let mut simulated_previous = sans.clone();
        simulated_previous.push("198.51.100.42".into());
        simulated_previous.sort();
        fs::write(
            root.path().join("tls/names.json"),
            serde_json::to_vec(&simulated_previous).unwrap(),
        )
        .unwrap();
        let leaf_before = fs::read(root.path().join("tls/certificate.pem")).unwrap();
        options.identity().unwrap().unwrap();
        assert_eq!(
            fs::read(root.path().join("tls/certificate.pem")).unwrap(),
            leaf_before
        );

        // Explicit --tls-san additions automatically re-issue the leaf while preserving the CA.
        options.names.push("new-explicit.example".into());
        options.identity().unwrap().unwrap();
        assert_eq!(
            fs::read(root.path().join("tls/ca-certificate.pem")).unwrap(),
            initial_ca
        );
        let updated_sans: Vec<String> =
            serde_json::from_slice(&fs::read(root.path().join("tls/names.json")).unwrap()).unwrap();
        assert!(updated_sans.contains(&"new-explicit.example".to_string()));
    }

    #[test]
    fn generated_certificate_has_three_month_validity_and_summary_metadata() {
        let root = tempfile::tempdir().unwrap();
        let options = Options {
            root: root.path().into(),
            listen: "127.0.0.1:50051".into(),
            names: vec!["daemon.example".into()],
            ..Default::default()
        };
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        options.identity().unwrap().unwrap();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let ca_der =
            certificate_der(&fs::read(root.path().join("tls/ca-certificate.pem")).unwrap())
                .unwrap();
        let ca_summary = parse_certificate_summary(&ca_der).unwrap();
        assert_eq!(ca_summary.subject_cn.as_deref(), Some("anticrumb Root CA"));
        // Ed25519 OID (1.3.101.112 -> 06 03 2B 65 70) must be used for both CA and leaf.
        let ed25519_oid = [0x06, 0x03, 0x2b, 0x65, 0x70];
        assert!(ca_der.windows(ed25519_oid.len()).any(|w| w == ed25519_oid));

        let der =
            certificate_der(&fs::read(root.path().join("tls/certificate.pem")).unwrap()).unwrap();
        assert!(der.windows(ed25519_oid.len()).any(|w| w == ed25519_oid));
        let summary = parse_certificate_summary(&der).unwrap();
        assert_eq!(summary.subject_cn.as_deref(), Some("anticrumb"));
        assert!(summary.sans.contains(&"localhost".to_string()));
        assert!(summary.sans.contains(&"127.0.0.1".to_string()));
        assert!(summary.sans.contains(&"::1".to_string()));
        assert!(summary.sans.contains(&"daemon.example".to_string()));

        let (y, m, d, secs_of_day) = utc_parts(before);
        let (ey, em, ed) = add_months(y, m, d, 3);
        let expected_after_min = epoch_from_utc(
            ey,
            em,
            ed,
            (secs_of_day / 3600) as u8,
            ((secs_of_day % 3600) / 60) as u8,
            (secs_of_day % 60) as u8,
        )
        .unwrap();
        let (y2, m2, d2, secs2) = utc_parts(after);
        let (ey2, em2, ed2) = add_months(y2, m2, d2, 3);
        let expected_after_max = epoch_from_utc(
            ey2,
            em2,
            ed2,
            (secs2 / 3600) as u8,
            ((secs2 % 3600) / 60) as u8,
            (secs2 % 60) as u8,
        )
        .unwrap();
        let actual_after = summary.not_after_epoch.unwrap();
        assert!(actual_after >= expected_after_min && actual_after <= expected_after_max);
        assert!(summary.not_before.ends_with(" UTC"));
        assert!(summary.not_after.ends_with(" UTC"));
        assert_eq!(format_utc(actual_after), summary.not_after);

        // If the leaf is within the 3-day renewal window, identity() automatically
        // re-signs a fresh leaf with the existing CA without changing the CA pin.
        let ca_pem_before = fs::read(root.path().join("tls/ca-certificate.pem")).unwrap();
        let ca_key_pem = fs::read_to_string(root.path().join("tls/ca-private-key.pem")).unwrap();
        let ca_key =
            rcgen::KeyPair::from_pem_and_sign_algo(&ca_key_pem, &rcgen::PKCS_ED25519).unwrap();
        let ca_issuer = build_ca_params(before)
            .unwrap()
            .self_signed(&ca_key)
            .unwrap();
        let (_, full_names) = options.desired_names().unwrap();
        let mut expiring_params = rcgen::CertificateParams::new(full_names).unwrap();
        let soon = before + 2 * 86_400;
        let (sy, sm, sd, ssecs) = utc_parts(soon);
        expiring_params.not_before =
            rcgen::date_time_ymd(y, m, d) + std::time::Duration::from_secs(secs_of_day);
        expiring_params.not_after =
            rcgen::date_time_ymd(sy, sm, sd) + std::time::Duration::from_secs(ssecs);
        let expiring_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let expiring_leaf = expiring_params
            .signed_by(&expiring_key, &ca_issuer, &ca_key)
            .unwrap();
        fs::write(root.path().join("tls/certificate.pem"), expiring_leaf.pem()).unwrap();
        fs::write(
            root.path().join("tls/private-key.pem"),
            expiring_key.serialize_pem(),
        )
        .unwrap();
        options.identity().unwrap().unwrap();
        assert_eq!(
            fs::read(root.path().join("tls/ca-certificate.pem")).unwrap(),
            ca_pem_before
        );
        let renewed_der =
            certificate_der(&fs::read(root.path().join("tls/certificate.pem")).unwrap()).unwrap();
        let renewed_summary = parse_certificate_summary(&renewed_der).unwrap();
        assert!(renewed_summary.not_after_epoch.unwrap() >= expected_after_min);
    }

    #[test]
    fn regenerate_token_replaces_persisted_token_and_rejects_explicit_overrides() {
        assert!(parse(&["--token", TOKEN, "--regenerate-token"], None).is_err());
        assert!(parse(&["--regenerate-token"], Some(TOKEN)).is_err());

        let root = tempfile::tempdir().unwrap();
        let initial = Options {
            root: root.path().into(),
            listen: "127.0.0.1:0".into(),
            ..Default::default()
        };
        let first = initial.network_token().unwrap();
        assert_eq!(initial.network_token().unwrap(), first);

        let rotate = Options {
            root: root.path().into(),
            listen: "127.0.0.1:0".into(),
            regenerate_token: true,
            ..Default::default()
        };
        let second = rotate.network_token().unwrap();
        assert_ne!(second, first);
        assert_eq!(second.len(), 64);
        assert!(second.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            fs::read_to_string(root.path().join("token")).unwrap(),
            second
        );
        assert_eq!(initial.network_token().unwrap(), second);
    }

    #[test]
    fn windows_firewall_parser_detects_cancelled_prompt_block_and_partial_profile_rules() {
        let exe = r"C:\Users\user\.local\bin\anticrumb.exe";
        let vpn_ips = vec![
            "192.168.1.25".parse().unwrap(),
            "100.121.6.125".parse().unwrap(),
        ];
        let cidr = detected_vpn_cidr(&vpn_ips);
        assert_eq!(cidr, "100.64.0.0/10");
        assert_eq!(
            detected_vpn_cidr(&["10.8.0.2".parse().unwrap()]),
            "10.0.0.0/8"
        );
        assert_eq!(
            detected_vpn_cidr(&["172.20.1.5".parse().unwrap()]),
            "172.16.0.0/12"
        );

        // 1. Cancelled Windows Firewall prompt creates explicit TCP/UDP Block rules.
        let cancelled_output = r#"
HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules
    TCP Query User{1234}C:\users\user\.local\bin\anticrumb.exe    REG_SZ    v2.30|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Public|Profile=Private|App=C:\users\user\.local\bin\anticrumb.exe|Name=anticrumb.exe|
    UDP Query User{1234}C:\users\user\.local\bin\anticrumb.exe    REG_SZ    v2.30|Action=Block|Active=TRUE|Dir=In|Protocol=17|Profile=Public|Profile=Private|App=C:\users\user\.local\bin\anticrumb.exe|Name=anticrumb.exe|
"#;
        let warn = evaluate_windows_firewall_rules(cancelled_output, exe, 50303, cidr).unwrap();
        assert!(warn.contains("active inbound BLOCK rule"), "{warn}");
        assert!(warn.contains("Remove-NetFirewallRule"), "{warn}");
        assert!(
            warn.contains("-LocalAddress 100.64.0.0/10 -RemoteAddress 100.64.0.0/10"),
            "{warn}"
        );
        assert!(!warn.to_ascii_lowercase().contains("tailscale"), "{warn}");

        // 2. Allowed on Private but Blocked on Public (e.g. Public checkbox left unchecked).
        let partial_block_output = r#"
    {A1}    REG_SZ    v2.30|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App=C:\Users\user\.local\bin\anticrumb.exe|Name=anticrumb|
    {A2}    REG_SZ    v2.30|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Public|App=C:\Users\user\.local\bin\anticrumb.exe|Name=anticrumb|
"#;
        let warn = evaluate_windows_firewall_rules(partial_block_output, exe, 50303, cidr).unwrap();
        assert!(warn.contains("profile(s): Public"), "{warn}");
        assert!(warn.contains("allowed only on: Private"), "{warn}");
        assert!(!warn.to_ascii_lowercase().contains("tailscale"), "{warn}");

        // 3. Allowed only on Private (no explicit Public rule -> Public not allowed).
        let private_only_output = r#"
    {A1}    REG_SZ    v2.30|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App=C:\Users\user\.local\bin\anticrumb.exe|Name=anticrumb|
"#;
        let warn = evaluate_windows_firewall_rules(private_only_output, exe, 50303, cidr).unwrap();
        assert!(warn.contains("Public profile is not allowed"), "{warn}");
        assert!(warn.contains("Remove-NetFirewallRule"), "{warn}");
        assert!(!warn.to_ascii_lowercase().contains("tailscale"), "{warn}");

        // 4. Allowed on all profiles (no Profile= restriction or includes Public) -> no warning.
        let allowed_all_output = r#"
    {A1}    REG_SZ    v2.30|Action=Allow|Active=TRUE|Dir=In|Protocol=6|App=C:\Users\user\.local\bin\anticrumb.exe|Name=anticrumb|
"#;
        assert!(evaluate_windows_firewall_rules(allowed_all_output, exe, 50303, cidr).is_none());

        // 5. Inactive block rule or different executable must not trigger a warning.
        let unrelated_output = r#"
    {B1}    REG_SZ    v2.30|Action=Block|Active=FALSE|Dir=In|Protocol=6|App=C:\Users\user\.local\bin\anticrumb.exe|
    {B2}    REG_SZ    v2.30|Action=Block|Active=TRUE|Dir=Out|Protocol=6|App=C:\Users\user\.local\bin\anticrumb.exe|
    {B3}    REG_SZ    v2.30|Action=Block|Active=TRUE|Dir=In|Protocol=6|App=C:\Other\anticrumb.exe|
"#;
        assert!(evaluate_windows_firewall_rules(unrelated_output, exe, 50303, cidr).is_none());
    }

    #[test]
    fn macos_firewall_parser_detects_blocked_application_when_enabled() {
        let exe = "/Users/alice/.local/bin/anticrumb";
        assert!(evaluate_macos_firewall_status(
            "Firewall is disabled. (State = 0)\n",
            "Incoming connection to the application is blocked\n",
            exe
        )
        .is_none());

        assert!(evaluate_macos_firewall_status(
            "Firewall is enabled. (State = 1)\n",
            "Incoming connection to the application is permitted\n",
            exe
        )
        .is_none());

        let warn = evaluate_macos_firewall_status(
            "Firewall is enabled. (State = 1)\n",
            "Incoming connection to the application is blocked\n",
            exe,
        )
        .unwrap();
        assert!(
            warn.contains("macOS Application Firewall is blocking"),
            "{warn}"
        );
        assert!(warn.contains("--unblockapp"), "{warn}");
    }
}
