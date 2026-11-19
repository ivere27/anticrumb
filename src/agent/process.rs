use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// Hard cap per stream so a chatty process cannot exhaust memory (extra bytes are drained+dropped).
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BACKGROUND_PROCESSES: usize = 8;

type SharedBuf = Arc<Mutex<Vec<u8>>>;

fn spawn_reader<R: Read + Send + 'static>(mut src: R, buf: SharedBuf) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match src.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut b) = buf.lock() {
                        let room = MAX_CAPTURE_BYTES.saturating_sub(b.len());
                        b.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
    })
}

/// A native shell child whose stdout/stderr are continuously captured on reader threads, so partial
/// output stays available on timeout/cancel and background processes can be polled.
pub struct CapturedProcess {
    child: Child,
    stdout: SharedBuf,
    stderr: SharedBuf,
    readers: Vec<JoinHandle<()>>,
    exit_code: Option<i32>,
    tree_terminated: bool,
    #[cfg(windows)]
    job: WindowsJob,
}

impl CapturedProcess {
    pub fn spawn(command: &str, cwd: &Path) -> Result<Self, String> {
        Self::spawn_with_env(command, cwd, None)
    }
    pub fn spawn_with_env(
        command: &str,
        cwd: &Path,
        environment: Option<&BTreeMap<String, String>>,
    ) -> Result<Self, String> {
        let shell = match environment {
            Some(environment) => environment.get("ANTICRUMB_SHELL").map(Into::into),
            None => std::env::var_os("ANTICRUMB_SHELL"),
        }
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "powershell.exe".into()
            } else {
                "bash".into()
            }
        });
        let powershell = Path::new(&shell).file_stem().is_some_and(|s| {
            let s = s.to_string_lossy();
            s.eq_ignore_ascii_case("powershell") || s.eq_ignore_ascii_case("pwsh")
        });
        let mut cmd = Command::new(&shell);
        if let Some(environment) = environment {
            cmd.env_clear().envs(environment);
        }
        if powershell {
            cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                .arg(format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); $OutputEncoding = [Console]::OutputEncoding; {command}"));
        } else {
            cmd.arg("-c").arg(command);
        }
        cmd.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group, so the whole tree (make -> cargo -> rustc ...) can be killed.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn shell: {e}"))?;
        #[cfg(windows)]
        let job = match WindowsJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let stdout: SharedBuf = Arc::default();
        let stderr: SharedBuf = Arc::default();
        let mut readers = Vec::new();
        if let Some(p) = child.stdout.take() {
            readers.push(spawn_reader(p, Arc::clone(&stdout)));
        }
        if let Some(p) = child.stderr.take() {
            readers.push(spawn_reader(p, Arc::clone(&stderr)));
        }
        Ok(Self {
            child,
            stdout,
            stderr,
            readers,
            exit_code: None,
            tree_terminated: false,
            #[cfg(windows)]
            job,
        })
    }

    /// `Some(code)` once the process exited (cached afterwards).
    pub fn poll_exit(&mut self) -> Option<i32> {
        if self.exit_code.is_none() {
            if let Ok(Some(st)) = self.child.try_wait() {
                self.exit_code = Some(st.code().unwrap_or(-1));
            }
        }
        // Keep draining descendants that still own output pipes until the tool's
        // timeout/cancellation boundary. Once the command is fully reported as
        // finished, descendants with redirected output must not keep running.
        if self.exit_code.is_some() && self.readers_finished() {
            self.terminate_tree();
        }
        self.exit_code
    }

    fn terminate_tree(&mut self) {
        if self.tree_terminated {
            return;
        }
        // Clean up exactly once, including when the shell has exited and the
        // output pipes are closed. Repeated cleanup must not signal a reused PID.
        self.tree_terminated = true;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
        }
        #[cfg(windows)]
        self.job.terminate();
    }

    /// Kill the owned Unix process group or Windows Job Object, including descendants.
    pub fn kill(&mut self) {
        self.terminate_tree();
        let _ = self.child.kill();
        if let Ok(st) = self.child.wait() {
            self.exit_code = Some(st.code().unwrap_or(-1));
        }
    }

    pub fn stdout_len(&self) -> usize {
        self.stdout.lock().map(|b| b.len()).unwrap_or(0)
    }

    pub fn stderr_len(&self) -> usize {
        self.stderr.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Output captured from byte offsets `(out_from, err_from)` onwards.
    pub fn output_since(&self, out_from: usize, err_from: usize) -> (String, String) {
        fn tail(buf: &SharedBuf, from: usize) -> String {
            buf.lock()
                .map(|b| String::from_utf8_lossy(&b[from.min(b.len())..]).to_string())
                .unwrap_or_default()
        }
        (tail(&self.stdout, out_from), tail(&self.stderr, err_from))
    }

    /// Wait for the reader threads (call after the process exited or was killed).
    pub fn readers_finished(&self) -> bool {
        self.readers.iter().all(|h| h.is_finished())
    }

    pub fn join_readers(&mut self) {
        let deadline = Instant::now() + std::time::Duration::from_millis(250);
        while !self.readers_finished() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for h in self.readers.drain(..) {
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for CapturedProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn client_environments_and_working_directories_are_isolated() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for (directory, name) in [(first.path(), "first"), (second.path(), "second")] {
                scope.spawn(move || {
                    let environment = BTreeMap::from([
                        ("PATH".into(), "/usr/bin:/bin".into()),
                        ("ANTICRUMB_SHELL".into(), "/bin/bash".into()),
                        ("PROJECT_NAME".into(), name.into()),
                    ]);
                    let mut process = CapturedProcess::spawn_with_env(
                        "printf '%s\\n' \"$PROJECT_NAME\" \"$PWD\" \"${HOME-unset}\"",
                        directory,
                        Some(&environment),
                    )
                    .unwrap();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while process.poll_exit().is_none() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert_eq!(process.poll_exit(), Some(0));
                    process.join_readers();
                    let expected_dir = std::fs::canonicalize(directory).unwrap();
                    assert_eq!(
                        process.output_since(0, 0).0,
                        format!("{name}\n{}\nunset\n", expected_dir.display())
                    );
                });
            }
        });
    }

    #[test]
    fn shell_completion_cleans_redirected_background_descendants() {
        let dir = tempfile::tempdir().unwrap();
        // The child closes captured output, stays alive after its parent exits,
        // and waits for us to release it before attempting a workspace write.
        let mut process = CapturedProcess::spawn(
            "(touch ready; for i in {1..500}; do if [ -e release ]; then echo leaked > survived; break; fi; sleep 0.01; done) >/dev/null 2>&1 & while [ ! -e ready ]; do sleep 0.01; done",
            dir.path(),
        ).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process.poll_exit().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(process.poll_exit(), Some(0));
        process.join_readers();
        assert!(process.readers_finished());
        drop(process);
        std::fs::write(dir.path().join("release"), "go").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !dir.path().join("survived").exists(),
            "redirected descendant outlived its command"
        );
    }
}

