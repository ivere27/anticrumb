//! Encapsulated debug-log session management for the terminal UI.
use crate::domain::now_ms;
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Default)]
pub(super) struct DebugLogger {
    pub level: u8,
    file: Option<std::fs::File>,
    pub path: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    last_error: Option<String>,
}

impl DebugLogger {
    pub(super) fn label(level: u8) -> &'static str {
        match level {
            0 => "off",
            1 => "info",
            _ => "trace",
        }
    }

    pub(super) fn parse_level(arg: &str) -> Option<u8> {
        match arg.trim().to_ascii_lowercase().as_str() {
            "0" | "off" | "false" | "none" => Some(0),
            "1" | "on" | "true" | "info" | "basic" => Some(1),
            "2" | "trace" | "verbose" | "full" | "all" => Some(2),
            _ => None,
        }
    }

    fn record_error(&mut self, msg: String) -> Option<String> {
        if self.last_error.as_deref() != Some(&msg) {
            self.last_error = Some(msg.clone());
            Some(msg)
        } else {
            None
        }
    }

    pub(super) fn ensure_file(&mut self, workspace: &Path, account: &str) -> Option<String> {
        if self.level == 0 || self.file.is_some() {
            return None;
        }
        let dir = self.dir.clone().unwrap_or_else(std::env::temp_dir);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            return self.record_error(format!(
                "Could not create debug log directory {}: {err}",
                dir.display()
            ));
        }
        let ts = now_ms();
        let pid = std::process::id();
        let path = dir.join(format!("anticrumb-debug-{ts}-{pid}.log"));
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(mut file) => {
                let header = format!(
                    "=== anticrumb debug log ===\nlevel: {} ({})\nworkspace: {}\naccount: {}\npid: {pid}\nstarted_ms: {ts}\n===========================\n",
                    self.level,
                    Self::label(self.level),
                    workspace.display(),
                    account
                );
                let _ = file.write_all(header.as_bytes());
                let _ = file.flush();
                self.last_error = None;
                self.file = Some(file);
                self.path = Some(path);
                None
            }
            Err(err) => self.record_error(format!(
                "Could not open debug log {}: {err}",
                path.display()
            )),
        }
    }

    pub(super) fn close(&mut self, started: Instant) -> Option<PathBuf> {
        if let Some(mut file) = self.file.take() {
            let line = format!(
                "[{} +{:.3}s] [debug_off] debug log closed\n",
                now_ms(),
                started.elapsed().as_secs_f64()
            );
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
        self.path.take()
    }

    pub(super) fn write(
        &mut self,
        started: Instant,
        workspace: &Path,
        account: &str,
        tag: &str,
        body: &str,
    ) -> Option<String> {
        if self.level == 0 {
            return None;
        }
        if let Some(err) = self.ensure_file(workspace, account) {
            return Some(err);
        }
        let file = self.file.as_mut()?;
        let ts = now_ms();
        let secs = started.elapsed().as_secs_f64();
        let mut entry = format!("[{ts} +{secs:.3}s] [{tag}]");
        if body.contains('\n') {
            entry.push('\n');
            entry.push_str(body);
            if !body.ends_with('\n') {
                entry.push('\n');
            }
        } else {
            entry.push(' ');
            entry.push_str(body);
            entry.push('\n');
        }
        if let Err(err) = file.write_all(entry.as_bytes()).and_then(|_| file.flush()) {
            return self.record_error(format!("Debug log write failed: {err}"));
        }
        None
    }
}
