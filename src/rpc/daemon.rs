pub use super::private_write;
use super::server::TlsIdentity;
use super::{host::Host, server, tls, validate_token};
use crate::domain::{Result, ServiceError};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn state_dir() -> PathBuf {
    crate::cli::app_dir().join("browser")
}
#[derive(Default)]
pub struct Options {
    pub root: PathBuf,
    pub listen: String,
    pub managed: bool,
    pub token: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub self_signed: bool,
    pub regenerate_tls: bool,
    pub names: Vec<String>,
    pub workers: Option<usize>,
    pub pace: Option<crate::gemini::HumanPace>,
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
            ..Default::default()
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::slice::Iter<'_, String>| {
                args.next()
                    .cloned()
                    .ok_or_else(|| ServiceError::invalid(format!("missing value for {arg}")))
            };
            match arg.as_str() {
                "--state-dir" => options.root = value(&mut args)?.into(),
                "--listen" => {
                    options.listen = value(&mut args)?;
                    options
                        .listen
                        .parse::<std::net::SocketAddr>()
                        .map_err(|_| ServiceError::invalid("--listen requires IP:PORT"))?;
                }
                "--managed" => options.managed = true,
                "--token" => options.token = Some(value(&mut args)?),
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
                _ => {
                    return Err(ServiceError::invalid(
                        "unknown daemon option; use `anticrumb daemon --help`",
                    ))
                }
            }
        }
        if options.listen.is_empty() {
            if options.token.is_some()
                || options.tls_cert.is_some()
                || options.tls_key.is_some()
                || options.self_signed
                || options.regenerate_tls
                || !options.names.is_empty()
                || options.root != state_dir()
            {
                return Err(ServiceError::invalid("--state-dir, token and TLS options require an explicit --listen IP:PORT; use ANTICRUMB_HOME to isolate a local daemon"));
            }
            // Local IPC trusts OS user permissions and never reads a bearer token.
            return Ok(options);
        }
        if options.managed {
            return Err(ServiceError::invalid(
                "automatic daemons cannot listen on a network address",
            ));
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
        options
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| {
                ServiceError::invalid(
                    "invalid daemon listen address; use --listen IP:PORT, such as 0.0.0.0:50051",
                )
            })?;
        if options.tls_cert.is_none() {
            options.self_signed = true;
        }
        if options.regenerate_tls && options.tls_cert.is_some() {
            return Err(ServiceError::invalid(
                "--regenerate-tls only replaces the generated self-signed TLS identity",
            ));
        }
        options.token = options.token.or_else(inherited_token);
        if let Some(token) = &options.token {
            validate_token(token)?;
        }
        Ok(options)
    }
    pub fn network_token(&self) -> Result<String> {
        let value = match &self.token {
            Some(token) => token.clone(),
            None => crate::cli::token(&self.root)?,
        };
        validate_token(&value)?;
        Ok(value)
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
        let (cert, key) = if self.tls_cert.is_none() {
            let directory = self.root.join("tls");
            let cert = directory.join("certificate.pem");
            let key = directory.join("private-key.pem");
            let manifest = directory.join("names.json");
            let mut names = self.names.clone();
            names.extend(["localhost".into(), "127.0.0.1".into(), "::1".into()]);
            let address: std::net::SocketAddr =
                self.listen.parse().map_err(ServiceError::storage)?;
            if !address.ip().is_unspecified() {
                names.push(address.ip().to_string());
            }
            names.sort();
            names.dedup();
            if !self.regenerate_tls && (cert.exists() || key.exists()) {
                let previous: Vec<String> =
                    serde_json::from_slice(&fs::read(&manifest).map_err(ServiceError::storage)?)
                        .map_err(ServiceError::storage)?;
                if previous != names {
                    return Err(ServiceError::invalid("self-signed certificate names changed; restart with --regenerate-tls to replace the identity, then reconnect clients to trust the new fingerprint"));
                }
            } else {
                install_generated_identity(&directory, &names)?;
            }
            (cert, key)
        } else if let (Some(cert), Some(key)) = (&self.tls_cert, &self.tls_key) {
            (cert.clone(), key.clone())
        } else {
            return Err(ServiceError::invalid(
                "network daemon requires a TLS identity",
            ));
        };
        Ok(Some(TlsIdentity::from_pem(
            fs::read(cert).map_err(ServiceError::storage)?,
            fs::read(key).map_err(ServiceError::storage)?,
        )))
    }
}

