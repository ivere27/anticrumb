//! Explicit remote setup with certificate confirmation before sending credentials.
use super::{
    client::{Connection, RpcClient},
    fingerprint, pb, private_write as atomic_private_write, tls, validate_token as valid_token,
};
use crate::{cli, domain::*};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    connection: Connection,
}

fn endpoint(value: &str) -> Result<String> {
    let url = crate::url_lite::Url::parse(value)
        .map_err(|_| ServiceError::invalid("invalid connection URL"))?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(ServiceError::invalid("connect requires an https://HOST:PORT URL without credentials, paths or query parameters"));
    }
    Ok(url.origin().ascii_serialization())
}

pub fn load(root: &Path) -> Result<Option<Connection>> {
    let path = root.join("connection.json");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ServiceError::storage(format!(
                "cannot read saved connection {}: {error}",
                path.display()
            )))
        }
    };
    let mut bytes = Vec::new();
    file.take(32 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(ServiceError::storage)?;
    if bytes.len() > 32 * 1024 {
        return Err(ServiceError::invalid("saved connection exceeds 32 KiB"));
    }
    let saved: Saved = serde_json::from_slice(&bytes).map_err(|_| {
        ServiceError::invalid("invalid saved connection configuration; run anticrumb connect again")
    })?;
    if saved.version != 1 {
        return Err(ServiceError::invalid(
            "unsupported saved connection version",
        ));
    }
    match &saved.connection {
        Connection::Grpc {
            endpoint: address,
            token,
            tls_pin,
            ..
        } => {
            endpoint(address)?;
            valid_token(token)?;
            if let Some(pin) = tls_pin {
                tls::normalize_pin(pin)?;
            }
        }
        _ => {
            return Err(ServiceError::invalid(
                "saved connections must use remote gRPC",
            ))
        }
    }
    Ok(Some(saved.connection))
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(ServiceError::storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(ServiceError::storage)?;
    }
    Ok(())
}

fn configuration_lock(root: &Path) -> Result<fs::File> {
    private_directory(root)?;
    use fs2::FileExt;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("connection.lock"))
        .map_err(ServiceError::storage)?;
    lock.try_lock_exclusive().map_err(|_| {
        ServiceError::new(
            ErrorCode::WorkspaceConflict,
            "connect",
            "another command is changing the saved connection",
        )
    })?;
    Ok(lock)
}

struct Setup {
    endpoint: String,
    token: String,
    pin: Option<String>,
    ca_file: Option<PathBuf>,
    server_name: Option<String>,
}
impl Setup {
    fn parse(address: &str, args: &[String]) -> Result<Self> {
        let mut token = std::env::var("ANTICRUMB_TOKEN").ok();
        let mut pin = None;
        let mut ca_file = None;
        let mut server_name = None;
        let mut args = args.iter();
        while let Some(option) = args.next() {
            if !["--token", "--tls-pin", "--ca-file", "--server-name"].contains(&option.as_str()) {
                return Err(ServiceError::invalid("connect accepts --token STRING, --tls-pin SHA256, --ca-file PATH and --server-name NAME"));
            }
            let value = args
                .next()
                .ok_or_else(|| ServiceError::invalid(format!("missing value for {option}")))?;
            match option.as_str() {
                "--token" => token = Some(value.clone()),
                "--tls-pin" => pin = Some(tls::normalize_pin(value)?),
                "--ca-file" => {
                    ca_file = Some(fs::canonicalize(value).map_err(ServiceError::storage)?)
                }
                "--server-name" => server_name = Some(value.clone()),
                _ => unreachable!(),
            }
        }
        let token = token
            .ok_or_else(|| ServiceError::invalid("supply --token STRING or set ANTICRUMB_TOKEN"))?;
        valid_token(&token)?;
        Ok(Self {
            endpoint: endpoint(address)?,
            token,
            pin,
            ca_file,
            server_name,
        })
    }
}

