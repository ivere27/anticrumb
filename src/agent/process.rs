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

    /// Incrementally read up to `max_bytes` raw bytes from each stream starting at
    /// `(*stdout_off, *stderr_off)`, advancing each offset by the exact number of raw
    /// bytes consumed.
    ///
    /// Incomplete multi-byte UTF-8 sequences (1..=3 trailing bytes) are deferred while
    /// `!flush_incomplete` or when more bytes already follow in the buffer beyond `max_bytes`.
    /// Invalid interior bytes are replaced with U+FFFD and skipped so reading never stalls.
    /// When `flush_incomplete` is true and the buffer end is reached, trailing incomplete
    /// bytes are emitted as U+FFFD and consumed.
    pub fn take_output_since(
        &self,
        stdout_off: &mut usize,
        stderr_off: &mut usize,
        max_bytes: usize,
        flush_incomplete: bool,
    ) -> (String, String) {
        fn take_buf(
            buf: &SharedBuf,
            offset: &mut usize,
            max_bytes: usize,
            flush_incomplete: bool,
        ) -> String {
            if max_bytes == 0 {
                return String::new();
            }
            let Ok(guard) = buf.lock() else {
                return String::new();
            };
            let start = (*offset).min(guard.len());
            if start >= guard.len() {
                return String::new();
            }
            let mut end = start.saturating_add(max_bytes).min(guard.len());
            let mut at_buf_end = end == guard.len();
            let (mut out, mut consumed) =
                decode_utf8_incremental(&guard[start..end], flush_incomplete && at_buf_end);
            // If `max_bytes` was smaller than a single valid multi-byte UTF-8 char
            // and more bytes are already buffered, extend up to 4 bytes so callers
            // cannot livelock on tiny chunk limits.
            if consumed == 0 && !at_buf_end {
                end = start.saturating_add(4).min(guard.len());
                at_buf_end = end == guard.len();
                let (ext_out, ext_consumed) =
                    decode_utf8_incremental(&guard[start..end], flush_incomplete && at_buf_end);
                out = ext_out;
                consumed = ext_consumed;
            }
            *offset = start + consumed;
            out
        }
        (
            take_buf(&self.stdout, stdout_off, max_bytes, flush_incomplete),
            take_buf(&self.stderr, stderr_off, max_bytes, flush_incomplete),
        )
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

/// Decode `bytes` as UTF-8, returning `(decoded_text, raw_bytes_consumed)`.
/// Invalid interior sequences are replaced with `U+FFFD` and skipped; trailing incomplete
/// sequences (1..=3 bytes) are deferred unless `flush_incomplete` is true.
pub(crate) fn decode_utf8_incremental(bytes: &[u8], flush_incomplete: bool) -> (String, usize) {
    let mut out = String::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        match std::str::from_utf8(&bytes[pos..]) {
            Ok(valid) => {
                out.push_str(valid);
                pos = bytes.len();
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if valid_len > 0 {
                    let valid = std::str::from_utf8(&bytes[pos..pos + valid_len])
                        .expect("valid UTF-8 prefix");
                    out.push_str(valid);
                    pos += valid_len;
                }
                match err.error_len() {
                    Some(bad_len) => {
                        out.push(char::REPLACEMENT_CHARACTER);
                        pos += bad_len;
                    }
                    None => {
                        if flush_incomplete {
                            out.push(char::REPLACEMENT_CHARACTER);
                            pos = bytes.len();
                        }
                        break;
                    }
                }
            }
        }
    }
    (out, pos)
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

    #[test]
    fn decode_utf8_incremental_defers_boundary_replaces_invalid_and_flushes_eof() {
        // Multi-byte Korean ("한글", 3B each) + 4B emoji ("🚀") split at arbitrary boundaries.
        let src = "안녕🚀세계".as_bytes();
        // Cut after 2 bytes of '안' (incomplete 3B sequence) -> defers cleanly.
        let (chunk1, n1) = decode_utf8_incremental(&src[..2], false);
        assert_eq!(chunk1, "");
        assert_eq!(n1, 0);

        // Cut in the middle of '🚀' (3B '안' + 3B '녕' + 2B of '🚀' = 8 bytes).
        let (chunk2, n2) = decode_utf8_incremental(&src[n1..8], false);
        assert_eq!(chunk2, "안녕");
        assert_eq!(n2, 6);

        // Next chunk finishes '🚀' and includes 1 byte of '세'.
        let (chunk3, n3) = decode_utf8_incremental(&src[n1 + n2..11], false);
        assert_eq!(chunk3, "🚀");
        assert_eq!(n3, 4);

        // Final chunk completes "세계".
        let (chunk4, n4) = decode_utf8_incremental(&src[n1 + n2 + n3..], true);
        assert_eq!(chunk4, "세계");
        assert_eq!(n1 + n2 + n3 + n4, src.len());

        // Invalid interior bytes are replaced with U+FFFD and skipped without stalling.
        let invalid = b"ok\xff\xfeend";
        let (decoded, consumed) = decode_utf8_incremental(invalid, false);
        assert_eq!(decoded, "ok\u{FFFD}\u{FFFD}end");
        assert_eq!(consumed, invalid.len());

        // Trailing incomplete sequence: deferred when !flush_incomplete, flushed as U+FFFD at EOF.
        let dangling = b"hi\xe2\x82";
        let (d1, c1) = decode_utf8_incremental(dangling, false);
        assert_eq!(d1, "hi");
        assert_eq!(c1, 2);
        let (d2, c2) = decode_utf8_incremental(&dangling[c1..], true);
        assert_eq!(d2, "\u{FFFD}");
        assert_eq!(c1 + c2, dangling.len());
    }

    #[test]
    fn captured_process_take_output_since_advances_raw_offsets_and_handles_utf8_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut proc = CapturedProcess::spawn(
            "printf '한글🚀stdout\\n'; printf '오류🔥stderr\\n' >&2",
            dir.path(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while proc.poll_exit().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(proc.poll_exit(), Some(0));
        proc.join_readers();

        // Drain with max_bytes = 2 (smaller than 3B/4B UTF-8 chars) to verify zero livelock
        // and exact byte-offset advancement.
        let mut out_off = 0usize;
        let mut err_off = 0usize;
        let mut out_all = String::new();
        let mut err_all = String::new();
        for _ in 0..100 {
            let (o, e) = proc.take_output_since(&mut out_off, &mut err_off, 2, false);
            out_all.push_str(&o);
            err_all.push_str(&e);
            if out_off >= proc.stdout_len() && err_off >= proc.stderr_len() {
                break;
            }
        }
        let (o_tail, e_tail) = proc.take_output_since(&mut out_off, &mut err_off, 64, true);
        out_all.push_str(&o_tail);
        err_all.push_str(&e_tail);
        assert_eq!(out_off, proc.stdout_len());
        assert_eq!(err_off, proc.stderr_len());
        assert_eq!(out_all, "한글🚀stdout\n");
        assert_eq!(err_all, "오류🔥stderr\n");
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
        let (o, er) = e.proc.take_output_since(
            &mut e.out_cursor,
            &mut e.err_cursor,
            MAX_CAPTURE_BYTES,
            code.is_some(),
        );
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
