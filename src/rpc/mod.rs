//! The protobuf contract is shared by the native module, IPC and network hosts.
pub mod pb {
    tonic::include_proto!("anticrumb.v1");
}
mod cache;
pub mod client;
pub mod codec;
pub mod connection;
pub mod daemon;
pub mod ffi;
pub(crate) mod fingerprint;
pub mod host;
mod host_sync;
pub(crate) mod local;
pub mod relay;
pub mod server;
pub mod server_cmd;
pub(crate) mod tls;
pub mod workspace_client;
pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;
pub const MAX_ARTIFACT: usize = 32 * 1024 * 1024;

pub(crate) fn validate_token(token: &str) -> crate::domain::Result<()> {
    if !(32..=4096).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(crate::domain::ServiceError::invalid(
            "token must contain 32..4096 printable non-whitespace ASCII characters",
        ));
    }
    Ok(())
}

pub(crate) fn constant_time_eq(provided: &str, expected: &str) -> bool {
    let mismatch = provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b));
    provided.len() == expected.len() && mismatch == 0
}

pub fn private_write(path: &std::path::Path, data: &[u8]) -> crate::domain::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(crate::domain::ServiceError::storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(crate::domain::ServiceError::storage)?;
        }
        file.write_all(data)
            .map_err(crate::domain::ServiceError::storage)?;
        file.sync_all()
            .map_err(crate::domain::ServiceError::storage)?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(crate::domain::ServiceError::storage)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