fn configure(setup: Setup) -> Result<Connection> {
    let root = cli::config_root();
    let runtime = tokio::runtime::Runtime::new().map_err(ServiceError::storage)?;
    let _lease = configuration_lock(&root)?;
    let mut pin = setup.pin;
    let mut certificate = None;
    if setup.ca_file.is_none() {
        let der = runtime.block_on(tls::inspect_peer(
            &setup.endpoint,
            setup.server_name.as_deref(),
        ))?;
        let offered = tls::certificate_fingerprint(&der);
        if let Some(expected) = &pin {
            if expected != &offered {
                return Err(ServiceError::new(
                    ErrorCode::AuthRequired,
                    "tls",
                    "server certificate does not match --tls-pin; no token was sent",
                ));
            }
        } else {
            let previous = load(&root)?;
            let trusted = matches!(&previous, Some(Connection::Grpc { endpoint, tls_pin: Some(saved), .. })
                if endpoint == &setup.endpoint && saved == &offered);
            if !trusted {
                eprintln!(
                    "Server: {}\nSHA256: {offered}\nEmoji: {}",
                    setup.endpoint,
                    fingerprint::emoji_fingerprint(&der)
                );
                if matches!(&previous, Some(Connection::Grpc { endpoint, .. }) if endpoint == &setup.endpoint)
                {
                    eprintln!("The server certificate has changed since the previous connection.");
                }
                eprint!("Compare with the fingerprint displayed by the daemon. Trust this certificate? [y/N] ");
                io::stderr().flush().map_err(ServiceError::storage)?;
                let mut answer = String::new();
                io::stdin()
                    .lock()
                    .take(128)
                    .read_line(&mut answer)
                    .map_err(ServiceError::storage)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    return Err(ServiceError::new(ErrorCode::Cancelled, "connect", "certificate was not trusted; no token was sent and saved settings are unchanged"));
                }
            }
        }
        pin = Some(offered);
        certificate = Some(der);
    }
    let client = runtime.block_on(RpcClient::network_pinned(
        &setup.endpoint,
        &setup.token,
        setup.ca_file.as_deref(),
        setup.server_name.as_deref(),
        pin.as_deref(),
    ))?;
    let info = runtime
        .block_on(client.get_info(pb::Empty {}))
        .map_err(|mut error| {
            error.message = format!(
                "browser daemon authentication at {} failed: {}; check --token",
                setup.endpoint,
                error.message.replace(&setup.token, "[redacted]")
            );
            error
        })?;
    if info.role != "browser" {
        return Err(ServiceError::invalid(
            "the endpoint is not a browser daemon; use the address of `anticrumb daemon`",
        ));
    }
    let mut ca_file = setup.ca_file;
    if let Some(der) = certificate {
        let directory = root.join("certificates");
        private_directory(&directory)?;
        let path = directory.join(format!("{}.pem", tls::certificate_fingerprint(&der)));
        atomic_private_write(&path, tls::certificate_der_to_pem(&der).as_bytes())?;
        ca_file = Some(fs::canonicalize(path).map_err(ServiceError::storage)?);
    }
    let connection = Connection::Grpc {
        endpoint: setup.endpoint.clone(),
        token: setup.token,
        ca_file,
        server_name: setup.server_name,
        tls_pin: pin,
    };
    atomic_private_write(
        &root.join("connection.json"),
        &serde_json::to_vec_pretty(&Saved {
            version: 1,
            connection: connection.clone(),
        })
        .map_err(ServiceError::storage)?,
    )?;
    eprintln!(
        "Connected to {} ({}). Profile saved for `anticrumb connect`; plain `anticrumb` uses the local daemon.",
        setup.endpoint, info.server_id
    );
    Ok(connection)
}

fn verify(runtime: &tokio::runtime::Runtime, connection: &Connection) -> Result<pb::HostInfo> {
    let client = runtime.block_on(RpcClient::connect(connection))?;
    let info = runtime
        .block_on(client.get_info(pb::Empty {}))
        .map_err(|mut error| {
            if let Connection::Grpc {
                endpoint, token, ..
            } = connection
            {
                error.message = format!(
                    "browser daemon at {}: {}",
                    super::client::endpoint_description(endpoint),
                    error.message.replace(token, "[redacted]")
                );
            }
            error
        })?;
    if info.role != "browser" {
        return Err(ServiceError::invalid(
            "the endpoint is not a browser daemon",
        ));
    }
    Ok(info)
}

pub fn handles(args: &[String]) -> bool {
    args.is_empty()
        || args.first().is_some_and(|value| {
            matches!(value.as_str(), "status" | "local") || value.contains("://")
        })
}

fn status(root: &Path) -> Result<serde_json::Value> {
    let saved = match load(root)? {
        Some(Connection::Grpc {
            endpoint, tls_pin, ..
        }) => json!({
            "transport": "grpc", "endpoint": super::client::endpoint_description(&endpoint),
            "fingerprint": tls_pin,
        }),
        _ => serde_json::Value::Null,
    };
    Ok(json!({"default":"local", "saved_remote":saved}))
}

