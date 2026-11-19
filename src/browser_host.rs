use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::url_lite::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_CDP_PORT: u16 = 9222;
pub const GEMINI_APP_URL: &str = "https://gemini.google.com/app";

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub port: u16,
    pub profile_dir: PathBuf,
    pub profile_sub_dir: Option<String>,
    pub headless: bool,
    pub xvfb: bool,
    pub window_size: Option<(u32, u32)>,
    pub initial_url: String,
    pub detached: bool,
}

impl LaunchOptions {
    pub fn for_account(
        account: &crate::domain::Account,
        headless: bool,
        initial_url: impl Into<String>,
    ) -> Self {
        Self {
            port: account.port,
            profile_dir: account.profile.clone(),
            profile_sub_dir: account.subprofile.clone(),
            headless,
            xvfb: false,
            window_size: Some((1280, 900)),
            initial_url: initial_url.into(),
            detached: true,
        }
    }

    pub fn headless_account(
        account: &crate::domain::Account,
        initial_url: impl Into<String>,
    ) -> Self {
        Self::for_account(account, true, initial_url)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunningCdpMeta {
    pub port: u16,
    pub user_data_dir: String,
    pub profile_dir: String,
    pub mode: String,
    pub headless: bool,
    pub xvfb: bool,
    pub pid: u32,
}

impl RunningCdpMeta {
    pub fn matches_account(&self, account: &crate::domain::Account) -> bool {
        let expected_sub = account
            .subprofile
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("Default");
        let same_root = fs::canonicalize(&account.profile)
            .ok()
            .zip(fs::canonicalize(&self.user_data_dir).ok())
            .is_some_and(|(a, b)| a == b);
        same_root && self.profile_dir.eq_ignore_ascii_case(expected_sub)
    }
}

pub(crate) struct CloseTabGuard {
    pub port: u16,
    pub target_id: Option<String>,
}

impl CloseTabGuard {
    pub fn new(port: u16, target_id: impl Into<String>) -> Self {
        Self {
            port,
            target_id: Some(target_id.into()),
        }
    }
}

impl Drop for CloseTabGuard {
    fn drop(&mut self) {
        if let Some(id) = &self.target_id {
            let _ = close_tab(self.port, id);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub ws_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserVersionInfo {
    #[serde(rename = "Browser", default)]
    pub browser: String,
    #[serde(rename = "Protocol-Version", default)]
    pub protocol_version: String,
    #[serde(rename = "User-Agent", default)]
    pub user_agent: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub ws_url: String,
}

#[derive(Debug)]
pub struct BrowserSession {
    pub port: u16,
    pub ws_path: String,
    pub profile_dir: PathBuf,
    pub child: Option<Child>,
    pub detached: bool,
    pub launched_new: bool,
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        if !self.detached {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if self.port != 0 {
                remove_running_meta(self.port);
            }
        }
    }
}

pub fn anticrumb_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("ANTICRUMB_HOME") {
        let trimmed = dir.to_string_lossy();
        let trimmed = trimmed.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    home.join(".anticrumb")
}

pub fn default_profile_dir() -> PathBuf {
    anticrumb_home().join("profile")
}

pub fn find_chrome_binary() -> Option<PathBuf> {
    if let Ok(path_str) = std::env::var("CHROME_BIN") {
        let trimmed = path_str.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.exists() {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let mut bases: Vec<PathBuf> = Vec::new();
        for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(val) = std::env::var(var) {
                let p = PathBuf::from(val);
                if !bases.contains(&p) {
                    bases.push(p);
                }
            }
        }
        for fixed in [r"C:\Program Files", r"C:\Program Files (x86)"] {
            let p = PathBuf::from(fixed);
            if !bases.contains(&p) {
                bases.push(p);
            }
        }
        let rels = [
            r"Google\Chrome\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
            r"BraveSoftware\Brave-Browser\Application\brave.exe",
            r"Chromium\Application\chrome.exe",
        ];
        for base in bases {
            for rel in rels {
                let full = base.join(rel);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
        for bin in ["chrome.exe", "msedge.exe", "brave.exe"] {
            if let Some(p) = find_in_path(bin) {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let mut app_roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            app_roots.push(PathBuf::from(home).join("Applications"));
        }
        let rels = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
        ];
        for root in app_roots {
            for rel in rels {
                let full = root.join(rel);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
        for bin in ["google-chrome", "chromium", "msedge", "brave-browser"] {
            if let Some(p) = find_in_path(bin) {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let candidates = [
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge-stable",
            "microsoft-edge",
            "brave-browser",
        ];
        for c in candidates {
            if let Some(p) = find_in_path(c) {
                return Some(p);
            }
        }
    }

    None
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let full: PathBuf = Path::new(&dir).join(binary);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

pub fn which_xvfb_run() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        find_in_path("xvfb-run")
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn cdp_meta_path_in(home: &Path, port: u16) -> PathBuf {
    home.join(format!("cdp-port-{port}.json"))
}

fn cdp_meta_path(port: u16) -> PathBuf {
    cdp_meta_path_in(&anticrumb_home(), port)
}

pub fn save_running_meta(meta: &RunningCdpMeta) {
    let path = cdp_meta_path(meta.port);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string_pretty(meta) {
        let _ = fs::write(&path, body);
    }
}

fn remove_running_meta_in(home: &Path, port: u16) {
    let _ = fs::remove_file(cdp_meta_path_in(home, port));
}

pub fn remove_running_meta(port: u16) {
    remove_running_meta_in(&anticrumb_home(), port);
}

#[cfg(any(unix, test))]
fn is_chrome_positional_arg(tok: &str) -> bool {
    tok.contains("://")
        || tok.starts_with("about:")
        || tok.starts_with("chrome:")
        || tok.starts_with("data:")
}

#[cfg(any(unix, test))]
fn parse_linux_cmdline_bytes(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let raw_parts: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    if raw_parts.is_empty() {
        return Vec::new();
    }
    if raw_parts.len() > 1 || !raw_parts[0].chars().any(char::is_whitespace) {
        return raw_parts;
    }

    // Chromium/Chrome uses SetProcessTitle on Linux, which replaces `/proc/<pid>/cmdline`
    // with a single NUL-terminated string containing space-joined argv tokens.
    let tokens: Vec<&str> = raw_parts[0].split_whitespace().collect();
    let mut args: Vec<String> = Vec::with_capacity(tokens.len());
    for (idx, tok) in tokens.iter().enumerate() {
        if args.is_empty() {
            args.push((*tok).to_string());
            continue;
        }
        let is_flag = tok.starts_with('-');
        let is_url = is_chrome_positional_arg(tok);
        let prev_is_kv_flag = args
            .last()
            .map(|p| p.starts_with("--") && p.contains('='))
            .unwrap_or(false);
        let has_upcoming_flag = tokens[idx..].iter().any(|t| t.starts_with('-'));

        if !is_flag && !is_url && (prev_is_kv_flag || (args.len() == 1 && has_upcoming_flag)) {
            if let Some(last) = args.last_mut() {
                last.push(' ');
                last.push_str(tok);
                continue;
            }
        }
        args.push((*tok).to_string());
    }
    args
}

fn parse_browser_args(port: u16, pid: u32, args: &[String]) -> Option<RunningCdpMeta> {
    if args.is_empty() {
        return None;
    }
    let port_flag = format!("--remote-debugging-port={port}");
    if !args.iter().any(|a| a == &port_flag) {
        return None;
    }
    // Skip Chromium helper/renderer child processes and wrapper shells
    if args.iter().any(|a| a.starts_with("--type=")) {
        return None;
    }
    let prog = Path::new(&args[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_browser = prog.contains("chrome")
        || prog.contains("chromium")
        || prog.contains("msedge")
        || prog.contains("brave");
    if !is_browser {
        return None;
    }

    let user_data_dir = args
        .iter()
        .find_map(|a| a.strip_prefix("--user-data-dir="))
        .unwrap_or("")
        .to_string();
    let profile_dir = args
        .iter()
        .find_map(|a| a.strip_prefix("--profile-directory="))
        .filter(|s| !s.is_empty())
        .unwrap_or("Default")
        .to_string();
    let is_headless = args.iter().any(|a| a.starts_with("--headless"));
    let is_xvfb = args.iter().any(|a| a == "--ozone-platform=x11") && !is_headless;
    let mode = if is_headless {
        "headless"
    } else if is_xvfb {
        "xvfb"
    } else {
        "visible"
    }
    .to_string();

    Some(RunningCdpMeta {
        port,
        user_data_dir,
        profile_dir,
        mode,
        headless: is_headless,
        xvfb: is_xvfb,
        pid,
    })
}

#[cfg(target_os = "linux")]
fn inspect_linux_proc_cdp(port: u16) -> Option<RunningCdpMeta> {
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let pid_str = fname.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args = parse_linux_cmdline_bytes(&bytes);
        let pid = pid_str.parse::<u32>().unwrap_or(0);
        if let Some(meta) = parse_browser_args(port, pid, &args) {
            return Some(meta);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn inspect_native_cdp(port: u16) -> Option<RunningCdpMeta> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,command="])
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_start();
        let (pid, command) = line.split_once(char::is_whitespace)?;
        let args = parse_linux_cmdline_bytes(command.trim_start().as_bytes());
        if let Some(meta) = parse_browser_args(port, pid.parse().ok()?, &args) {
            return Some(meta);
        }
    }
    None
}

#[cfg(windows)]
fn windows_arguments(command: &str) -> Vec<String> {
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};
    let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let mut count = 0;
    // Use Windows' parser so quoted paths and backslashes retain their exact meaning.
    unsafe {
        let arguments = CommandLineToArgvW(wide.as_ptr(), &mut count);
        if arguments.is_null() {
            return Vec::new();
        }
        let mut result = Vec::new();
        for argument in std::slice::from_raw_parts(arguments, count as usize) {
            let mut length = 0;
            while *argument.add(length) != 0 {
                length += 1;
            }
            result.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                *argument, length,
            )));
        }
        LocalFree(arguments.cast());
        result
    }
}

#[cfg(windows)]
fn inspect_native_cdp(port: u16) -> Option<RunningCdpMeta> {
    use std::os::windows::process::CommandExt;
    let output = Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new(); @(Get-CimInstance Win32_Process -Filter \"Name='chrome.exe' OR Name='chromium.exe' OR Name='msedge.exe' OR Name='brave.exe'\" | Select-Object ProcessId,CommandLine) | ConvertTo-Json -Compress"])
        .creation_flags(0x08000000).output().ok()?;
    let values: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;
    values.iter().find_map(|value| {
        let args = windows_arguments(value["CommandLine"].as_str()?);
        parse_browser_args(port, value["ProcessId"].as_u64()? as u32, &args)
    })
}

#[cfg(target_os = "linux")]
fn linux_pid_matches_cdp_port(port: u16, pid: u32) -> Option<RunningCdpMeta> {
    if pid == 0 {
        return None;
    }
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args = parse_linux_cmdline_bytes(&bytes);
    parse_browser_args(port, pid, &args)
}

fn inspect_running_cdp_alive_in(home: &Path, port: u16) -> Option<RunningCdpMeta> {
    let cached: Option<RunningCdpMeta> = fs::read_to_string(cdp_meta_path_in(home, port))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    #[cfg(target_os = "linux")]
    {
        if let Some(meta) = cached
            .as_ref()
            .and_then(|m| linux_pid_matches_cdp_port(port, m.pid))
        {
            return Some(meta);
        }
        if let Some(meta) = inspect_linux_proc_cdp(port) {
            return Some(meta);
        }
    }
    #[cfg(any(target_os = "macos", windows))]
    if let Some(meta) = inspect_native_cdp(port) {
        return Some(meta);
    }
    cached
}

fn inspect_running_cdp_alive(port: u16) -> Option<RunningCdpMeta> {
    inspect_running_cdp_alive_in(&anticrumb_home(), port)
}

fn inspect_running_cdp_in(home: &Path, port: u16) -> Option<RunningCdpMeta> {
    if port == 0 || !is_port_alive(port) {
        return None;
    }
    inspect_running_cdp_alive_in(home, port)
}

pub fn inspect_running_cdp(port: u16) -> Option<RunningCdpMeta> {
    inspect_running_cdp_in(&anticrumb_home(), port)
}

pub fn fetch_version(port: u16) -> Result<BrowserVersionInfo, String> {
    let body = http_request(port, "GET", "/json/version", Duration::from_millis(2500))?;
    serde_json::from_str(&body).map_err(|e| format!("parse /json/version: {e}"))
}

pub fn is_port_alive(port: u16) -> bool {
    port != 0 && fetch_version(port).is_ok()
}

pub fn list_targets(port: u16) -> Result<Vec<TargetInfo>, String> {
    let body = http_request(port, "GET", "/json/list", Duration::from_secs(3))?;
    serde_json::from_str(&body).map_err(|e| format!("parse /json/list: {e}"))
}

/// A browser process owned exclusively by one external browsing session. Its
/// profile is never shared with the signed-in browser used by providers.
struct PublicBrowser {
    port: u16,
    ws_path: String,
    child: Option<Child>,
    profile: PathBuf,
    #[cfg(windows)]
    job: Option<BrowserJob>,
}

impl PublicBrowser {
    fn start(proxy: SocketAddr) -> Result<Self, String> {
        let chrome = find_chrome_binary().ok_or("Chrome/Chromium executable not found")?;
        let profile = std::env::temp_dir().join(format!(
            "anticrumb-public-browser-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let directory = fs::DirBuilder::new();
        #[cfg(unix)]
        let directory = {
            use std::os::unix::fs::DirBuilderExt;
            let mut directory = directory;
            directory.mode(0o700);
            directory
        };
        directory
            .create(&profile)
            .map_err(|e| format!("create isolated browser profile: {e}"))?;
        let mut browser = Self {
            port: 0,
            ws_path: String::new(),
            child: None,
            profile,
            #[cfg(windows)]
            job: None,
        };
        fs::create_dir(browser.profile.join("Default"))
            .map_err(|e| format!("create isolated browser preferences: {e}"))?;
        // HTTP proxies do not carry ICE/STUN UDP. Apply Chromium's WebRTC
        // policy before any page runs, as both a preference and launch switch.
        fs::write(
            browser.profile.join("Default/Preferences"),
            json!({"webrtc": {
                "ip_handling_policy": "disable_non_proxied_udp",
                "multiple_routes_enabled": false,
                "nonproxied_udp_enabled": false
            }})
            .to_string(),
        )
        .map_err(|e| format!("write isolated browser preferences: {e}"))?;
        let mut command = Command::new(chrome);
        command
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--disable-extensions",
                "--disable-quic",
                "--webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--proxy-bypass-list=<-loopback>",
                "--window-size=1280,900",
            ])
            .arg(format!("--proxy-server=http://{proxy}"))
            .arg(format!("--user-data-dir={}", browser.profile.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        browser.child = Some(
            command
                .spawn()
                .map_err(|e| format!("launch isolated Chrome: {e}"))?,
        );
        #[cfg(windows)]
        {
            browser.job = Some(BrowserJob::attach(browser.child.as_ref().unwrap())?);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = browser
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .map_err(|e| format!("inspect isolated Chrome: {e}"))?
            {
                return Err(format!(
                    "isolated Chrome exited before becoming ready: {status}"
                ));
            }
            if let Ok(active) = fs::read_to_string(browser.profile.join("DevToolsActivePort")) {
                let mut lines = active.lines();
                if let (Some(port), Some(path)) = (
                    lines.next().and_then(|line| line.parse::<u16>().ok()),
                    lines.next(),
                ) {
                    if port != 0 && path.starts_with("/devtools/browser/") {
                        browser.port = port;
                        browser.ws_path = path.to_owned();
                        if let Ok(version) = fetch_version(port) {
                            if ws_path_from_url(&version.ws_url).as_deref() == Ok(path) {
                                return Ok(browser);
                            }
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err("isolated Chrome did not become ready within 15 seconds".into());
            }
            sleep(Duration::from_millis(50));
        }
    }

    fn connect(&self) -> Result<CdpClient, String> {
        let mut cdp = CdpClient::connect(self.port, &self.ws_path)?;
        cdp.set_control(
            crate::agent::CancelToken::new(),
            Instant::now() + Duration::from_secs(3),
        );
        Ok(cdp)
    }
}

impl Drop for PublicBrowser {
    fn drop(&mut self) {
        if self
            .child
            .as_mut()
            .is_some_and(|child| !matches!(child.try_wait(), Ok(Some(_))))
        {
            if !self.ws_path.is_empty() {
                if let Ok(mut cdp) = self.connect() {
                    let _ = cdp.call("Browser.close", json!({}));
                }
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            let child = self.child.as_mut().unwrap();
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                sleep(Duration::from_millis(25));
            }
            if !matches!(child.try_wait(), Ok(Some(_))) {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                #[cfg(windows)]
                if let Some(job) = &self.job {
                    job.terminate();
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        #[cfg(windows)]
        drop(self.job.take());
        // This path was created by this instance with an unpredictable name.
        // Never remove an account profile or a caller-supplied directory.
        let _ = fs::remove_dir_all(&self.profile);
    }
}

#[cfg(windows)]
struct BrowserJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for BrowserJob {}
#[cfg(windows)]
impl BrowserJob {
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
                    "assign isolated Chrome to job: {}",
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
impl Drop for BrowserJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Owns an isolated browsing context, disposable browser and outbound proxy.
/// Dropping the guard closes the context and browser before stopping the proxy.
pub struct BrowserContextGuard {
    context_id: String,
    port: u16,
    browser: Option<PublicBrowser>,
    _proxy: Option<crate::browser_network::BrowserNetworkProxy>,
}

impl BrowserContextGuard {
    fn connect(&self) -> Result<CdpClient, String> {
        if let Some(browser) = &self.browser {
            return browser.connect();
        }
        let version = fetch_version(self.port)?;
        let mut cdp = CdpClient::connect(self.port, &ws_path_from_url(&version.ws_url)?)?;
        cdp.set_control(
            crate::agent::CancelToken::new(),
            Instant::now() + Duration::from_secs(3),
        );
        Ok(cdp)
    }

    pub(crate) fn targets(&self) -> Result<Vec<TargetInfo>, String> {
        let result = self.connect()?.call("Target.getTargets", json!({}))?;
        Ok(result["targetInfos"]
            .as_array()
            .ok_or("missing browser targets")?
            .iter()
            .filter(|t| t["type"] == "page" && t["browserContextId"] == self.context_id)
            .filter_map(|t| {
                let id = t["targetId"].as_str()?.to_owned();
                Some(TargetInfo {
                    ws_url: format!("ws://127.0.0.1:{}/devtools/page/{id}", self.port),
                    id,
                    target_type: "page".into(),
                    title: t["title"].as_str().unwrap_or("").into(),
                    url: t["url"].as_str().unwrap_or("").into(),
                })
            })
            .collect())
    }
    /// Open another tab sharing this session's cookies and network policy.
    pub(crate) fn create_tab(&self) -> Result<TargetInfo, String> {
        let mut cdp = self.connect()?;
        let result = cdp.call(
            "Target.createTarget",
            json!({
                "url":"about:blank", "browserContextId":self.context_id
            }),
        )?;
        let id = result["targetId"]
            .as_str()
            .ok_or("missing target id")?
            .to_owned();
        Ok(TargetInfo {
            ws_url: format!("ws://127.0.0.1:{}/devtools/page/{id}", self.port),
            id,
            target_type: "page".into(),
            title: String::new(),
            url: "about:blank".into(),
        })
    }
}

impl Drop for BrowserContextGuard {
    fn drop(&mut self) {
        if let Ok(mut cdp) = self.connect() {
            let _ = cdp.call(
                "Target.disposeBrowserContext",
                json!({"browserContextId": self.context_id}),
            );
        }
        // Fields drop in declaration order: the owned process is stopped before
        // its proxy, including popups, workers, and other browser contexts.
    }
}

/// Create an external browsing target with its own network-restricted process.
/// The supplied provider port is used only when private fetching is opted in.
/// Keep the returned guard alive until the target is no longer needed.
pub fn create_browser_tool_tab(
    port: u16,
) -> Result<(u16, TargetInfo, Option<BrowserContextGuard>), String> {
    if crate::web::allow_private_env() {
        // Opting into private destinations does not opt into provider cookies.
        let version = fetch_version(port)?;
        let mut cdp = CdpClient::connect(port, &ws_path_from_url(&version.ws_url)?)?;
        let result = cdp.call("Target.createBrowserContext", json!({}))?;
        let guard = BrowserContextGuard {
            context_id: result["browserContextId"]
                .as_str()
                .ok_or("missing browser context id")?
                .into(),
            port,
            browser: None,
            _proxy: None,
        };
        let target = guard.create_tab()?;
        return Ok((port, target, Some(guard)));
    }
    let proxy = crate::browser_network::BrowserNetworkProxy::start()?;
    let browser = PublicBrowser::start(proxy.address())?;
    let port = browser.port;
    let mut cdp = browser.connect()?;
    let result = cdp.call(
        "Target.createBrowserContext",
        json!({
            "proxyServer": format!("http://{}", proxy.address()),
            "proxyBypassList": "<-loopback>"
        }),
    )?;
    let context_id = result["browserContextId"]
        .as_str()
        .ok_or("missing browser context id")?
        .to_owned();
    let guard = BrowserContextGuard {
        context_id,
        port,
        browser: Some(browser),
        _proxy: Some(proxy),
    };
    let result = cdp.call(
        "Target.createTarget",
        json!({"url": "about:blank", "browserContextId": guard.context_id}),
    )?;
    let id = result["targetId"]
        .as_str()
        .ok_or("missing target id")?
        .to_owned();
    let target = TargetInfo {
        ws_url: format!("ws://127.0.0.1:{port}/devtools/page/{id}"),
        id,
        target_type: "page".into(),
        title: String::new(),
        url: "about:blank".into(),
    };
    Ok((port, target, Some(guard)))
}

pub fn create_tab(port: u16, target_url: &str) -> Result<TargetInfo, String> {
    // A single mutation: a lost create reply must not create a second target.
    let version = fetch_version(port)?;
    let ws_path = ws_path_from_url(&version.ws_url)?;
    let mut cdp = CdpClient::connect(port, &ws_path)?;
    let res = cdp.call("Target.createTarget", json!({ "url": target_url }))?;
    let target_id = res
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Target.createTarget missing targetId: {res}"))?
        .to_string();

    for _ in 0..20 {
        if let Ok(targets) = list_targets(port) {
            if let Some(found) = targets.into_iter().find(|t| t.id == target_id) {
                return Ok(found);
            }
        }
        sleep(Duration::from_millis(50));
    }

    Ok(TargetInfo {
        id: target_id.clone(),
        target_type: "page".to_string(),
        title: String::new(),
        url: target_url.to_string(),
        ws_url: format!("ws://127.0.0.1:{port}/devtools/page/{target_id}"),
    })
}

pub fn close_tab(port: u16, target_id: &str) -> Result<(), String> {
    let path = format!("/json/close/{target_id}");
    match http_request(port, "GET", &path, Duration::from_secs(3)) {
        Ok(_) => {}
        Err(err) => {
            let version = fetch_version(port)?;
            let ws_path = ws_path_from_url(&version.ws_url)?;
            let mut cdp = CdpClient::connect(port, &ws_path)?;
            cdp.call("Target.closeTarget", json!({ "targetId": target_id }))
                .map(|_| ())
                .map_err(|e| format!("{err}; fallback failed: {e}"))?;
        }
    }
    for _ in 0..10 {
        match list_targets(port) {
            Ok(targets) if !targets.iter().any(|t| t.id == target_id) => break,
            Ok(_) => sleep(Duration::from_millis(30)),
            Err(_) => break,
        }
    }
    Ok(())
}

pub fn focus_browser_tab(port: u16) -> Result<(), String> {
    let targets = list_targets(port)?;
    let Some(target) = targets
        .iter()
        .find(|t| t.target_type == "page" && t.url.contains("gemini.google.com"))
        .or_else(|| targets.iter().find(|t| t.target_type == "page"))
    else {
        return Ok(());
    };
    let ws_path = if target.ws_url.trim().is_empty() {
        format!("/devtools/page/{}", target.id)
    } else {
        ws_path_from_url(&target.ws_url)?
    };
    let mut cdp = CdpClient::connect(port, &ws_path)?;
    cdp.call("Page.bringToFront", json!({}))?;
    Ok(())
}

fn close_browser_port_in(home: &Path, port: u16) -> Result<(), String> {
    let version = fetch_version(port)?;
    let ws_path = ws_path_from_url(&version.ws_url)?;
    let mut cdp = CdpClient::connect(port, &ws_path)?;
    cdp.set_control(
        crate::agent::CancelToken::new(),
        Instant::now() + Duration::from_secs(3),
    );
    let _ = cdp.call("Browser.close", json!({}));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !is_port_alive(port) {
            remove_running_meta_in(home, port);
            sleep(Duration::from_millis(250));
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    remove_running_meta_in(home, port);
    Ok(())
}

pub fn close_browser_port(port: u16) -> Result<(), String> {
    close_browser_port_in(&anticrumb_home(), port)
}

#[cfg(unix)]
fn pid_matches_browser_port(port: u16, pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    if !alive {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        linux_pid_matches_cdp_port(port, pid).is_some()
    }
    #[cfg(target_os = "macos")]
    {
        inspect_native_cdp(port).is_some_and(|m| m.pid == pid)
    }
}

#[cfg(unix)]
fn terminate_browser_pid_if_matching(port: u16, pid: u32) {
    if !pid_matches_browser_port(port, pid) {
        return;
    }
    // If CDP already closed, give the browser a brief moment to finish flushing and exit.
    if !is_port_alive(port) {
        let grace = Instant::now() + Duration::from_secs(1);
        while Instant::now() < grace {
            if !pid_matches_browser_port(port, pid) {
                return;
            }
            sleep(Duration::from_millis(50));
        }
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !pid_matches_browser_port(port, pid) {
            return;
        }
        sleep(Duration::from_millis(50));
    }
    if pid_matches_browser_port(port, pid) {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
fn terminate_browser_pid_if_matching(port: u16, pid: u32) {
    if pid == 0 || !is_port_alive(port) {
        return;
    }
    if inspect_native_cdp(port).is_some_and(|m| m.pid == pid) {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .status();
    }
}

fn graceful_shutdown_browser_in(home: &Path, port: u16, pid: u32, profile_dir: Option<&Path>) {
    let _ = close_browser_port_in(home, port);
    terminate_browser_pid_if_matching(port, pid);
    remove_running_meta_in(home, port);
    if let Some(dir) = profile_dir {
        wait_for_profile_unlock(dir, Duration::from_secs(2));
    }
}

/// Gracefully shuts down any running Chrome instances launched for managed profiles
/// under `anticrumb_home()`, leaving externally attached browsers untouched.
/// Returns the IDs of accounts whose running browser was closed.
pub fn shutdown_managed_browsers(accounts: &[crate::domain::Account]) -> Vec<String> {
    shutdown_managed_browsers_in(&anticrumb_home(), accounts)
}

fn shutdown_managed_browsers_in(home: &Path, accounts: &[crate::domain::Account]) -> Vec<String> {
    if !home.exists() {
        return Vec::new();
    }
    struct ShutdownTarget {
        account_id: Option<String>,
        port: u16,
        pid: u32,
        profile_dir: PathBuf,
    }

    let mut seen_ports = std::collections::HashSet::new();
    let mut targets = Vec::new();

    for account in accounts {
        if account.port == 0 || validate_profile_under(home, &account.profile).is_err() {
            continue;
        }
        let Some(meta) = inspect_running_cdp_in(home, account.port) else {
            continue;
        };
        if !meta.matches_account(account) {
            continue;
        }
        if seen_ports.insert(account.port) {
            targets.push(ShutdownTarget {
                account_id: Some(account.id.clone()),
                port: account.port,
                pid: meta.pid,
                profile_dir: account.profile.clone(),
            });
        }
    }

    if let Ok(entries) = fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("cdp-port-") && name.ends_with(".json")) {
                continue;
            }
            let Ok(raw) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(cached) = serde_json::from_str::<RunningCdpMeta>(&raw) else {
                continue;
            };
            if cached.port == 0 || seen_ports.contains(&cached.port) {
                continue;
            }
            let profile_dir = PathBuf::from(&cached.user_data_dir);
            if validate_profile_under(home, &profile_dir).is_err() {
                continue;
            }
            let Some(live) = inspect_running_cdp_in(home, cached.port) else {
                remove_running_meta_in(home, cached.port);
                continue;
            };
            if !same_dir(Path::new(&live.user_data_dir), &profile_dir)
                || !live.profile_dir.eq_ignore_ascii_case(&cached.profile_dir)
            {
                continue;
            }
            if seen_ports.insert(cached.port) {
                targets.push(ShutdownTarget {
                    account_id: None,
                    port: cached.port,
                    pid: live.pid,
                    profile_dir,
                });
            }
        }
    }

    if targets.is_empty() {
        return Vec::new();
    }

    let closed_ids = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for target in &targets {
            scope.spawn(|| {
                graceful_shutdown_browser_in(
                    home,
                    target.port,
                    target.pid,
                    Some(&target.profile_dir),
                );
                if let Some(id) = &target.account_id {
                    if let Ok(mut ids) = closed_ids.lock() {
                        ids.push(id.clone());
                    }
                }
            });
        }
    });
    closed_ids.into_inner().unwrap_or_default()
}

pub fn wait_for_profile_unlock(profile_dir: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let lock_files = [
        profile_dir.join("SingletonLock"),
        profile_dir.join("lockfile"),
    ];
    while Instant::now() < deadline {
        if !lock_files.iter().any(|p| fs::symlink_metadata(p).is_ok()) {
            break;
        }
        sleep(Duration::from_millis(100));
    }
}

/// Browser lifecycle operations are restricted to managed profiles.
pub fn validate_managed_profile(profile_dir: &Path) -> Result<(), String> {
    let root = default_profile_dir().parent().unwrap().to_path_buf();
    validate_profile_under(&root, profile_dir)
}

fn validate_profile_under(root: &Path, profile_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|e| format!("managed profile root: {e}"))?;
    let root = fs::canonicalize(root).map_err(|e| format!("managed profile root: {e}"))?;
    let mut existing = profile_dir.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or("invalid profile path")?
                .to_owned(),
        );
        if !existing.pop() {
            return Err("invalid profile path".into());
        }
    }
    let mut profile = fs::canonicalize(existing).map_err(|e| e.to_string())?;
    for part in suffix.iter().rev() {
        profile.push(part);
    }
    let relative = profile
        .strip_prefix(&root)
        .map_err(|_| "refusing to manage an external Chrome profile".to_string())?;
    let parts: Vec<_> = relative.components().collect();
    let managed = relative == Path::new("profile")
        || (parts.len() == 3
            && parts[0].as_os_str() == "accounts"
            && matches!(parts[1], std::path::Component::Normal(_))
            && parts[2].as_os_str() == "profile");
    if !managed {
        return Err("refusing to manage a non-managed profile directory".into());
    }
    Ok(())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

pub fn ensure_browser(opts: &LaunchOptions) -> Result<BrowserSession, String> {
    if let Ok(version) = fetch_version(opts.port) {
        let meta = inspect_running_cdp_alive(opts.port)
            .ok_or("cannot verify the profile of the existing CDP browser")?;
        let requested = opts
            .profile_sub_dir
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("Default");
        if !same_dir(Path::new(&meta.user_data_dir), &opts.profile_dir)
            || !meta.profile_dir.eq_ignore_ascii_case(requested)
        {
            return Err("CDP port belongs to a different profile; choose a different port".into());
        }
        return Ok(BrowserSession {
            port: opts.port,
            ws_path: ws_path_from_url(&version.ws_url)?,
            profile_dir: opts.profile_dir.clone(),
            child: None,
            detached: true,
            launched_new: false,
        });
    }
    validate_managed_profile(&opts.profile_dir)?;
    launch_browser(opts)
}

pub fn launch_browser(opts: &LaunchOptions) -> Result<BrowserSession, String> {
    let binary = find_chrome_binary().ok_or_else(|| {
        "could not locate Chrome/Chromium/Edge binary. Install Chrome or set CHROME_BIN."
            .to_string()
    })?;

    if opts.xvfb {
        #[cfg(not(target_os = "linux"))]
        {
            return Err("--xvfb is only supported on Linux".to_string());
        }
    }

    fs::create_dir_all(&opts.profile_dir)
        .map_err(|e| format!("create profile dir '{}': {e}", opts.profile_dir.display()))?;
    wait_for_profile_unlock(&opts.profile_dir, Duration::from_secs(2));

    let port_file = opts.profile_dir.join("DevToolsActivePort");
    let _ = fs::remove_file(&port_file);

    let sub_dir = opts
        .profile_sub_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (win_w, win_h) = opts.window_size.unwrap_or((1920, 1080));

    let mut cmd = if opts.xvfb {
        #[cfg(target_os = "linux")]
        {
            let xvfb = which_xvfb_run().ok_or_else(|| {
                "xvfb-run not found in PATH. Install `xvfb` or omit `--xvfb`.".to_string()
            })?;
            let mut c = Command::new(xvfb);
            c.arg("-a").arg(&binary).arg("--ozone-platform=x11");
            c.env_remove("WAYLAND_DISPLAY")
                .env_remove("X_PRIVILEGED_WAYLAND_SOCKET");
            c
        }
        #[cfg(not(target_os = "linux"))]
        {
            unreachable!()
        }
    } else {
        let mut c = Command::new(&binary);
        if opts.headless {
            c.arg("--headless=new");
            #[cfg(target_os = "linux")]
            {
                c.env_remove("WAYLAND_DISPLAY")
                    .env_remove("X_PRIVILEGED_WAYLAND_SOCKET");
            }
        }
        c
    };

    cmd.arg(format!("--window-size={win_w},{win_h}"))
        .arg(format!("--remote-debugging-port={}", opts.port))
        .arg(format!("--user-data-dir={}", opts.profile_dir.display()));
    if let Some(sub) = sub_dir {
        cmd.arg(format!("--profile-directory={sub}"));
    }
    cmd.arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(&opts.initial_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if opts.detached {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NEW_PROCESS_GROUP (0x200) | DETACHED_PROCESS (0x8)
            cmd.creation_flags(0x0000_0200 | 0x0000_0008);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("launch browser ({}): {e}", binary.display()))?;
    let pid = child.id();

    let mode_label = if opts.xvfb {
        "xvfb"
    } else if opts.headless {
        "headless"
    } else {
        "visible"
    }
    .to_string();

    let start = Instant::now();
    let timeout = Duration::from_secs(15);
    while start.elapsed() < timeout {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "browser exited immediately with status {status}. Check if another Chrome instance is locking '{}'.",
                opts.profile_dir.display()
            ));
        }

        if opts.port != 0 {
            if let Ok(ver) = fetch_version(opts.port) {
                if let Ok(ws_path) = ws_path_from_url(&ver.ws_url) {
                    save_running_meta(&RunningCdpMeta {
                        port: opts.port,
                        user_data_dir: opts.profile_dir.to_string_lossy().to_string(),
                        profile_dir: sub_dir.unwrap_or("Default").to_string(),
                        mode: mode_label,
                        headless: opts.headless && !opts.xvfb,
                        xvfb: opts.xvfb,
                        pid,
                    });
                    return Ok(BrowserSession {
                        port: opts.port,
                        ws_path,
                        profile_dir: opts.profile_dir.clone(),
                        child: Some(child),
                        detached: opts.detached,
                        launched_new: true,
                    });
                }
            }
        }

        if let Ok(content) = fs::read_to_string(&port_file) {
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() >= 2 {
                if let Ok(port) = lines[0].trim().parse::<u16>() {
                    let ws_path = lines[1].trim().to_string();
                    if is_port_alive(port) {
                        save_running_meta(&RunningCdpMeta {
                            port,
                            user_data_dir: opts.profile_dir.to_string_lossy().to_string(),
                            profile_dir: sub_dir.unwrap_or("Default").to_string(),
                            mode: mode_label,
                            headless: opts.headless && !opts.xvfb,
                            xvfb: opts.xvfb,
                            pid,
                        });
                        return Ok(BrowserSession {
                            port,
                            ws_path,
                            profile_dir: opts.profile_dir.clone(),
                            child: Some(child),
                            detached: opts.detached,
                            launched_new: true,
                        });
                    }
                }
            }
        }

        sleep(Duration::from_millis(100));
    }

    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "timed out waiting for Chrome DevTools on port {} (profile: {})",
        opts.port,
        opts.profile_dir.display()
    ))
}

pub fn ws_path_from_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('/') {
        return Ok(trimmed.to_string());
    }
    let parsed = Url::parse(trimmed).map_err(|e| format!("invalid ws url '{trimmed}': {e}"))?;
    let mut path = parsed.path().to_string();
    if let Some(q) = parsed.query() {
        path.push('?');
        path.push_str(q);
    }
    Ok(path)
}

fn http_request(port: u16, method: &str, path: &str, timeout: Duration) -> Result<String, String> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("invalid address: {e}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))
        .map_err(|e| format!("connect 127.0.0.1:{port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write HTTP request: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush HTTP request: {e}"))?;

    read_http_response(&mut stream)
}

struct HttpHead {
    status_code: u16,
    content_length: Option<usize>,
    is_chunked: bool,
}

fn parse_http_head(raw: &[u8]) -> HttpHead {
    let head = String::from_utf8_lossy(raw);
    let status_line = head.lines().next().unwrap_or_default();
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "content-length" {
                content_length = val.parse().ok();
            } else if key == "transfer-encoding" && val.to_ascii_lowercase().contains("chunked") {
                is_chunked = true;
            }
        }
    }

    HttpHead {
        status_code,
        content_length,
        is_chunked,
    }
}

fn finalize_http_body(status_code: u16, is_chunked: bool, body: &[u8]) -> Result<String, String> {
    let decoded = if is_chunked {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };
    let text = String::from_utf8_lossy(&decoded).to_string();

    if !(200..300).contains(&status_code) {
        return Err(format!("HTTP {status_code}: {text}"));
    }
    Ok(text)
}

fn read_http_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream
            .read(&mut chunk)
            .map_err(|e| format!("read HTTP headers: {e}"))?;
        if n == 0 {
            return Err("malformed HTTP response (EOF before headers)".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 65536 {
            return Err("HTTP response headers too large".to_string());
        }
    };

    let head = parse_http_head(&buf[..header_end]);
    let mut body = buf[header_end + 4..].to_vec();
    if head.is_chunked {
        while !is_chunked_complete(&body) {
            let n = stream
                .read(&mut chunk)
                .map_err(|e| format!("read chunked HTTP body: {e}"))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(len) = head.content_length {
        while body.len() < len {
            let need = len - body.len();
            let to_read = need.min(chunk.len());
            let n = stream
                .read(&mut chunk[..to_read])
                .map_err(|e| format!("read HTTP body: {e}"))?;
            if n == 0 {
                return Err(format!(
                    "truncated HTTP body (expected {len} bytes, got {})",
                    body.len()
                ));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    } else {
        stream
            .read_to_end(&mut body)
            .map_err(|e| format!("read HTTP response: {e}"))?;
    }

    finalize_http_body(head.status_code, head.is_chunked, &body)
}

fn is_chunked_complete(mut data: &[u8]) -> bool {
    loop {
        let Some(pos) = data.windows(2).position(|w| w == b"\r\n") else {
            return false;
        };
        let len_line = std::str::from_utf8(&data[..pos])
            .ok()
            .and_then(|s| s.split(';').next())
            .unwrap_or("")
            .trim();
        let Ok(size) = usize::from_str_radix(len_line, 16) else {
            return false;
        };
        data = &data[pos + 2..];
        if size == 0 {
            return data.windows(2).any(|w| w == b"\r\n") || data.is_empty();
        }
        if data.len() < size + 2 {
            return false;
        }
        data = &data[size + 2..];
    }
}

pub fn parse_http_response(raw: &[u8]) -> Result<String, String> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response (missing header separator)".to_string())?;
    let head = parse_http_head(&raw[..sep]);
    finalize_http_body(head.status_code, head.is_chunked, &raw[sep + 4..])
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(pos) = data.windows(2).position(|w| w == b"\r\n") {
        let len_line = std::str::from_utf8(&data[..pos])
            .map_err(|e| format!("chunk size utf8: {e}"))?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(len_line, 16)
            .map_err(|e| format!("invalid chunk size '{len_line}': {e}"))?;
        data = &data[pos + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            return Err("truncated chunked body".to_string());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    Ok(out)
}

pub use crate::cdp::CdpClient;

#[cfg(test)]
pub(crate) struct MockHttpServer {
    pub port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl MockHttpServer {
    pub fn spawn(mut handler: impl FnMut(TcpStream, &str, u16) + Send + 'static) -> Self {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut req = [0u8; 2048];
                    let n = stream.read(&mut req).unwrap_or(0);
                    let req_str = String::from_utf8_lossy(&req[..n]).into_owned();
                    handler(stream, &req_str, port);
                } else {
                    sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            port,
            stop,
            thread: Some(thread),
        }
    }

    pub fn write_json(mut stream: TcpStream, body: &str) {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    }

    pub fn accept_ws(
        mut stream: TcpStream,
        req_str: &str,
    ) -> tokio_tungstenite::tungstenite::protocol::WebSocket<TcpStream> {
        let key = req_str
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| v.trim())
            })
            .unwrap_or("");
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let hs = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        let _ = stream.write_all(hs.as_bytes());
        tokio_tungstenite::tungstenite::protocol::WebSocket::from_raw_socket(
            stream,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
    }
}

#[cfg(test)]
impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
struct WsFrame {
    opcode: u8,
    payload: Vec<u8>,
}

#[cfg(test)]
fn read_ws_frame(stream: &mut TcpStream) -> std::io::Result<WsFrame> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header)?;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut len = (header[1] & 0x7F) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext)?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext)?;
        len = u64::from_be_bytes(ext) as usize;
    }
    let mask = if masked {
        let mut m = [0u8; 4];
        stream.read_exact(&mut m)?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(WsFrame { opcode, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn test_ws_path_from_url() {
        assert_eq!(
            ws_path_from_url("ws://127.0.0.1:9222/devtools/page/ABC").unwrap(),
            "/devtools/page/ABC"
        );
        assert_eq!(
            ws_path_from_url("/devtools/browser/123").unwrap(),
            "/devtools/browser/123"
        );
    }

    #[test]
    fn test_parse_http_response_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_http_response(plain).unwrap(), "hello");

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(chunked).unwrap(), "hello world");

        let err_resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\nnope";
        assert!(parse_http_response(err_resp)
            .unwrap_err()
            .contains("HTTP 404"));
    }

    #[test]
    fn test_cdp_client_handles_events_pings_and_fragmentation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            while !req.ends_with(b"\r\n\r\n") {
                let mut b = [0u8; 1];
                stream.read_exact(&mut b).unwrap();
                req.push(b[0]);
            }
            let request = String::from_utf8(req).unwrap();
            let key = request
                .lines()
                .find_map(|line| {
                    let (name, val) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| val.trim())
                })
                .unwrap();
            let accept =
                tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
            write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();

            // Read client's CDP request
            let req_frame = read_ws_frame(&mut stream).unwrap();
            let req_json: Value = serde_json::from_slice(&req_frame.payload).unwrap();
            let id = req_json["id"].as_u64().unwrap();

            // 1. Send an async CDP event (no id)
            let ev = br#"{"method":"Runtime.consoleAPICalled","params":{}}"#;
            let mut f1 = vec![0x81, ev.len() as u8];
            f1.extend_from_slice(ev);
            stream.write_all(&f1).unwrap();

            // 2. Send a WebSocket Ping
            let mut ping = vec![0x89, 2];
            ping.extend_from_slice(b"hi");
            stream.write_all(&ping).unwrap();

            // Expect Pong back
            let pong = read_ws_frame(&mut stream).unwrap();
            assert_eq!(pong.opcode, 0xA);
            assert_eq!(pong.payload, b"hi");

            // 3. Send fragmented response matching id
            let full = format!(r#"{{"id":{id},"result":{{"ok":true}}}}"#);
            let split = full.len() / 2;
            let part1 = &full.as_bytes()[..split];
            let part2 = &full.as_bytes()[split..];

            let mut frag1 = vec![0x01, part1.len() as u8]; // FIN=0, opcode=text
            frag1.extend_from_slice(part1);
            stream.write_all(&frag1).unwrap();

            let mut frag2 = vec![0x80, part2.len() as u8]; // FIN=1, opcode=continuation
            frag2.extend_from_slice(part2);
            stream.write_all(&frag2).unwrap();
        });

        let mut client = CdpClient::connect(port, "/devtools/page/test").unwrap();
        let res = client
            .call("Runtime.evaluate", json!({"expression": "1+1"}))
            .unwrap();
        assert_eq!(res["ok"], json!(true));
        worker.join().unwrap();
    }

    #[test]
    fn test_http_request_does_not_wait_for_eof_when_content_length_present() {
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel::<()>();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 512];
            let _ = stream.read(&mut req_buf);
            // Intentionally omit space after Content-Length: and keep socket open
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length:11\r\n\r\nhello world")
                .unwrap();
            stream.flush().unwrap();
            // Hold socket open until client finishes reading
            let _ = rx.recv_timeout(Duration::from_secs(2));
        });

        let start = Instant::now();
        let body = http_request(port, "GET", "/json/version", Duration::from_millis(900)).unwrap();
        assert_eq!(body, "hello world");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "http_request waited for EOF instead of honoring Content-Length"
        );
        let _ = tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn test_parse_linux_cmdline_and_cdp_meta() {
        // 1. Live Chrome SetProcessTitle format (single NUL, space-joined argv)
        let title_bytes = b"/opt/google/chrome/chrome --remote-debugging-port=9222 --user-data-dir=/home/test-user/.anticrumb/profile --no-first-run --no-default-browser-check https://gemini.google.com/app\0";
        let args = parse_linux_cmdline_bytes(title_bytes);
        let meta = parse_browser_args(9222, 1234, &args).unwrap();
        assert_eq!(meta.port, 9222);
        assert_eq!(meta.pid, 1234);
        assert_eq!(meta.user_data_dir, "/home/test-user/.anticrumb/profile");
        assert_eq!(meta.profile_dir, "Default");
        assert_eq!(meta.mode, "visible");
        assert!(!meta.headless);
        assert!(!meta.xvfb);

        // 2. Single-NUL cmdline with spaces inside binary, user-data-dir, and profile-directory
        let spaced_bytes = b"/opt/My Chrome/chrome --headless=new --remote-debugging-port=9333 --user-data-dir=/home/test-user/My Data --profile-directory=Profile 2 --no-first-run about:blank\0";
        let spaced_args = parse_linux_cmdline_bytes(spaced_bytes);
        assert_eq!(spaced_args[0], "/opt/My Chrome/chrome");
        let meta2 = parse_browser_args(9333, 42, &spaced_args).unwrap();
        assert_eq!(meta2.user_data_dir, "/home/test-user/My Data");
        assert_eq!(meta2.profile_dir, "Profile 2");
        assert_eq!(meta2.mode, "headless");
        assert!(meta2.headless);

        // 3. Standard kernel NUL-separated argv with Xvfb
        let nul_bytes = b"/usr/bin/chromium\0--ozone-platform=x11\0--remote-debugging-port=9444\0--user-data-dir=/tmp/prof\0--profile-directory=Profile 1\0";
        let nul_args = parse_linux_cmdline_bytes(nul_bytes);
        let meta3 = parse_browser_args(9444, 77, &nul_args).unwrap();
        assert_eq!(meta3.mode, "xvfb");
        assert!(meta3.xvfb);
        assert!(!meta3.headless);
        assert_eq!(meta3.profile_dir, "Profile 1");

        // 4. Child helper processes (`--type=renderer`) and mismatched ports must be ignored
        let renderer = b"/opt/google/chrome/chrome --type=renderer --remote-debugging-port=9222\0";
        assert!(parse_browser_args(9222, 99, &parse_linux_cmdline_bytes(renderer)).is_none());
        assert!(parse_browser_args(9999, 1234, &args).is_none());
    }
    #[test]
    fn managed_profile_validation_allows_new_profiles_but_rejects_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            validate_profile_under(root.path(), &root.path().join("accounts/new/profile")).is_ok()
        );
        assert!(validate_profile_under(root.path(), &root.path().join("unrelated")).is_err());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join("accounts")).unwrap();
            assert!(
                validate_profile_under(root.path(), &root.path().join("accounts/new/profile"))
                    .is_err()
            );
        }
    }

    #[test]
    fn shutdown_managed_browsers_closes_only_matching_managed_profiles() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let home = tempfile::tempdir().unwrap();
        let managed_profile = home.path().join("accounts/work-claude/profile");
        fs::create_dir_all(&managed_profile).unwrap();
        let external_dir = tempfile::tempdir().unwrap();

        let managed_closed = Arc::new(AtomicBool::new(false));
        let managed_alive = Arc::new(AtomicBool::new(true));
        let managed_closed_srv = managed_closed.clone();
        let managed_alive_srv = managed_alive.clone();
        let managed_srv = MockHttpServer::spawn(move |stream, req_str, port| {
            if !managed_alive_srv.load(Ordering::SeqCst) {
                return;
            }
            if req_str.starts_with("GET /json/version ") {
                let body = json!({
                    "Browser": "Chrome/130.0.0.0",
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/test")
                })
                .to_string();
                MockHttpServer::write_json(stream, &body);
            } else if req_str.contains("/devtools/browser/test") {
                let mut ws = MockHttpServer::accept_ws(stream, req_str);
                if let Ok(tokio_tungstenite::tungstenite::Message::Text(txt)) = ws.read() {
                    let req: Value = serde_json::from_str(&txt).unwrap();
                    if req["method"] == "Browser.close" {
                        managed_closed_srv.store(true, Ordering::SeqCst);
                        let _ = ws.send(tokio_tungstenite::tungstenite::Message::Text(
                            json!({"id": req["id"], "result": {}}).to_string().into(),
                        ));
                        managed_alive_srv.store(false, Ordering::SeqCst);
                    }
                }
            }
        });

        let external_closed = Arc::new(AtomicBool::new(false));
        let external_closed_srv = external_closed.clone();
        let external_srv = MockHttpServer::spawn(move |stream, req_str, port| {
            if req_str.starts_with("GET /json/version ") {
                let body = json!({
                    "Browser": "Chrome/130.0.0.0",
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/ext")
                })
                .to_string();
                MockHttpServer::write_json(stream, &body);
            } else if req_str.contains("/devtools/browser/ext") {
                external_closed_srv.store(true, Ordering::SeqCst);
                let _ = MockHttpServer::accept_ws(stream, req_str);
            }
        });

        let managed_meta = RunningCdpMeta {
            port: managed_srv.port,
            user_data_dir: managed_profile.to_string_lossy().into_owned(),
            profile_dir: "Default".into(),
            mode: "headless".into(),
            headless: true,
            xvfb: false,
            pid: 0,
        };
        fs::write(
            cdp_meta_path_in(home.path(), managed_srv.port),
            serde_json::to_string_pretty(&managed_meta).unwrap(),
        )
        .unwrap();

        let accounts = vec![
            crate::domain::Account {
                id: "work-claude".into(),
                provider: "claude-web".into(),
                profile: managed_profile,
                subprofile: None,
                port: managed_srv.port,
                headless: true,
                state: "ready".into(),
            },
            crate::domain::Account {
                id: "attached".into(),
                provider: "gemini-web".into(),
                profile: external_dir.path().to_path_buf(),
                subprofile: None,
                port: external_srv.port,
                headless: false,
                state: "ready".into(),
            },
        ];

        let closed = shutdown_managed_browsers_in(home.path(), &accounts);
        assert_eq!(closed, vec!["work-claude".to_string()]);
        assert!(managed_closed.load(Ordering::SeqCst));
        assert!(!external_closed.load(Ordering::SeqCst));
        assert!(!cdp_meta_path_in(home.path(), managed_srv.port).exists());
    }
}
