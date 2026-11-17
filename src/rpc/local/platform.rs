use super::{Result, ServiceError};
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(unix)]
mod implementation {
    use super::super::private_directory;
    use super::*;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    pub type Socket = tokio::net::UnixStream;
    pub struct Listener(tokio::net::UnixListener);

    pub fn endpoint(home: &Path, directory: &Path) -> Result<PathBuf> {
        let direct = directory.join("daemon.sock");
        use std::os::unix::ffi::OsStrExt;
        if direct.as_os_str().as_bytes().len() < 100 {
            return Ok(direct);
        }
        // sockaddr_un is small even when a user's home/workspace path is long.
        let key = crate::domain::digest(home.as_os_str().as_bytes());
        let directory = PathBuf::from(format!(
            "/tmp/anticrumb-{}-{}",
            unsafe { libc::geteuid() },
            &key[..24]
        ));
        private_directory(&directory)?;
        Ok(directory.join("daemon.sock"))
    }
    fn same_user(socket: &Socket) -> io::Result<()> {
        if socket.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local daemon peer is a different OS user",
            ));
        }
        Ok(())
    }
    pub async fn connect(endpoint: &Path) -> io::Result<Socket> {
        let socket = Socket::connect(endpoint).await?;
        same_user(&socket)?;
        Ok(socket)
    }
    impl Listener {
        pub fn bind(endpoint: &Path) -> Result<Self> {
            // Caller holds the daemon lifetime lock. Never unlink a live or
            // foreign socket, symlink, or an unrelated regular file.
            match std::fs::symlink_metadata(endpoint) {
                Ok(metadata)
                    if metadata.file_type().is_socket()
                        && metadata.uid() == unsafe { libc::geteuid() } =>
                {
                    std::fs::remove_file(endpoint).map_err(ServiceError::storage)?;
                }
                Ok(_) => {
                    return Err(ServiceError::invalid(
                        "local daemon endpoint is not an owned socket",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(ServiceError::storage(error)),
            }
            let listener =
                tokio::net::UnixListener::bind(endpoint).map_err(ServiceError::storage)?;
            std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
                .map_err(ServiceError::storage)?;
            Ok(Self(listener))
        }
        pub async fn accept(&mut self) -> io::Result<Socket> {
            loop {
                let (socket, _) = self.0.accept().await?;
                if same_user(&socket).is_ok() {
                    return Ok(socket);
                }
            }
        }
    }
    pub fn remove_endpoint(endpoint: &Path) {
        let _ = std::fs::remove_file(endpoint);
    }
    pub fn detach(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        // No terminal hangup reaches the daemon; other clients may still use it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    pub async fn shutdown_signal() {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(windows)]
mod implementation {
    use super::*;
    use std::{
        ffi::c_void,
        os::windows::{io::AsRawHandle, process::CommandExt},
        ptr,
    };
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        },
        System::{
            Pipes::GetNamedPipeServerProcessId,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, CREATE_NEW_PROCESS_GROUP,
                DETACHED_PROCESS, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };

    pub type Socket = NamedPipeClient;
    pub struct Listener {
        endpoint: PathBuf,
        next: NamedPipeServer,
        security: Security,
    }
    struct Security(*mut c_void);
    // The immutable descriptor is owned by Listener and copied by CreateNamedPipe.
    unsafe impl Send for Security {}
    impl Drop for Security {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn user_sid() -> io::Result<String> {
        process_user_sid(unsafe { GetCurrentProcess() })
    }
    fn process_user_sid(process: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
        unsafe {
            let mut token = ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let result = (|| {
                let mut length = 0;
                GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
                // Keep TOKEN_USER suitably aligned, including on 32-bit Windows.
                let mut buffer =
                    vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
                if GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    length,
                    &mut length,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
                let mut string = ptr::null_mut();
                if ConvertSidToStringSidW(user.User.Sid, &mut string) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut length = 0;
                while *string.add(length) != 0 {
                    length += 1;
                }
                let sid = String::from_utf16_lossy(std::slice::from_raw_parts(string, length));
                LocalFree(string.cast());
                Ok(sid)
            })();
            CloseHandle(token);
            result
        }
    }
    impl Security {
        fn new() -> io::Result<Self> {
            // Explicit user SID; default pipe ACLs also admit Everyone/anonymous.
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", user_sid()?)
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = ptr::null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }
        fn create(&self, endpoint: &Path, first: bool) -> io::Result<NamedPipeServer> {
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            };
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(
                        endpoint,
                        (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                    )
            }
        }
    }
    pub fn endpoint(home: &Path, _: &Path) -> Result<PathBuf> {
        let key = crate::domain::digest(
            format!(
                "{}:{}",
                user_sid().map_err(ServiceError::storage)?,
                home.display()
            )
            .as_bytes(),
        );
        Ok(PathBuf::from(format!(r"\\.\pipe\anticrumb-{}", &key[..32])))
    }
    pub async fn connect(endpoint: &Path) -> io::Result<Socket> {
        // SECURITY_IDENTIFICATION is Tokio's default; the server cannot
        // impersonate a privileged client. Remote clients are rejected above.
        let socket = ClientOptions::new().open(endpoint)?;
        // Verify the server's OS user before sending any remote credentials.
        unsafe {
            let mut pid = 0;
            if GetNamedPipeServerProcessId(socket.as_raw_handle(), &mut pid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let server = process_user_sid(process);
            CloseHandle(process);
            if server? != user_sid()? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "local daemon peer is a different OS user",
                ));
            }
        }
        Ok(socket)
    }
    impl Listener {
        pub fn bind(endpoint: &Path) -> Result<Self> {
            let security = Security::new().map_err(ServiceError::storage)?;
            let next = security
                .create(endpoint, true)
                .map_err(ServiceError::storage)?;
            Ok(Self {
                endpoint: endpoint.into(),
                next,
                security,
            })
        }
        pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
            self.next.connect().await?;
            // Create the next instance before handing this one to HTTP/2.
            let next = self.security.create(&self.endpoint, false)?;
            Ok(std::mem::replace(&mut self.next, next))
        }
    }
    pub fn remove_endpoint(_: &Path) {} // Last handle closes the pipe.
    pub fn detach(command: &mut Command) {
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    pub async fn shutdown_signal() {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub(super) use implementation::*;
