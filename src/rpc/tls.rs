//! Pinned TLS still validates the certificate name, validity and handshake signatures.
use crate::domain::{ErrorCode, Result, ServiceError};
use base64::Engine as _;
use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tonic::transport::{Channel, Endpoint};

pub(super) fn normalize_pin(value: &str) -> Result<String> {
    let value = value.trim();
    let valid = if value.contains(':') {
        let parts: Vec<_> = value.split(':').collect();
        parts.len() == 32
            && parts
                .iter()
                .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
    } else {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    if !valid {
        return Err(ServiceError::invalid(
            "TLS pin must be a SHA-256 certificate fingerprint: 64 hexadecimal digits, optionally separated into byte pairs by colons",
        ));
    }
    Ok(value.replace(':', "").to_ascii_lowercase())
}

pub(crate) fn certificate_fingerprint(der: &[u8]) -> String {
    crate::domain::digest(der)
}

pub(crate) fn certificate_der_to_pem(der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for offset in (0..encoded.len()).step_by(64) {
        pem.push_str(&encoded[offset..(offset + 64).min(encoded.len())]);
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

#[derive(Debug)]
struct PinnedVerifier {
    pin: String,
    roots: Option<Vec<CertificateDer<'static>>>,
    provider: Arc<CryptoProvider>,
}

fn verify_anchor_validity(der: &[u8], now: UnixTime) -> std::result::Result<(), rustls::Error> {
    if let Some((not_before, not_after)) = super::daemon::certificate_validity_epochs(der) {
        let now_secs = now.as_secs();
        if now_secs < not_before || now_secs > not_after {
            return Err(rustls::Error::General(
                "TLS trust anchor certificate is not valid at current time".into(),
            ));
        }
    }
    Ok(())
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // A configured pin is explicit trust in the root trust anchor (internal CA
        // or self-signed certificate), allowing short-lived leaf rotation.
        let anchor = match &self.roots {
            Some(configured) => configured
                .iter()
                .find(|cert| crate::domain::digest(cert.as_ref()) == self.pin)
                .cloned()
                .ok_or_else(|| {
                    rustls::Error::General(
                        "TLS certificate fingerprint does not match the configured pin".into(),
                    )
                })?,
            None => {
                let candidate = intermediates.last().unwrap_or(end_entity);
                if crate::domain::digest(candidate.as_ref()) != self.pin {
                    return Err(rustls::Error::General(
                        "TLS certificate fingerprint does not match the configured pin".into(),
                    ));
                }
                candidate.clone().into_owned()
            }
        };
        verify_anchor_validity(anchor.as_ref(), now)?;
        let mut roots = RootCertStore::empty();
        roots.add(anchor)?;
        let verifier =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
                .build()
                .map_err(|error| rustls::Error::General(error.to_string()))?;
        verifier.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct InspectionVerifier {
    certificate: Arc<Mutex<Option<Vec<u8>>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for InspectionVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let candidate = intermediates.last().unwrap_or(end_entity);
        // Inspect the offered trust anchor (root CA or self-signed leaf), requiring
        // normal chain, hostname, and validity checks before it can be shown to the user.
        PinnedVerifier {
            pin: certificate_fingerprint(candidate.as_ref()),
            roots: None,
            provider: self.provider.clone(),
        }
        .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        *self.certificate.lock().map_err(|_| inspection_stopped())? =
            Some(candidate.as_ref().to_vec());
        // Never finish this handshake: inspection cannot send a bearer token or
        // any other application data, and does not establish certificate trust.
        Err(inspection_stopped())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(inspection_stopped())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(inspection_stopped())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn inspection_stopped() -> rustls::Error {
    rustls::Error::General("certificate inspection stops before authentication".into())
}

/// Inspect a candidate certificate without completing TLS or sending application data.
pub(crate) async fn inspect_peer(endpoint: &str, server_name: Option<&str>) -> Result<Vec<u8>> {
    let result = inspect_peer_inner(endpoint, server_name).await;
    result.map_err(|mut error| {
        error.message = format!(
            "TLS certificate inspection endpoint={}: {}",
            super::client::endpoint_description(endpoint),
            error.message
        );
        error
    })
}

async fn inspect_peer_inner(endpoint: &str, server_name: Option<&str>) -> Result<Vec<u8>> {
    let endpoint = crate::url_lite::Url::parse(endpoint)
        .map_err(|_| ServiceError::invalid("invalid TLS endpoint"))?;
    if endpoint.scheme() != "https"
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err(ServiceError::invalid(
            "certificate inspection requires an https URL without credentials",
        ));
    }
    let (host, port, name) = peer_address(&endpoint, server_name)?;
    let certificate = Arc::new(Mutex::new(None));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(ServiceError::storage)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InspectionVerifier {
            certificate: certificate.clone(),
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let attempt = tokio::time::timeout(Duration::from_secs(5), async move {
        let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await
    })
    .await;
    let certificate = certificate
        .lock()
        .map_err(|_| {
            ServiceError::new(
                ErrorCode::Internal,
                "connect",
                "certificate inspection lock failed",
            )
        })?
        .take();
    if let Some(certificate) = certificate {
        return Ok(certificate);
    }
    let message = match attempt {
        Err(error) => error.to_string(),
        Ok(Err(error)) => super::client::error_chain(&error),
        Ok(Ok(_)) => "server did not supply a certificate for inspection".into(),
    };
    Err(ServiceError::new(
        ErrorCode::TransportDisconnected,
        "connect",
        message,
    ))
}

fn peer_address(
    endpoint: &crate::url_lite::Url,
    server_name: Option<&str>,
) -> Result<(String, u16, ServerName<'static>)> {
    let host = match endpoint.host() {
        Some(crate::url_lite::Host::Domain(host)) => host.to_owned(),
        Some(crate::url_lite::Host::Ipv4(host)) => host.to_string(),
        Some(crate::url_lite::Host::Ipv6(host)) => host.to_string(),
        None => return Err(ServiceError::invalid("gRPC endpoint requires a hostname")),
    };
    let port = endpoint
        .port_or_known_default()
        .ok_or_else(|| ServiceError::invalid("gRPC endpoint requires a valid network port"))?;
    let name = ServerName::try_from(server_name.unwrap_or(&host).to_owned())
        .map_err(|_| ServiceError::invalid("invalid TLS server name"))?;
    Ok((host, port, name))
}

pub(super) async fn connect(
    endpoint: &crate::url_lite::Url,
    pin: &str,
    ca_file: Option<&Path>,
    server_name: Option<&str>,
) -> Result<Channel> {
    let pin = normalize_pin(pin)?;
    let (host, port, name) = peer_address(endpoint, server_name)?;
    let roots = if let Some(path) = ca_file {
        let bytes = std::fs::read(path).map_err(ServiceError::storage)?;
        let mut parsed = Vec::new();
        let mut check_store = RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut bytes.as_slice()) {
            let cert = cert.map_err(ServiceError::storage)?;
            check_store
                .add(cert.clone())
                .map_err(ServiceError::storage)?;
            parsed.push(cert);
        }
        if parsed.is_empty() {
            return Err(ServiceError::invalid(
                "TLS CA file contains no certificates",
            ));
        }
        Some(parsed)
    } else {
        None
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedVerifier {
        pin,
        roots,
        provider: provider.clone(),
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(ServiceError::storage)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    // Every reconnect revalidates the pinned certificate and its validity period.
    config.resumption = rustls::client::Resumption::disabled();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let connector = tower::service_fn(move |_| {
        let host = host.clone();
        let name = name.clone();
        let connector = connector.clone();
        async move {
            let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
            stream.set_nodelay(true)?;
            let stream = connector.connect(name, stream).await?;
            if stream.get_ref().1.alpn_protocol() != Some(b"h2".as_slice()) {
                return Err(std::io::Error::other("TLS server did not negotiate HTTP/2"));
            }
            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
        }
    });
    let origin = endpoint.origin().ascii_serialization();
    // This custom connector already performs TLS. The internal URI must be HTTP
    // to prevent tonic wrapping the verified stream in a second TLS session;
    // the request origin remains HTTPS and no plaintext connection is possible.
    let internal = origin.replacen("https://", "http://", 1);
    Endpoint::from_shared(internal)
        .map_err(ServiceError::storage)?
        .origin(origin.parse().map_err(ServiceError::storage)?)
        .connect_timeout(Duration::from_secs(5))
        .connect_with_connector(connector)
        .await
        .map_err(|error| {
            ServiceError::new(
                ErrorCode::TransportDisconnected,
                "connect",
                super::client::error_chain(&error),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_format_is_normalized_and_rejects_malformed_values() {
        assert_eq!(normalize_pin(&"AB".repeat(32)).unwrap(), "ab".repeat(32));
        assert_eq!(
            normalize_pin(&["AB"; 32].join(":")).unwrap(),
            "ab".repeat(32)
        );
        for value in ["", "ab", &"gg".repeat(32), &format!(":{}", "ab".repeat(32))] {
            assert!(normalize_pin(value).is_err());
        }
    }

    #[test]
    fn pin_still_requires_matching_name_and_certificate_validity() {
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        ca_params.not_after = rcgen::date_time_ymd(2030, 1, 1);
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "anticrumb Root CA");
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut leaf_params = rcgen::CertificateParams::new(vec!["daemon.example".into()]).unwrap();
        leaf_params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        leaf_params.not_after = rcgen::date_time_ymd(2025, 1, 1);
        leaf_params.is_ca = rcgen::IsCa::ExplicitNoCa;
        leaf_params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params.use_authority_key_identifier_extension = true;
        let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let certificate = leaf_cert.der();
        let chain = [ca_cert.der().clone()];
        let verifier = PinnedVerifier {
            pin: crate::domain::digest(ca_cert.der().as_ref()),
            roots: None,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        };
        let name = ServerName::try_from("daemon.example").unwrap();
        let valid_time = UnixTime::since_unix_epoch(Duration::from_secs(1_700_000_000));
        assert!(verifier
            .verify_server_cert(certificate, &chain, &name, &[], valid_time)
            .is_ok());
        let wrong_name = ServerName::try_from("wrong.example").unwrap();
        assert!(verifier
            .verify_server_cert(certificate, &chain, &wrong_name, &[], valid_time)
            .is_err());
        let before_validity = UnixTime::since_unix_epoch(Duration::ZERO);
        assert!(verifier
            .verify_server_cert(certificate, &chain, &name, &[], before_validity)
            .is_err());
        let after_validity = UnixTime::since_unix_epoch(Duration::from_secs(1_800_000_000));
        assert!(verifier
            .verify_server_cert(certificate, &chain, &name, &[], after_validity)
            .is_err());
        let wrong_pin = PinnedVerifier {
            pin: "00".repeat(32),
            ..verifier
        };
        assert!(wrong_pin
            .verify_server_cert(certificate, &chain, &name, &[], valid_time)
            .is_err());
    }

    #[tokio::test]
    async fn inspection_checks_hostname_and_stops_before_application_data() {
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "anticrumb Root CA");
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut leaf_params = rcgen::CertificateParams::new(vec!["daemon.example".into()]).unwrap();
        leaf_params.is_ca = rcgen::IsCa::ExplicitNoCa;
        leaf_params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params.use_authority_key_identifier_extension = true;
        let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let expected_ca = ca_cert.der().to_vec();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der());
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.der().clone(), ca_cert.der().clone()],
            key.into(),
        )
        .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                // Neither the valid-name inspection nor the rejected-name
                // inspection may finish TLS and reach an RPC authorization check.
                assert!(acceptor.accept(stream).await.is_err());
            }
        });
        let inspected = inspect_peer(&endpoint, Some("daemon.example"))
            .await
            .unwrap();
        assert_eq!(inspected, expected_ca);
        let pem = certificate_der_to_pem(&inspected);
        let decoded = rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(decoded.as_ref(), expected_ca);
        assert!(inspect_peer(&endpoint, Some("wrong.example"))
            .await
            .is_err());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