fn install_generated_identity(directory: &Path, names: &[String]) -> Result<()> {
    let rcgen::CertifiedKey {
        cert: certificate,
        key_pair,
    } = rcgen::generate_simple_self_signed(names.to_vec()).map_err(ServiceError::storage)?;
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
        private_write(
            &staged.join("private-key.pem"),
            key_pair.serialize_pem().as_bytes(),
        )?;
        private_write(
            &staged.join("certificate.pem"),
            certificate.pem().as_bytes(),
        )?;
        private_write(
            &staged.join("names.json"),
            &serde_json::to_vec(names).map_err(ServiceError::storage)?,
        )?;
        // The daemon owns the state lease while replacing the complete identity.
        // Stage all three files before touching the old key/certificate pair.
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

pub fn run(args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        println!("Run a persistent local daemon:\n  anticrumb daemon\n\nNormal anticrumb commands connect through a Unix socket or Windows named pipe.\nThey start a daemon automatically when needed; the automatic daemon exits after\nits last client disconnects. An explicitly started daemon stays running.\nLocal connections use OS user permissions and need no token or TLS setup.\n\nExpose a browser daemon to another machine:\n  anticrumb daemon --listen IP:50051\n\nOnly --listen opens a TCP port. TLS and a token are created automatically.\nCopy the printed connect command to the other machine and compare fingerprints.\nThe HTTPS port serves browser RPC and provider APIs (/v1 and /v1beta).\nWorkspace files and shell tools always execute on the TUI machine.\n\nOptions:\n  --workers N                        TempChat tabs per account (default 2)\n  --pace LEVEL                       fast, low (default), high\n  --token STRING                     remote token override (or ANTICRUMB_TOKEN)\n  --state-dir PATH                   remote browser state directory\n  --tls-san NAME                     add a hostname/IP to the certificate\n  --tls-self-signed                  generate/reuse a self-signed identity\n  --regenerate-tls                   replace the certificate\n  --tls-cert PATH --tls-key PATH     use a supplied PEM identity\n\nUse ANTICRUMB_HOME for a separate local daemon and state directory.");
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
            if options.listen.is_empty() {
                return super::local::run(options.managed, None).await;
            }
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
                        "daemon_listen",
                        format!(
                            "cannot bind browser daemon at {}: {error}; check for an existing service on this port, stop the conflicting instance or choose another address with `anticrumb daemon --listen IP:PORT`",
                            options.listen
                        ),
                    )
                })?;
            let tls = options.identity()?.ok_or_else(|| {
                ServiceError::invalid("network daemon requires a TLS identity")
            })?;
            // Validate the key/certificate pair before generating or displaying
            // a token. The serving path uses this exact in-memory identity.
            let _ = tls.acceptor()?;
            let certificate = options
                .tls_cert
                .clone()
                .unwrap_or_else(|| options.root.join("tls/certificate.pem"));
            let der = certificate_der(&fs::read(certificate).map_err(ServiceError::storage)?)?;
            let token = options.network_token()?;
            let address = listener.local_addr().map_err(ServiceError::storage)?;
            eprintln!("anticrumb daemon: https://{address}");
            eprintln!("Provider API: https://{address}/v1 (same TLS certificate and token)");
            let fingerprint = tls::certificate_fingerprint(&der);
            eprintln!("TLS certificate SHA256: {fingerprint}");
            eprintln!(
                "TLS certificate emoji: {}",
                super::fingerprint::emoji_fingerprint(&der)
            );
            if options.token.is_none() {
                eprintln!("Connection token: {token}");
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
                eprintln!("Replace HOST with a name included via --tls-san; alternatively, bind the machine's IP with --listen IP:PORT.");
            }
            tokio::try_join!(server::serve(
                host.clone(),
                Some(token),
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                Some(tls),
            ), super::local::run(false, Some(host)))?;
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
            &[
                "--listen",
                "127.0.0.1:0",
                "--state-dir",
                state.to_str().unwrap(),
            ],
            None,
        )
        .unwrap();
        assert!(options.self_signed);
        assert!(options.token.is_none());
        assert!(!state.exists());
        let options = parse(&["--listen", "127.0.0.1:0"], Some(TOKEN)).unwrap();
        assert!(options.self_signed);
        assert_eq!(options.network_token().unwrap(), TOKEN);
        let options = parse(
            &["--listen", "127.0.0.1:0", "--token", TOKEN],
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
            vec!["--listen", "127.0.0.1:50051", "--insecure-http"],
            vec!["--listen", "[::1]:50051", "--insecure-http"],
            vec!["--listen", "0.0.0.0:50051", "--insecure-http"],
            vec!["--insecure-http", "--tls-self-signed"],
            vec!["--insecure-http", "--regenerate-tls"],
            vec!["--tls-cert", "cert", "--tls-key", "key", "--regenerate-tls"],
        ] {
            assert!(parse(&args, Some(TOKEN)).is_err());
        }
        let options = parse(
            &["--listen", "127.0.0.1:0", "--regenerate-tls"],
            Some(TOKEN),
        )
        .unwrap();
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
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let pem = generated.cert.pem();
        let expected = digest(generated.cert.der().as_ref());
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
        let cert_path = root.path().join("tls/certificate.pem");
        let key_path = root.path().join("tls/private-key.pem");
        let first_cert = fs::read(&cert_path).unwrap();
        let first_key = fs::read(&key_path).unwrap();
        let first_pin = certificate_fingerprint(&first_cert).unwrap();
        options.identity().unwrap().unwrap();
        assert!(fs::read(&cert_path).unwrap() == first_cert);
        assert!(fs::read(&key_path).unwrap() == first_key);

        options.names.push("browser.example".into());
        assert!(options.identity().is_err());
        assert!(fs::read(&cert_path).unwrap() == first_cert);
        assert!(fs::read(&key_path).unwrap() == first_key);

        options.regenerate_tls = true;
        options.identity().unwrap().unwrap();
        assert_ne!(
            certificate_fingerprint(&fs::read(&cert_path).unwrap()).unwrap(),
            first_pin
        );
        assert!(fs::read(&key_path).unwrap() != first_key);
        options.regenerate_tls = false;
        options.identity().unwrap().unwrap();
        let renewed_cert = fs::read(&cert_path).unwrap();
        let renewed_key = fs::read(&key_path).unwrap();
        options.regenerate_tls = true;
        options.names.push("non-ascii-☃".into());
        assert!(options.identity().is_err());
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
        }
    }
}