/// Return an explicitly selected remote connection; informational commands do not
/// start a runtime or contact either a local host or the saved remote endpoint.
pub fn command(args: &[String]) -> Result<Option<Connection>> {
    match args.first().map(String::as_str) {
        Some("status") if args.len() == 1 => {
            println!("{}", status(&cli::config_root())?);
            Ok(None)
        }
        Some("local") if args.len() == 1 => {
            let root = cli::config_root();
            let _lease = configuration_lock(&root)?;
            match fs::remove_file(root.join("connection.json")) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(ServiceError::storage(error)),
            }
            println!("Saved remote profile removed. Run anticrumb to use the local daemon.");
            Ok(None)
        }
        Some(address) => configure(Setup::parse(address, &args[1..])?).map(Some),
        None => {
            let Some(connection) = load(&cli::config_root())? else {
                return Err(ServiceError::invalid(
                    "no saved remote profile; run anticrumb connect https://HOST:PORT --token STRING",
                ));
            };
            let runtime = tokio::runtime::Runtime::new().map_err(ServiceError::storage)?;
            verify(&runtime, &connection)?;
            Ok(Some(connection))
        }
    }
}

pub fn inline(args: &[String]) -> Result<Option<(Connection, Vec<String>)>> {
    let before_separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    if !args[..before_separator]
        .iter()
        .any(|arg| arg == "--connect")
    {
        return Ok(None);
    }
    let mut remaining = Vec::new();
    let mut options = Vec::new();
    let mut address = None;
    let mut index = 0;
    while index < before_separator {
        let arg = &args[index];
        if [
            "--connect",
            "--token",
            "--tls-pin",
            "--ca-file",
            "--server-name",
        ]
        .contains(&arg.as_str())
        {
            let value = args
                .get(index + 1)
                .filter(|_| index + 1 < before_separator)
                .ok_or_else(|| ServiceError::invalid(format!("missing value for {arg}")))?;
            if arg == "--connect" {
                if address.replace(value.clone()).is_some() {
                    return Err(ServiceError::invalid("use --connect only once"));
                }
            } else {
                options.extend([arg.clone(), value.clone()]);
            }
            index += 2;
        } else {
            remaining.push(arg.clone());
            index += 1;
        }
    }
    remaining.extend_from_slice(&args[before_separator..]);
    let connection = configure(Setup::parse(address.as_deref().unwrap(), &options)?)?;
    Ok(Some((connection, remaining)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_urls_reject_embedded_secrets_and_non_tls_endpoints() {
        for address in [
            "http://localhost:50051",
            "https://user:private-token@localhost:50051",
            "https://localhost:50051/?token=private-token",
            "https://localhost:50051/private-token",
            "https://localhost:50051/#private-token",
            "not-a-url-private-token",
        ] {
            let error = endpoint(address).unwrap_err();
            assert!(!error.message.contains("private-token"));
        }
        assert_eq!(
            endpoint("https://localhost:50051/").unwrap(),
            "https://localhost:50051"
        );
    }

    #[test]
    fn saved_connection_is_private_and_reloads_without_a_separate_token_file() {
        let root = tempfile::tempdir().unwrap();
        let token = "synthetic-connection-token-32-characters";
        let connection = Connection::Grpc {
            endpoint: "https://browser.example:50051".into(),
            token: token.into(),
            ca_file: None,
            server_name: None,
            tls_pin: Some("a".repeat(64)),
        };
        atomic_private_write(
            &root.path().join("connection.json"),
            &serde_json::to_vec(&Saved {
                version: 1,
                connection,
            })
            .unwrap(),
        )
        .unwrap();
        let loaded = load(root.path()).unwrap().unwrap();
        assert!(!format!("{loaded:?}").contains(token));
        assert!(matches!(loaded, Connection::Grpc { token: value, .. } if value == token));
        assert!(!root.path().join("token").exists());
        let summary = status(root.path()).unwrap();
        assert_eq!(summary["default"], "local");
        assert_eq!(
            summary["saved_remote"]["endpoint"],
            "https://browser.example:50051"
        );
        assert!(!summary.to_string().contains(token));
        assert!(!summary.to_string().contains("connected"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("connection.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn connection_status_is_local_without_creating_state() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("absent");
        assert_eq!(
            status(&absent).unwrap(),
            json!({"default":"local", "saved_remote":null})
        );
        assert!(!absent.exists());
        assert!(handles(&[]));
    }

    #[test]
    fn configuration_changes_serialize_and_reject_local_profiles() {
        let root = tempfile::tempdir().unwrap();
        let lease = configuration_lock(root.path()).unwrap();
        let error = configuration_lock(root.path()).unwrap_err();
        assert_eq!(error.code, ErrorCode::WorkspaceConflict);
        assert!(error.message.contains("changing the saved connection"));
        drop(lease);
        assert!(configuration_lock(root.path()).is_ok());
        let saved = Saved {
            version: 1,
            connection: Connection::Local,
        };
        fs::write(
            root.path().join("connection.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        assert!(load(root.path()).is_err());
    }
}