pub fn format_report(code: Option<i32>, stdout: &str, stderr: &str) -> String {
    let mut report = match code {
        Some(c) => format!("exit_code: {c}"),
        None => "exit_code: (running)".to_string(),
    };
    if !stdout.trim().is_empty() {
        report.push_str(&format!("\n[stdout]\n{}", stdout.trim_end()));
    }
    if !stderr.trim().is_empty() {
        report.push_str(&format!("\n[stderr]\n{}", stderr.trim_end()));
    }
    report
}

struct BackgroundEntry {
    command: String,
    started: Instant,
    proc: CapturedProcess,
    out_cursor: usize,
    err_cursor: usize,
}

/// Long-running shell processes (dev servers, watchers, slow builds) started by the agent that
/// keep running while the agent continues working. All of them die with the manager.
#[derive(Default)]
pub struct BackgroundProcesses {
    next_id: AtomicU64,
    procs: Mutex<BTreeMap<u64, BackgroundEntry>>,
}

impl BackgroundProcesses {
    pub fn start(&self, command: &str, cwd: &Path) -> Result<u64, String> {
        self.start_with_env(command, cwd, None)
    }
    pub fn start_with_env(
        &self,
        command: &str,
        cwd: &Path,
        environment: Option<&BTreeMap<String, String>>,
    ) -> Result<u64, String> {
        let mut procs = self.procs.lock().map_err(|e| e.to_string())?;
        let running = procs
            .values_mut()
            .map(|e| e.proc.poll_exit().is_none())
            .filter(|alive| *alive)
            .count();
        if running >= MAX_BACKGROUND_PROCESSES {
            return Err(format!(
                "too many background processes running ({running}); kill one with stop_process first"
            ));
        }
        let proc = CapturedProcess::spawn_with_env(command, cwd, environment)?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        procs.insert(
            id,
            BackgroundEntry {
                command: command.to_string(),
                started: Instant::now(),
                proc,
                out_cursor: 0,
                err_cursor: 0,
            },
        );
        Ok(id)
    }

    /// New output since the previous poll plus a status line. `None` if the id is unknown.
    pub fn poll(&self, id: u64) -> Option<(Option<i32>, String)> {
        let mut procs = self.procs.lock().ok()?;
        let e = procs.get_mut(&id)?;
        let code = e.proc.poll_exit();
        if code.is_some() {
            e.proc.join_readers();
        }
        let (o, er) = e.proc.output_since(e.out_cursor, e.err_cursor);
        e.out_cursor += o.len();
        e.err_cursor += er.len();
        let mut report = format!(
            "process #{id} `{}` ({}s)\n",
            e.command,
            e.started.elapsed().as_secs()
        );
        report.push_str(&format_report(code, &o, &er));
        Some((code, report))
    }

    /// `(exit code if finished, unread output waiting)`; `None` if the id is unknown.
    pub fn status(&self, id: u64) -> Option<(Option<i32>, bool)> {
        let mut procs = self.procs.lock().ok()?;
        let e = procs.get_mut(&id)?;
        let unread = e.proc.stdout_len() > e.out_cursor || e.proc.stderr_len() > e.err_cursor;
        Some((e.proc.poll_exit(), unread))
    }

    pub fn kill(&self, id: u64) -> bool {
        let Ok(mut procs) = self.procs.lock() else {
            return false;
        };
        match procs.get_mut(&id) {
            Some(e) => {
                e.proc.kill();
                true
            }
            None => false,
        }
    }

    pub fn kill_all(&self) {
        if let Ok(mut procs) = self.procs.lock() {
            for e in procs.values_mut() {
                e.proc.kill();
            }
        }
    }

    pub fn list(&self) -> Vec<String> {
        let Ok(mut procs) = self.procs.lock() else {
            return Vec::new();
        };
        procs
            .iter_mut()
            .map(|(id, e)| {
                let st = match e.proc.poll_exit() {
                    Some(c) => format!("exited({c})"),
                    None => "running".to_string(),
                };
                format!("#{id} [{st}] {}", e.command)
            })
            .collect()
    }
}

impl Drop for BackgroundProcesses {
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for WindowsJob {}
#[cfg(windows)]
impl WindowsJob {
    fn attach(child: &Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let job = Self(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0
            {
                return Err(format!(
                    "assign shell process to job: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(job)
        }
    }
    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}
#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
