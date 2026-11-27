use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::cancel::CancelToken;
use super::events::{OutputStream, ToolCallRequest, ToolOutput, DEFAULT_SHELL_TIMEOUT_SECS};
use super::process::{format_report, BackgroundProcesses, CapturedProcess};

const MAX_TOOL_OUTPUT_CHARS: usize = 12_000;
const MAX_WEB_TOOL_OUTPUT_CHARS: usize = 16_000;
const SHELL_STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const SHELL_STREAM_COALESCE_BYTES: usize = 4 * 1024;
const SHELL_STREAM_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_READ_LINES: usize = 500;
const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GREP_FILES: usize = 2000;
const MAX_GREP_LINE_CHARS: usize = 300;
const MAX_SHELL_TIMEOUT_SECS: u64 = 600;
const MAX_PROCESS_WAIT_SECS: u64 = 60;
const MAX_TEXT_FILE_BYTES: u64 = 16 * 1024 * 1024;

fn read_text(path: &Path) -> Result<String, String> {
    if !fs::metadata(path).map_err(|e| e.to_string())?.is_file() {
        return Err("text tools require a regular file".into());
    }
    let file = fs::File::open(path).map_err(|e| format!("read '{}': {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > MAX_TEXT_FILE_BYTES {
        return Err("text tools require a regular UTF-8 file of at most 16 MiB".into());
    }
    let mut text = String::new();
    file.take(MAX_TEXT_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_TEXT_FILE_BYTES {
        return Err("file grew beyond 16 MiB".into());
    }
    Ok(text)
}

// Replace atomically within the same directory, preserving existing file permissions.
// The content check catches concurrent edits; the workspace lease serializes our agents.
fn write_text(path: &Path, content: &str, expected: Option<&str>) -> Result<(), String> {
    if content.len() as u64 > MAX_TEXT_FILE_BYTES {
        return Err("output exceeds 16 MiB".into());
    }
    let path = if path.exists() {
        fs::canonicalize(path).map_err(|e| e.to_string())?
    } else {
        path.to_path_buf()
    };
    let parent = path.parent().ok_or("missing parent directory")?;
    let temp = parent.join(format!(".gemini-write-{}", uuid::Uuid::new_v4().simple()));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(temp.clone());
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    if let Ok(meta) = fs::metadata(&path) {
        out.set_permissions(meta.permissions())
            .map_err(|e| e.to_string())?;
    }
    out.write_all(content.as_bytes())
        .and_then(|_| out.sync_all())
        .map_err(|e| e.to_string())?;
    match expected {
        Some(old) if read_text(&path)? != old => {
            return Err("file changed during edit; read it again".into())
        }
        None if path.exists() => return Err("file appeared during creation; read it first".into()),
        _ => {}
    }
    drop(out);
    fs::rename(&temp, &path).map_err(|e| e.to_string())?;
    // Windows cannot FlushFileBuffers on a read-only directory handle. The
    // replacement file has already been synced above; directory fsync is Unix-only.
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ============================================================================
// 1. Context & sub-query runner
// ============================================================================

/// Callback invoked by `browser_fetch` to navigate a temporary Chrome tab via the active
/// browser runtime (embedded FFI or remote daemon). Parameters: `(url, account_id, wait_ms, max_chars, cancel)`.
pub type BrowserFetchFn = Arc<
    dyn Fn(&str, Option<&str>, u32, usize, &CancelToken) -> Result<String, String> + Send + Sync,
>;

/// Callback invoked by `browser_control` to drive stateful interactive Chrome tabs on the
/// browser host. Parameters: `(args_json, cancel)`.
pub type BrowserControlFn =
    Arc<dyn Fn(&Value, &CancelToken) -> Result<ToolContent, String> + Send + Sync>;

/// Native attachments accompany the next model turn; they are never encoded in prompt text.
#[derive(Default)]
pub struct ToolContent {
    pub text: String,
    pub artifacts: Vec<String>,
}
impl From<String> for ToolContent {
    fn from(text: String) -> Self {
        Self {
            text,
            artifacts: Vec::new(),
        }
    }
}
impl From<&str> for ToolContent {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

#[derive(Debug, Clone)]
pub struct SpawnAgentOutcome {
    pub summary: String,
    pub files_changed: Vec<String>,
}

/// Callback invoked by `spawn_agent` to run an isolated worker (`TempChat`) sub-agent.
/// Parameters: `(label, task, read_only, cancel)`.
pub type SpawnAgentFn =
    Arc<dyn Fn(&str, &str, bool, &CancelToken) -> Result<SpawnAgentOutcome, String> + Send + Sync>;

/// Incremental live-output callback for streaming tools (e.g., `run_shell`).
pub type ToolOutputFn = Arc<dyn Fn(OutputStream, &str) + Send + Sync>;

pub struct ToolContext {
    pub environment: Option<std::collections::BTreeMap<String, String>>,
    pub workspace_root: PathBuf,
    pub shell_timeout: Duration,
    pub allow_shell: bool,
    /// Triggered when the surrounding agent run is cancelled; long tools must honour it.
    pub cancel: CancelToken,
    /// Processes started by `start_process` (killed when the context is dropped).
    pub background: Arc<BackgroundProcesses>,
    /// Optional runtime bridge for `browser_fetch` (Chrome tab fetch via local or remote daemon).
    pub browser_fetch: Option<BrowserFetchFn>,
    /// Optional runtime bridge for `browser_control` (stateful Chrome CDP actions).
    pub browser_control: Option<BrowserControlFn>,
    browser_control_help_ready: Arc<AtomicBool>,
    /// Optional runtime bridge for `spawn_agent` (delegating tasks to the `Worker` `TempChat` tab pool).
    pub spawn_agent: Option<SpawnAgentFn>,
    /// Optional live stdout/stderr streaming callback for long-running tools.
    pub on_output: Option<ToolOutputFn>,
    /// Whether `read_file` should elide repeated reads of unchanged file ranges in persistent sessions.
    pub dedup_reads: bool,
    read_cache: Arc<Mutex<BTreeMap<(PathBuf, usize, usize), String>>>,
    changed_files: Arc<Mutex<BTreeSet<String>>>,
}

impl ToolContext {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            environment: None,
            workspace_root,
            shell_timeout: Duration::from_secs(DEFAULT_SHELL_TIMEOUT_SECS),
            allow_shell: true,
            cancel: CancelToken::new(),
            background: Arc::default(),
            browser_fetch: None,
            browser_control: None,
            browser_control_help_ready: Arc::new(AtomicBool::new(false)),
            spawn_agent: None,
            on_output: None,
            dedup_reads: false,
            read_cache: Arc::new(Mutex::new(BTreeMap::new())),
            changed_files: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// O(1) shallow clone sharing mutable state (`browser_control_help_ready`, `read_cache`,
    /// `changed_files`, `background`, `cancel`) with an attached live-output callback.
    pub fn with_output_callback(&self, cb: ToolOutputFn) -> Self {
        Self {
            environment: self.environment.clone(),
            workspace_root: self.workspace_root.clone(),
            shell_timeout: self.shell_timeout,
            allow_shell: self.allow_shell,
            cancel: self.cancel.clone(),
            background: Arc::clone(&self.background),
            browser_fetch: self.browser_fetch.clone(),
            browser_control: self.browser_control.clone(),
            browser_control_help_ready: Arc::clone(&self.browser_control_help_ready),
            spawn_agent: self.spawn_agent.clone(),
            on_output: Some(cb),
            dedup_reads: self.dedup_reads,
            read_cache: Arc::clone(&self.read_cache),
            changed_files: Arc::clone(&self.changed_files),
        }
    }

    pub(super) fn acknowledge_browser_help(&self) {
        self.browser_control_help_ready
            .store(true, Ordering::Relaxed);
    }

    fn canon_root(&self) -> PathBuf {
        fs::canonicalize(&self.workspace_root).unwrap_or_else(|_| self.workspace_root.clone())
    }

    /// Resolve `raw_path` (relative or absolute) inside the workspace. Rejects `..` escapes **and**
    /// symlinks that lead outside of the workspace root (checked on the deepest existing ancestor,
    /// so paths of files that do not exist yet are validated as well).
    pub fn resolve_workspace_path(&self, raw_path: &str) -> Result<PathBuf, String> {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            return Err("path must not be empty".into());
        }
        let canon_root = self.canon_root();
        let candidate = if Path::new(trimmed).is_absolute() {
            PathBuf::from(trimmed)
        } else {
            canon_root.join(trimmed)
        };

        let normalized = normalize_lexical_path(&candidate);
        let escape = || {
            format!(
                "path '{}' escapes workspace root '{}'",
                raw_path,
                canon_root.display()
            )
        };
        if !normalized.starts_with(&canon_root) {
            return Err(escape());
        }

        let mut existing = normalized.clone();
        while fs::symlink_metadata(&existing).is_err() {
            if !existing.pop() {
                break;
            }
        }
        let real = fs::canonicalize(&existing)
            .map_err(|e| format!("cannot resolve '{}': {e}", existing.display()))?;
        if !real.starts_with(&canon_root) {
            return Err(format!("{} (via symlink)", escape()));
        }
        Ok(normalized)
    }

    fn rel_to_root(&self, path: &Path) -> String {
        path.strip_prefix(self.canon_root())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn record_change(&self, path: &Path) {
        let rel = self.rel_to_root(path);
        if let Ok(mut set) = self.changed_files.lock() {
            set.insert(rel);
        }
        if let Ok(mut cache) = self.read_cache.lock() {
            cache.retain(|(cached_path, _, _), _| cached_path != path);
        }
    }

    pub fn record_relative_changes(&self, rel_paths: &[String]) {
        if let Ok(mut set) = self.changed_files.lock() {
            for rel in rel_paths {
                if !rel.trim().is_empty() {
                    set.insert(rel.clone());
                }
            }
        }
    }

    /// Workspace-relative files touched by `write_file` / `edit_file` so far (sorted).
    pub fn changed_files(&self) -> Vec<String> {
        self.changed_files
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ============================================================================
// 2. Tool trait & registry (with parallel execution of independent calls)
// ============================================================================

pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    /// A tool may reveal its complete interface through a help response instead.
    /// Execution always validates against parameters_schema.
    fn initial_parameters_schema(&self) -> Value {
        self.parameters_schema()
    }
    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String>;
    fn execute_content(&self, args: &Value, ctx: &ToolContext) -> Result<ToolContent, String> {
        self.execute(args, ctx).map(Into::into)
    }
    /// Read-only tools without ordering requirements run concurrently when the model emits
    /// several of them in one reply. Mutating tools always run sequentially in order.
    fn is_read_only(&self) -> bool {
        false
    }
}

pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::with_builtin_coding_tools()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn with_builtin_coding_tools() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(FindFilesTool));
        reg.register(Box::new(GrepSearchTool));
        reg.register(Box::new(ReadFileTool));
        #[cfg(feature = "tree-sitter")]
        reg.register(Box::new(CodeOutlineTool));
        #[cfg(feature = "tree-sitter")]
        reg.register(Box::new(FindSymbolTool));
        #[cfg(feature = "tree-sitter")]
        reg.register(Box::new(FindReferencesTool));
        reg.register(Box::new(EditFileTool));
        reg.register(Box::new(WriteFileTool));
        reg.register(Box::new(ShellTool));
        reg.register(Box::new(StartProcessTool));
        reg.register(Box::new(ProcessOutputTool));
        reg.register(Box::new(StopProcessTool));
        reg.register(Box::new(WebFetchTool));
        reg.register(Box::new(BrowserFetchTool));
        reg.register(Box::new(BrowserControlTool));
        reg.register(Box::new(SpawnAgentTool));
        reg
    }

    pub fn register(&mut self, tool: Box<dyn AgentTool>) {
        self.tools.insert(tool.name().to_string(), Arc::from(tool));
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn list_schemas(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters_schema(),
                })
            })
            .collect()
    }

    pub fn list_schemas_with_overrides(&self, overrides: &BTreeMap<String, String>) -> Vec<Value> {
        self.schemas_with_overrides(overrides, false)
    }

    pub fn list_initial_schemas_with_overrides(
        &self,
        overrides: &BTreeMap<String, String>,
    ) -> Vec<Value> {
        self.schemas_with_overrides(overrides, true)
    }

    pub fn render_compact_tools_block(&self, overrides: &BTreeMap<String, String>) -> String {
        self.list_initial_schemas_with_overrides(overrides)
            .iter()
            .map(|t| {
                let name = t["name"].as_str().unwrap_or_default();
                let desc = t["description"].as_str().unwrap_or_default();
                format_tool_signature(name, desc, &t["parameters"])
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn schemas_with_overrides(
        &self,
        overrides: &BTreeMap<String, String>,
        initial: bool,
    ) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                let name = t.name();
                let desc = overrides
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or_else(|| t.description());
                json!({
                    "name": name,
                    "description": desc,
                    "parameters": if initial { t.initial_parameters_schema() } else { t.parameters_schema() },
                })
            })
            .collect()
    }

    pub fn execute_call(&self, call: &ToolCallRequest, ctx: &ToolContext) -> ToolOutput {
        let started = Instant::now();
        let make = |ok: bool, output: String| ToolOutput {
            tool: call.tool.clone(),
            ok,
            output,
            artifacts: Vec::new(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };

        let Some(tool) = self.tools.get(&call.tool) else {
            return make(
                false,
                format!(
                    "unknown tool '{}'. Available tools: {}",
                    call.tool,
                    self.tools.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            );
        };
        if ctx.cancel.is_cancelled() {
            return make(false, "error: cancelled before execution".into());
        }
        match crate::schema::is_valid(&tool.parameters_schema(), &call.args) {
            Ok(true) => {}
            _ => {
                return make(
                    false,
                    "error: arguments do not match the declared tool schema".into(),
                )
            }
        }

        // A panicking tool must never take the whole agent thread down.
        let max_output_chars = match call.tool.as_str() {
            "web_fetch" | "browser_fetch" | "browser_control" => MAX_WEB_TOOL_OUTPUT_CHARS,
            _ => MAX_TOOL_OUTPUT_CHARS,
        };
        let res = catch_unwind(AssertUnwindSafe(|| tool.execute_content(&call.args, ctx)))
            .unwrap_or_else(|_| Err("tool panicked".to_string()));
        match res {
            Ok(raw) => {
                let mut out = make(true, truncate_tool_output(&raw.text, max_output_chars));
                out.artifacts = raw.artifacts;
                out
            }
            Err(err) => make(
                false,
                format!("error: {}", truncate_tool_output(&err, max_output_chars)),
            ),
        }
    }

    fn is_read_only(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|t| t.is_read_only())
            .unwrap_or(false)
    }

    /// Execute all calls of one model reply. Consecutive read-only calls run concurrently,
    /// everything else runs sequentially in the order the model wrote it. Results keep call order.
    /// `on_start` / `on_output` / `on_done` fire in strict causal order per call.
    pub fn execute_batch(
        &self,
        calls: &[ToolCallRequest],
        ctx: &ToolContext,
        on_start: &(dyn Fn(usize, &ToolCallRequest) + Sync),
        on_output: &(dyn Fn(usize, &ToolCallRequest, OutputStream, &str) + Sync),
        on_done: &(dyn Fn(usize, &ToolCallRequest, &ToolOutput) + Sync),
    ) -> Vec<ToolOutput> {
        enum BatchMsg {
            Start(usize),
            Output(usize, OutputStream, String),
            Done(usize, ToolOutput),
        }

        let mut results: Vec<Option<ToolOutput>> = vec![None; calls.len()];
        let mut idx = 0;
        while idx < calls.len() {
            let mut end = idx + 1;
            if self.is_read_only(&calls[idx].tool) {
                while end < calls.len() && self.is_read_only(&calls[end].tool) {
                    end += 1;
                }
            }
            let (tx, rx) = std::sync::mpsc::channel::<BatchMsg>();
            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(end - idx);
                for i in idx..end {
                    let call = &calls[i];
                    let worker_tx = tx.clone();
                    let existing_cb = ctx.on_output.clone();
                    let out_tx = worker_tx.clone();
                    let call_ctx = ctx.with_output_callback(Arc::new(move |stream, text| {
                        if let Some(prev) = &existing_cb {
                            prev(stream, text);
                        }
                        let _ = out_tx.send(BatchMsg::Output(i, stream, text.to_owned()));
                    }));
                    handles.push((
                        i,
                        s.spawn(move || {
                            let _ = worker_tx.send(BatchMsg::Start(i));
                            let out = self.execute_call(call, &call_ctx);
                            let _ = worker_tx.send(BatchMsg::Done(i, out));
                        }),
                    ));
                }
                drop(tx);
                for msg in rx {
                    match msg {
                        BatchMsg::Start(i) => on_start(i, &calls[i]),
                        BatchMsg::Output(i, stream, text) => {
                            on_output(i, &calls[i], stream, &text);
                        }
                        BatchMsg::Done(i, out) => {
                            on_done(i, &calls[i], &out);
                            results[i] = Some(out);
                        }
                    }
                }
                for (i, handle) in handles {
                    if handle.join().is_err() && results[i].is_none() {
                        let out = ToolOutput {
                            tool: calls[i].tool.clone(),
                            ok: false,
                            output: "error: tool worker thread panicked".into(),
                            artifacts: Vec::new(),
                            elapsed_ms: 0,
                        };
                        on_done(i, &calls[i], &out);
                        results[i] = Some(out);
                    }
                }
            });
            idx = end;
        }
        results.into_iter().flatten().collect()
    }
}

/// Keep the head and the tail of oversized output (errors usually sit at the end).
pub fn truncate_tool_output(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let head_n = max_chars * 6 / 10;
    let tail_n = max_chars - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().skip(count - tail_n).collect();
    format!(
        "{head}\n... [truncated {} chars] ...\n{tail}",
        count - max_chars
    )
}

pub fn format_tool_signature(name: &str, description: &str, schema: &Value) -> String {
    let mut params_str = String::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        let mut ordered_params: Vec<(&str, &Value, bool)> = Vec::new();
        let mut seen = BTreeSet::new();

        if let Some(req_arr) = schema.get("required").and_then(Value::as_array) {
            for req_val in req_arr {
                if let Some(req_name) = req_val.as_str() {
                    if let Some(prop_schema) = props.get(req_name) {
                        ordered_params.push((req_name, prop_schema, true));
                        seen.insert(req_name);
                    }
                }
            }
        }

        let mut optional_params: Vec<(&str, &Value)> = Vec::new();
        for (prop_name, prop_schema) in props {
            if !seen.contains(prop_name.as_str()) {
                optional_params.push((prop_name.as_str(), prop_schema));
            }
        }
        optional_params.sort_by_key(|&(k, _)| {
            let priority = match k {
                "pattern" | "query" => 0,
                "path" => 1,
                "start_line" => 2,
                "end_line" => 3,
                _ => 10,
            };
            (priority, k)
        });

        for (opt_name, opt_schema) in optional_params {
            ordered_params.push((opt_name, opt_schema, false));
        }

        let formatted_args: Vec<String> = ordered_params
            .into_iter()
            .map(|(p_name, p_schema, is_req)| {
                let type_str = format_param_type(p_schema);
                if is_req {
                    format!("{p_name}: {type_str}")
                } else {
                    format!("{p_name}?: {type_str}")
                }
            })
            .collect();

        params_str = formatted_args.join(", ");
    }

    let clean_desc = description.split_whitespace().collect::<Vec<_>>().join(" ");

    if clean_desc.is_empty() {
        format!("- {name}({params_str})")
    } else {
        format!("- {name}({params_str}) \u{2014} {clean_desc}")
    }
}

fn format_param_type(schema: &Value) -> String {
    if let Some(const_val) = schema.get("const") {
        if let Some(s) = const_val.as_str() {
            return format!("\"{s}\"");
        }
        return const_val.to_string();
    }

    if let Some(enum_vals) = schema.get("enum").and_then(Value::as_array) {
        if !enum_vals.is_empty() {
            return enum_vals
                .iter()
                .map(|v| {
                    if let Some(s) = v.as_str() {
                        format!("\"{s}\"")
                    } else {
                        v.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("|");
        }
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "str".to_string(),
        Some("integer") => "int".to_string(),
        Some("number") => "num".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("object") => "obj".to_string(),
        Some("array") => {
            if let Some(items) = schema.get("items") {
                let inner = format_param_type(items);
                format!("list[{inner}]")
            } else {
                "list".to_string()
            }
        }
        _ => "any".to_string(),
    }
}

// ============================================================================
// 3. Built-in coding tools
// ============================================================================

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | ".idea" | ".vscode" | "__pycache__" | ".next" | "dist"
    )
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let t = text.to_ascii_lowercase();
    if p.is_empty() || p == "*" {
        return true;
    }
    if !p.contains('*') && !p.contains('?') {
        return t.contains(&p);
    }
    let p_bytes: Vec<char> = p.chars().collect();
    let t_bytes: Vec<char> = t.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);

    while ti < t_bytes.len() {
        if pi < p_bytes.len() && (p_bytes[pi] == '?' || p_bytes[pi] == t_bytes[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p_bytes.len() && p_bytes[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p_bytes.len() && p_bytes[pi] == '*' {
        pi += 1;
    }
    pi == p_bytes.len()
}

/// Walks `dir` like ripgrep `--hidden`: honors `.gitignore` (also outside git repos and from
/// parent directories), `.ignore`, `.git/info/exclude` and the global git excludes file;
/// includes dotfiles except `.git`. `is_ignored_dir` stays as a safety net for repos that do
/// not ignore build output. Results are in sorted (deterministic) depth-first order.
fn collect_workspace_files(
    root: &Path,
    dir: &Path,
    pattern: &str,
    max_results: usize,
    cancel: &CancelToken,
    out: &mut Vec<PathBuf>,
) {
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && is_ignored_dir(&e.file_name().to_string_lossy()))
        })
        .build();
    for entry in walker.flatten() {
        if out.len() >= max_results || cancel.is_cancelled() {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let fname = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if wildcard_match(pattern, &rel) || wildcard_match(pattern, &fname) {
            out.push(path);
        }
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string parameter '{key}'"))
}

/// Tool: `find_files`
pub struct FindFilesTool;

impl AgentTool for FindFilesTool {
    fn name(&self) -> &'static str {
        "find_files"
    }

    fn description(&self) -> &'static str {
        "Find files by glob or substring."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob or substring pattern to match file paths (default '*')." },
                "dir": { "type": "string", "description": "Optional subdirectory inside workspace." },
                "limit": { "type": "integer", "description": "Max files to return (default 100)." }
            }
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or("*")
            .trim();
        let sub_dir = args.get("dir").and_then(Value::as_str).unwrap_or(".");
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;

        let start_dir = ctx.resolve_workspace_path(sub_dir)?;
        let canon_root = ctx.canon_root();
        let mut matches = Vec::new();
        collect_workspace_files(
            &canon_root,
            &start_dir,
            pattern,
            limit,
            &ctx.cancel,
            &mut matches,
        );

        if matches.is_empty() {
            return Ok(format!("No files matched pattern '{pattern}'."));
        }
        let lines: Vec<String> = matches.iter().map(|p| ctx.rel_to_root(p)).collect();
        Ok(format!(
            "Found {} file(s):\n{}",
            lines.len(),
            lines.join("\n")
        ))
    }
}

/// Tool: `grep_search` (regex by default, ripgrep-compatible syntax)
pub struct GrepSearchTool;

#[derive(Clone, Copy, PartialEq, Eq)]
enum GrepMode {
    Content,
    Files,
    Count,
}

/// ripgrep `--smart-case`: case-sensitive only when the pattern contains an uppercase
/// literal. Escapes (`\S`, `\W`) and Unicode class names (`\p{Lu}`, `\pL`) are ignored.
pub(crate) fn pattern_has_uppercase(pattern: &str, fixed: bool) -> bool {
    if fixed {
        return pattern.chars().any(char::is_uppercase);
    }
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some('p' | 'P') = chars.next() {
                if chars.peek() == Some(&'{') {
                    for c2 in chars.by_ref() {
                        if c2 == '}' {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

/// Compiles a ripgrep-compatible regex with smart-case and literal fallback on invalid regex
/// (matching `GrepSearchTool::execute` semantics for Inspector search-match highlighting).
pub(crate) fn compile_grep_regex(pattern: &str, fixed: bool) -> Option<regex::Regex> {
    if pattern.is_empty() {
        return None;
    }
    let case_sensitive = pattern_has_uppercase(pattern, fixed);
    let build = |src: &str| {
        regex::RegexBuilder::new(src)
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .crlf(true)
            .size_limit(2 << 20)
            .build()
    };
    if fixed {
        build(&regex::escape(pattern)).ok()
    } else {
        build(pattern)
            .ok()
            .or_else(|| build(&regex::escape(pattern)).ok())
    }
}

fn clip_grep_line(line: &str) -> String {
    if line.len() <= MAX_GREP_LINE_CHARS {
        return line.to_string();
    }
    let total = line.chars().count();
    if total <= MAX_GREP_LINE_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_GREP_LINE_CHARS).collect();
    format!("{head}…[+{} chars]", total - MAX_GREP_LINE_CHARS)
}

/// Renders `matches` (sorted 0-based line indices) like `rg --heading -n`:
/// `N:text` for matches, `N-text` for context, `--` between non-adjacent groups.
fn render_grep_matches(lines: &[&str], matches: &[usize], context: usize, out: &mut String) {
    let last = lines.len().saturating_sub(1);
    let mut i = 0;
    while i < matches.len() {
        let start = matches[i].saturating_sub(context);
        let mut end = (matches[i] + context).min(last);
        let mut j = i + 1;
        while j < matches.len() && matches[j].saturating_sub(context) <= end + 1 {
            end = (matches[j] + context).min(last);
            j += 1;
        }
        if i > 0 && context > 0 {
            out.push_str("--\n");
        }
        let group = &matches[i..j];
        for (ln, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            let sep = if group.binary_search(&ln).is_ok() {
                ':'
            } else {
                '-'
            };
            out.push_str(&format!("{}{sep}{}\n", ln + 1, clip_grep_line(line)));
        }
        i = j;
    }
}

impl AgentTool for GrepSearchTool {
    fn name(&self) -> &'static str {
        "grep_search"
    }

    fn description(&self) -> &'static str {
        "Regex search across workspace files (ripgrep syntax; smart case, force with (?i)/(?-i); \
         invalid regex falls back to literal). Output is grouped by file as `N:line`."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex (ripgrep/Rust syntax). Smart case: case-insensitive unless the pattern has an uppercase letter." },
                "path": { "type": "string", "description": "File or directory to search (default: workspace root)." },
                "glob": { "type": "string", "description": "Filename/path glob filter (e.g. '*.rs')." },
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content (default): matching lines grouped by file; files_with_matches: paths only; count: per-file match counts." },
                "fixed_strings": { "type": "boolean", "description": "Treat pattern as literal text (rg -F)." },
                "context": { "type": "integer", "description": "Context lines around each match (0-5, default 0)." },
                "head_limit": { "type": "integer", "description": "Max matching lines (files for files_with_matches/count) to return (default 50, max 200)." }
            },
            "required": ["pattern"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let pattern = str_arg(args, "pattern")?;
        if pattern.is_empty() {
            return Err("'pattern' must not be empty".into());
        }
        let fixed = args
            .get("fixed_strings")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let case_sensitive = pattern_has_uppercase(pattern, fixed);
        let mode = match args
            .get("output_mode")
            .and_then(Value::as_str)
            .unwrap_or("content")
        {
            "content" => GrepMode::Content,
            "files_with_matches" | "files" => GrepMode::Files,
            "count" => GrepMode::Count,
            other => {
                return Err(format!(
                    "invalid output_mode '{other}' (expected content|files_with_matches|count)"
                ))
            }
        };
        let context = args
            .get("context")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(5) as usize;
        let head_limit = args
            .get("head_limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let glob = args.get("glob").and_then(Value::as_str).unwrap_or("*");
        let start =
            ctx.resolve_workspace_path(args.get("path").and_then(Value::as_str).unwrap_or("."))?;

        // multi_line + crlf keep `^`/`$` line-anchored, so a whole-file `is_match` is a
        // sound prefilter for the per-line scan.
        let build = |src: &str| {
            regex::RegexBuilder::new(src)
                .case_insensitive(!case_sensitive)
                .multi_line(true)
                .crlf(true)
                .size_limit(8 << 20)
                .build()
        };
        let mut note = String::new();
        let re = match build(&if fixed {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        }) {
            Ok(re) => re,
            Err(e) if !fixed => {
                let msg = e.to_string();
                let reason = msg.lines().last().unwrap_or("parse error").trim();
                note = format!("[invalid regex ({reason}); searched as literal text]\n");
                build(&regex::escape(pattern)).map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };

        let mut files = Vec::new();
        if start.is_file() {
            files.push(start);
        } else {
            collect_workspace_files(
                &ctx.canon_root(),
                &start,
                glob,
                MAX_GREP_FILES,
                &ctx.cancel,
                &mut files,
            );
        }
        let scan_capped = files.len() >= MAX_GREP_FILES;

        let mut out = String::new();
        let (mut emitted, mut total_matches, mut file_hits) = (0usize, 0usize, 0usize);
        let mut limited = false;
        for file_path in &files {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if emitted >= head_limit {
                limited = true;
                break;
            }
            if fs::metadata(file_path)
                .map(|m| m.len() > MAX_GREP_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(content) = fs::read_to_string(file_path) else {
                continue; // skip binary or unreadable files
            };
            if !re.is_match(&content) {
                continue;
            }
            let rel = ctx.rel_to_root(file_path);
            match mode {
                GrepMode::Files => {
                    out.push_str(&rel);
                    out.push('\n');
                    emitted += 1;
                    file_hits += 1;
                }
                GrepMode::Count => {
                    let n = content.lines().filter(|l| re.is_match(l)).count();
                    if n == 0 {
                        continue;
                    }
                    out.push_str(&format!("{rel}:{n}\n"));
                    emitted += 1;
                    file_hits += 1;
                    total_matches += n;
                }
                GrepMode::Content => {
                    let lines: Vec<&str> = content.lines().collect();
                    let mut matched = Vec::new();
                    for (idx, line) in lines.iter().enumerate() {
                        if re.is_match(line) {
                            if emitted + matched.len() >= head_limit {
                                limited = true;
                                break;
                            }
                            matched.push(idx);
                        }
                    }
                    if matched.is_empty() {
                        continue;
                    }
                    emitted += matched.len();
                    total_matches += matched.len();
                    file_hits += 1;
                    out.push_str(&rel);
                    out.push('\n');
                    render_grep_matches(&lines, &matched, context, &mut out);
                }
            }
        }

        let mut footer = String::new();
        if limited {
            footer.push_str(&format!(
                "[stopped at head_limit={head_limit}; more results may exist - narrow pattern/path/glob]\n"
            ));
        }
        if scan_capped {
            footer.push_str(&format!(
                "[only the first {MAX_GREP_FILES} files were scanned - narrow path/glob]\n"
            ));
        }
        if file_hits == 0 {
            return Ok(format!("{note}No matches for /{pattern}/.\n{footer}")
                .trim_end()
                .to_string());
        }
        let header = match mode {
            GrepMode::Files => format!("Found {file_hits} file(s):"),
            _ => format!("Found {total_matches} match(es) in {file_hits} file(s):"),
        };
        Ok(format!("{note}{header}\n{out}{footer}")
            .trim_end()
            .to_string())
    }
}

// ----------------------------------------------------------------------------
// AST tools (tree-sitter). Gated by the `ast` permission so usage can be measured
// (events: kind='tool_finished', data.tool) and switched off without code changes.
// ----------------------------------------------------------------------------

#[cfg(feature = "tree-sitter")]
const AST_OUTPUT_BUDGET_CHARS: usize = 10_000;
#[cfg(feature = "tree-sitter")]
const AST_MAX_DIR_FILES: usize = 2000;
#[cfg(feature = "tree-sitter")]
const MAX_SYMBOL_MATCHES: usize = 20;
#[cfg(feature = "tree-sitter")]
const MAX_SYMBOL_BODY_LINES: usize = 300;
#[cfg(feature = "tree-sitter")]
const MAX_OUTLINE_LINE_CHARS: usize = 160;

/// Names of the tree-sitter tools, removed together when the `ast` permission is off.
pub const AST_TOOLS: [&str; 3] = ["code_outline", "find_symbol", "find_references"];

#[cfg(feature = "tree-sitter")]
fn clip_outline_line(line: &str) -> String {
    let line = line.trim();
    if line.chars().count() <= MAX_OUTLINE_LINE_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_OUTLINE_LINE_CHARS).collect();
    format!("{head}…")
}

#[cfg(feature = "tree-sitter")]
fn line_span(start: usize, end: usize) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

/// Supported, size-bounded source files under `start` (or `start` itself).
#[cfg(feature = "tree-sitter")]
fn ast_source_files(ctx: &ToolContext, start: &Path) -> (Vec<PathBuf>, bool) {
    use crate::tree_sitter_diff::{SupportedLanguage, MAX_TREESITTER_FILE_BYTES};
    let max_bytes = MAX_TREESITTER_FILE_BYTES as u64;
    let within_limit = |p: &Path| {
        fs::metadata(p)
            .map(|m| m.len() <= max_bytes)
            .unwrap_or(false)
    };
    let mut files = Vec::new();
    if start.is_file() {
        return (vec![start.to_path_buf()], false);
    }
    collect_workspace_files(
        &ctx.canon_root(),
        start,
        "*",
        AST_MAX_DIR_FILES,
        &ctx.cancel,
        &mut files,
    );
    let capped = files.len() >= AST_MAX_DIR_FILES;
    files.retain(|p| SupportedLanguage::from_path(p).is_some() && within_limit(p));
    (files, capped)
}

/// Renders symbols as `start-end  <signature>  // <doc>`, indented by depth. Symbols deeper
/// than `max_depth` are hidden and counted on their visible ancestor as `(+N)`.
#[cfg(feature = "tree-sitter")]
fn render_outline(
    symbols: &[crate::tree_sitter_diff::Symbol],
    lines: &[&str],
    max_depth: usize,
) -> String {
    let mut out = String::new();
    for (i, sym) in symbols.iter().enumerate() {
        if sym.depth > max_depth {
            continue;
        }
        let hidden = if sym.depth == max_depth {
            symbols[i + 1..]
                .iter()
                .take_while(|s| s.depth > sym.depth)
                .count()
        } else {
            0
        };
        let display = sym
            .signature
            .as_deref()
            .map(clip_outline_line)
            .unwrap_or_else(|| {
                clip_outline_line(lines.get(sym.name_line - 1).copied().unwrap_or(""))
            });
        let hidden_note = if hidden > 0 {
            format!("  (+{hidden})")
        } else {
            String::new()
        };
        let doc_note = sym
            .doc
            .as_deref()
            .map(|d| format!("  // {d}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}{}  {}{}{}\n",
            "  ".repeat(sym.depth),
            line_span(sym.start_line, sym.end_line),
            display,
            hidden_note,
            doc_note,
        ));
    }
    out
}

/// Tool: `code_outline`
#[cfg(feature = "tree-sitter")]
pub struct CodeOutlineTool;

#[cfg(feature = "tree-sitter")]
impl AgentTool for CodeOutlineTool {
    fn name(&self) -> &'static str {
        "code_outline"
    }

    fn description(&self) -> &'static str {
        "List definitions (tree-sitter) as `start-end  <signature>  // <doc>`: nested for a file, \
         top-level per file for a directory. Normalized signatures only; call read_file or \
         find_symbol(include_body=true) before edit_file."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Source file or directory relative to workspace root." }
            },
            "required": ["path"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        use crate::tree_sitter_diff::extract_symbols;
        let raw_path = str_arg(args, "path")?;
        let start = ctx.resolve_workspace_path(raw_path)?;

        if start.is_file() {
            let source = read_text(&start)?;
            let (lang, symbols) = extract_symbols(&start, &source)?;
            let lines: Vec<&str> = source.lines().collect();
            let header = format!(
                "{} ({}, {} lines, {} definitions)",
                ctx.rel_to_root(&start),
                lang.as_str(),
                lines.len(),
                symbols.len()
            );
            if symbols.is_empty() {
                return Ok(format!("{header}\nNo definitions found."));
            }
            // Fold nested levels until the outline fits, instead of a blind head/tail cut.
            let deepest = symbols.iter().map(|s| s.depth).max().unwrap_or(0);
            let mut max_depth = deepest;
            let mut body = render_outline(&symbols, &lines, max_depth);
            while body.len() > AST_OUTPUT_BUDGET_CHARS && max_depth > 0 {
                max_depth -= 1;
                body = render_outline(&symbols, &lines, max_depth);
            }
            let note = if max_depth < deepest {
                "[nested definitions folded as (+N) to fit; use find_symbol for a member]\n"
            } else {
                ""
            };
            return Ok(format!("{header}\n{note}{}", body.trim_end()));
        }

        let (files, capped) = ast_source_files(ctx, &start);
        let mut out = String::new();
        let mut shown = 0usize;
        for file in &files {
            if ctx.cancel.is_cancelled() || out.len() > AST_OUTPUT_BUDGET_CHARS {
                break;
            }
            let Ok(source) = read_text(file) else {
                continue;
            };
            let Ok((_, symbols)) = extract_symbols(file, &source) else {
                continue;
            };
            let lines: Vec<&str> = source.lines().collect();
            out.push_str(&format!(
                "{} ({} lines)\n",
                ctx.rel_to_root(file),
                lines.len()
            ));
            out.push_str(&render_outline(&symbols, &lines, 0));
            shown += 1;
        }
        if shown == 0 {
            return Ok(format!("No supported source files under '{raw_path}'."));
        }
        if shown < files.len() {
            out.push_str(&format!(
                "[{} more file(s) not shown; pass a narrower path]\n",
                files.len() - shown
            ));
        }
        if capped {
            out.push_str(&format!(
                "[only the first {AST_MAX_DIR_FILES} files were scanned]\n"
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

/// Tool: `find_symbol`
#[cfg(feature = "tree-sitter")]
pub struct FindSymbolTool;

#[cfg(feature = "tree-sitter")]
impl AgentTool for FindSymbolTool {
    fn name(&self) -> &'static str {
        "find_symbol"
    }

    fn description(&self) -> &'static str {
        "Find definitions by name or Parent/child name path (tree-sitter; exact match, else \
         case-insensitive substring). include_body returns numbered source like read_file."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name or name path, e.g. 'execute', 'GrepSearchTool/execute' ('::' and '.' also accepted)." },
                "path": { "type": "string", "description": "File or directory to search (default: workspace root)." },
                "include_body": { "type": "boolean", "description": "Return the full numbered source of each match (default false)." }
            },
            "required": ["name"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        use crate::tree_sitter_diff::{extract_symbols, split_name_path, Symbol};
        let name = str_arg(args, "name")?;
        let query = split_name_path(name);
        let Some(last) = query.last() else {
            return Err("'name' must contain a symbol name".into());
        };
        let include_body = args
            .get("include_body")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let start =
            ctx.resolve_workspace_path(args.get("path").and_then(Value::as_str).unwrap_or("."))?;
        let (files, capped) = ast_source_files(ctx, &start);
        let needle = last.to_lowercase();

        // (file, source, symbols) for files that can contain the name at all.
        let mut parsed: Vec<(PathBuf, String, Vec<Symbol>)> = Vec::new();
        for file in files {
            if ctx.cancel.is_cancelled() {
                break;
            }
            let Ok(source) = read_text(&file) else {
                continue;
            };
            if !source.to_lowercase().contains(&needle) {
                continue;
            }
            if let Ok((_, symbols)) = extract_symbols(&file, &source) {
                parsed.push((file, source, symbols));
            }
        }

        let collect = |exact: bool| -> Vec<(usize, usize)> {
            let mut hits = Vec::new();
            for (fi, (_, _, symbols)) in parsed.iter().enumerate() {
                for (si, sym) in symbols.iter().enumerate() {
                    if crate::tree_sitter_diff::symbol_matches(&sym.path, &query, exact) {
                        hits.push((fi, si));
                    }
                }
            }
            hits
        };
        let mut note = "";
        let mut hits = collect(true);
        if hits.is_empty() {
            hits = collect(false);
            note = "[no exact match; showing case-insensitive substring matches]\n";
        }
        if hits.is_empty() {
            let mut msg = format!("No definitions matching '{name}'.");
            if capped {
                msg.push_str(&format!(
                    " [only the first {AST_MAX_DIR_FILES} files were scanned; pass a narrower path]"
                ));
            }
            return Ok(msg);
        }

        let total = hits.len();
        let mut shown = 0usize;
        let mut budget_stopped = false;
        let mut out = format!("{note}Found {total} definition(s) of '{name}':\n");
        for &(fi, si) in hits.iter().take(MAX_SYMBOL_MATCHES) {
            if shown > 0 && out.len() >= AST_OUTPUT_BUDGET_CHARS {
                budget_stopped = true;
                break;
            }
            let (file, source, symbols) = &parsed[fi];
            let sym = &symbols[si];
            let lines: Vec<&str> = source.lines().collect();
            let rel = ctx.rel_to_root(file);
            let span = line_span(sym.start_line, sym.end_line);
            let qualified = if sym.path.len() > 1 {
                format!("  {}", sym.qualified_name())
            } else {
                String::new()
            };
            let doc_note = sym
                .doc
                .as_deref()
                .map(|d| format!("  // {d}"))
                .unwrap_or_default();
            if include_body {
                out.push_str(&format!("{rel}:{span}{qualified}{doc_note}\n"));
                let end = sym
                    .end_line
                    .min(sym.start_line + MAX_SYMBOL_BODY_LINES - 1)
                    .min(lines.len());
                for ln in sym.start_line..=end {
                    out.push_str(&format!("{ln:>4} | {}\n", lines[ln - 1]));
                }
                if end < sym.end_line {
                    out.push_str(&format!(
                        "... [{} more line(s); call read_file with start_line={}]\n",
                        sym.end_line - end,
                        end + 1
                    ));
                }
            } else {
                let display = sym
                    .signature
                    .as_deref()
                    .map(clip_outline_line)
                    .unwrap_or_else(|| {
                        clip_outline_line(lines.get(sym.name_line - 1).copied().unwrap_or(""))
                    });
                out.push_str(&format!("{rel}:{span}{qualified}  {display}{doc_note}\n"));
            }
            shown += 1;
        }
        if budget_stopped || total > MAX_SYMBOL_MATCHES {
            out.push_str(&format!(
                "[{} more match(es) not shown; use a Parent/child name path or narrower path]\n",
                total - shown
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

/// Tool: `find_references`
#[cfg(feature = "tree-sitter")]
pub struct FindReferencesTool;

#[cfg(feature = "tree-sitter")]
impl AgentTool for FindReferencesTool {
    fn name(&self) -> &'static str {
        "find_references"
    }

    fn description(&self) -> &'static str {
        "Find AST-verified references to a symbol or Parent/member (skips comments, strings, \
         imports, definitions, and tests by default). Shows enclosing function and call line."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name or Parent/member (e.g. 'execute' or 'Grep::execute')." },
                "path": { "type": "string", "description": "File or directory to search (default: workspace root)." },
                "include_tests": { "type": "boolean", "description": "Include references inside tests (default false)." },
                "max_results": { "type": "integer", "description": "Max references to return (default 20, max 50)." }
            },
            "required": ["name"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        use crate::tree_sitter_diff::{find_references_in_files, split_name_path};
        let name = str_arg(args, "name")?;
        let parts = split_name_path(name);
        let Some(&leaf) = parts.last() else {
            return Err("'name' must not be empty".into());
        };
        let include_tests = args
            .get("include_tests")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50) as usize;
        let start =
            ctx.resolve_workspace_path(args.get("path").and_then(Value::as_str).unwrap_or("."))?;
        let (file_paths, capped) = ast_source_files(ctx, &start);

        let mut sources: Vec<(PathBuf, String)> = Vec::new();
        for fp in file_paths {
            if ctx.cancel.is_cancelled() {
                break;
            }
            let Ok(src) = read_text(&fp) else { continue };
            if !src.contains(leaf) {
                continue;
            }
            sources.push((PathBuf::from(ctx.rel_to_root(&fp)), src));
        }
        let refs = find_references_in_files(name, &sources, include_tests, max);
        if refs.is_empty() {
            let mut msg = format!("No references to '{name}' found.");
            if capped {
                msg.push_str(" [file limit reached; narrow path]");
            }
            return Ok(msg);
        }

        // Group output by file (like grep_search) to minimize repeated paths.
        let mut files_count = 0usize;
        let mut body = String::new();
        let mut cur_file: Option<&Path> = None;
        for r in &refs {
            if cur_file != Some(r.file.as_path()) {
                files_count += 1;
                cur_file = Some(r.file.as_path());
                body.push_str(&r.file.to_string_lossy().replace('\\', "/"));
                body.push('\n');
            }
            let scope = r.enclosing_sig.as_deref().unwrap_or("module");
            body.push_str(&format!("  {} [{scope}]  {}\n", r.line, r.call_line));
        }
        let mut out = format!(
            "Found {} reference(s) to '{name}' in {files_count} file(s):\n{body}",
            refs.len()
        );
        if refs.len() >= max {
            out.push_str("[limit reached; pass a narrower path or increase max_results]\n");
        }
        if capped {
            out.push_str(&format!(
                "[only the first {AST_MAX_DIR_FILES} files were scanned]\n"
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

/// Tool: `read_file`
pub struct ReadFileTool;

impl AgentTool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read file lines (1-based start_line..end_line, default: first 500 lines)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to workspace root." },
                "start_line": { "type": "integer", "description": "Optional 1-based start line." },
                "end_line": { "type": "integer", "description": "Optional 1-based inclusive end line." }
            },
            "required": ["path"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let raw_path = str_arg(args, "path")?;
        let path = ctx.resolve_workspace_path(raw_path)?;
        let content = read_text(&path).map_err(|e| {
            format!(
                "read '{}': {e} (binary or non-UTF-8 files are not supported)",
                path.display()
            )
        })?;

        let total = content.lines().count();
        if total == 0 {
            return Ok(format!("File '{raw_path}' is empty (0 lines)."));
        }

        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        if start > total {
            return Err(format!(
                "start_line {start} exceeds file length ({total} lines)"
            ));
        }
        let explicit_end = args.get("end_line").and_then(Value::as_u64);
        let end = explicit_end
            .unwrap_or((start + DEFAULT_READ_LINES - 1) as u64)
            .max(start as u64)
            .min(total as u64)
            .min((start.saturating_add(DEFAULT_READ_LINES - 1)) as u64) as usize;

        let mut numbered = Vec::with_capacity(end - start + 1);
        for (idx, line) in content
            .lines()
            .skip(start - 1)
            .take(end - start + 1)
            .enumerate()
        {
            numbered.push(format!("{:>4} | {}", start + idx, line));
        }
        let body = numbered.join("\n");
        let mut out = format!("File: {raw_path} (lines {start}..={end} of {total})\n{body}");
        if end < total {
            out.push_str(&format!(
                "\n... [{} more line(s); call read_file with start_line={}]",
                total - end,
                end + 1
            ));
        }
        if ctx.dedup_reads && out.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
            let hash = crate::domain::digest(body.as_bytes());
            let key = (path, start, end);
            if let Ok(mut cache) = ctx.read_cache.lock() {
                if cache.get(&key).is_some_and(|prev| prev == &hash) {
                    return Ok(format!(
                        "File: {raw_path} (lines {start}..={end} of {total}) [unchanged since earlier read in this session; sha256={}]",
                        &hash[..12]
                    ));
                }
                cache.insert(key, hash);
            }
        }
        Ok(out)
    }
}

/// Tool: `edit_file`
pub struct EditFileTool;

impl AgentTool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace exact old_text with new_text in a file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to workspace root." },
                "old_text": { "type": "string", "description": "Exact existing text block to find." },
                "new_text": { "type": "string", "description": "Replacement text block." },
                "replace_all": { "type": "boolean", "description": "If true, replace all occurrences instead of requiring a unique match." }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let raw_path = str_arg(args, "path")?;
        let old_text = str_arg(args, "old_text")?;
        let new_text = str_arg(args, "new_text")?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if old_text.is_empty() {
            return Err(
                "'old_text' must not be empty (use write_file to overwrite or create files)".into(),
            );
        }

        let path = ctx.resolve_workspace_path(raw_path)?;
        let original = read_text(&path)?;

        let count = original.matches(old_text).count();
        if count == 0 {
            return Err(format!(
                "'old_text' not found in '{raw_path}'. Read the file first to verify exact lines and indentation."
            ));
        }
        if count > 1 && !replace_all {
            return Err(format!(
                "'old_text' matched {count} locations in '{raw_path}'. Include more surrounding context lines or set \"replace_all\": true."
            ));
        }

        let updated = if replace_all {
            original.replace(old_text, new_text)
        } else {
            original.replacen(old_text, new_text, 1)
        };

        write_text(&path, &updated, Some(&original))?;
        ctx.record_change(&path);

        Ok(format!(
            "Edited '{raw_path}' ({} replacement(s), -{} / +{} lines).",
            if replace_all { count } else { 1 },
            old_text.lines().count(),
            new_text.lines().count()
        ))
    }
}

/// Tool: `write_file`
pub struct WriteFileTool;

impl AgentTool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to workspace root." },
                "content": { "type": "string", "description": "Full text content to write." }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let raw_path = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;

        let path = ctx.resolve_workspace_path(raw_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create parent dirs '{}': {e}", parent.display()))?;
        }
        let original = if path.exists() {
            Some(read_text(&path)?)
        } else {
            None
        };
        write_text(&path, content, original.as_deref())?;
        ctx.record_change(&path);

        Ok(format!(
            "Wrote {} bytes ({} lines) to '{raw_path}'.",
            content.len(),
            content.lines().count()
        ))
    }
}

fn resolve_cwd(args: &Value, ctx: &ToolContext) -> Result<PathBuf, String> {
    match args.get("cwd").and_then(Value::as_str) {
        Some(sub) if !sub.trim().is_empty() => ctx.resolve_workspace_path(sub),
        _ => Ok(ctx.workspace_root.clone()),
    }
}

fn require_shell(ctx: &ToolContext) -> Result<(), String> {
    if ctx.allow_shell {
        Ok(())
    } else {
        Err("shell tools are disabled (--no-shell)".into())
    }
}

fn non_empty_command(args: &Value) -> Result<&str, String> {
    let c = str_arg(args, "command")?;
    if c.trim().is_empty() {
        return Err("'command' must not be empty".into());
    }
    Ok(c)
}

fn slice_utf8_max_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn emit_stream_buf(
    cb: &ToolOutputFn,
    stream: OutputStream,
    buf: &mut String,
    emitted_bytes: &mut usize,
) {
    if buf.is_empty() {
        return;
    }
    let remaining = SHELL_STREAM_MAX_BYTES.saturating_sub(*emitted_bytes);
    if remaining == 0 {
        buf.clear();
        return;
    }
    let chunk = slice_utf8_max_bytes(buf, remaining);
    if !chunk.is_empty() {
        cb(stream, chunk);
        *emitted_bytes += chunk.len();
    }
    buf.clear();
}

/// Tool: `run_shell` (blocking with timeout, cancellable, kills the whole process tree)
pub struct ShellTool;

impl AgentTool for ShellTool {
    fn name(&self) -> &'static str {
        "run_shell"
    }

    fn description(&self) -> &'static str {
        "Run a shell command and wait for exit (default timeout 60s, max 600s)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Native shell command string to execute." },
                "cwd": { "type": "string", "description": "Optional working directory relative to workspace root." },
                "timeout_secs": { "type": "integer", "description": "Optional timeout override in seconds (default 60, max 600)." }
            },
            "required": ["command"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        require_shell(ctx)?;
        let command_str = non_empty_command(args)?;
        let cwd = resolve_cwd(args, ctx)?;
        let timeout = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .map(|s| Duration::from_secs(s.clamp(1, MAX_SHELL_TIMEOUT_SECS)))
            .unwrap_or(ctx.shell_timeout);

        let mut proc =
            CapturedProcess::spawn_with_env(command_str, &cwd, ctx.environment.as_ref())?;
        let deadline = Instant::now() + timeout;
        let mut stdout_off = 0usize;
        let mut stderr_off = 0usize;
        let mut pending_out = String::new();
        let mut pending_err = String::new();
        let mut emitted_bytes = 0usize;
        let mut last_flush = Instant::now();

        let mut drain_stream = |proc: &CapturedProcess, force_flush: bool| {
            let Some(cb) = &ctx.on_output else {
                return;
            };
            if emitted_bytes >= SHELL_STREAM_MAX_BYTES {
                pending_out.clear();
                pending_err.clear();
                return;
            }
            loop {
                let (out_chunk, err_chunk) = proc.take_output_since(
                    &mut stdout_off,
                    &mut stderr_off,
                    SHELL_STREAM_COALESCE_BYTES,
                    force_flush,
                );
                let got_any = !out_chunk.is_empty() || !err_chunk.is_empty();
                if !out_chunk.is_empty() {
                    pending_out.push_str(&out_chunk);
                }
                if !err_chunk.is_empty() {
                    pending_err.push_str(&err_chunk);
                }
                let pending_len = pending_out.len() + pending_err.len();
                let should_flush = force_flush
                    || pending_len >= SHELL_STREAM_COALESCE_BYTES
                    || (pending_len > 0 && last_flush.elapsed() >= SHELL_STREAM_FLUSH_INTERVAL);
                if should_flush && pending_len > 0 {
                    emit_stream_buf(
                        cb,
                        OutputStream::Stdout,
                        &mut pending_out,
                        &mut emitted_bytes,
                    );
                    emit_stream_buf(
                        cb,
                        OutputStream::Stderr,
                        &mut pending_err,
                        &mut emitted_bytes,
                    );
                    last_flush = Instant::now();
                }
                if !force_flush || !got_any || emitted_bytes >= SHELL_STREAM_MAX_BYTES {
                    break;
                }
            }
        };

        loop {
            drain_stream(&proc, false);
            if let Some(code) = proc.poll_exit() {
                if proc.readers_finished() {
                    proc.join_readers();
                    drain_stream(&proc, true);
                    let (o, e) = proc.output_since(0, 0);
                    return Ok(format_report(Some(code), &o, &e));
                }
            }
            let stop = if ctx.cancel.is_cancelled() {
                Some("cancelled".to_string())
            } else if Instant::now() >= deadline {
                Some(format!("command timed out after {}s", timeout.as_secs()))
            } else {
                None
            };
            if let Some(reason) = stop {
                proc.kill();
                proc.join_readers();
                drain_stream(&proc, true);
                let (o, e) = proc.output_since(0, 0);
                return Err(format!(
                    "{reason}: {command_str}\n{}",
                    format_report(None, &o, &e)
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Tool: `start_process`
pub struct StartProcessTool;

impl AgentTool for StartProcessTool {
    fn name(&self) -> &'static str {
        "start_process"
    }

    fn description(&self) -> &'static str {
        "Start a background shell command and return its id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Native shell command string to start." },
                "cwd": { "type": "string", "description": "Optional working directory relative to workspace root." }
            },
            "required": ["command"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        require_shell(ctx)?;
        let command_str = non_empty_command(args)?;
        let cwd = resolve_cwd(args, ctx)?;
        let id = ctx
            .background
            .start_with_env(command_str, &cwd, ctx.environment.as_ref())?;
        Ok(format!(
            "Started background process #{id}: {command_str}\nUse process_output {{\"id\": {id}}} to read new output and stop_process to stop it."
        ))
    }
}

/// Tool: `process_output`
pub struct ProcessOutputTool;

impl AgentTool for ProcessOutputTool {
    fn name(&self) -> &'static str {
        "process_output"
    }

    fn description(&self) -> &'static str {
        "Read new output/status from a background process."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Process id returned by start_process." },
                "wait_secs": { "type": "integer", "description": "Optional: wait up to N seconds for new output or exit." }
            },
            "required": ["id"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let id = args
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| "missing integer parameter 'id'".to_string())?;
        let wait = args
            .get("wait_secs")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(MAX_PROCESS_WAIT_SECS);

        let unknown = || {
            format!(
                "unknown background process #{id}. Known: [{}]",
                ctx.background.list().join("; ")
            )
        };
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            let (code, unread) = ctx.background.status(id).ok_or_else(unknown)?;
            if code.is_some() || unread || ctx.cancel.is_cancelled() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let (_, report) = ctx.background.poll(id).ok_or_else(unknown)?;
        Ok(report)
    }
}

/// Tool: `stop_process`
pub struct StopProcessTool;

impl AgentTool for StopProcessTool {
    fn name(&self) -> &'static str {
        "stop_process"
    }

    fn description(&self) -> &'static str {
        "Kill a background process by id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Process id returned by start_process." }
            },
            "required": ["id"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let id = args
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| "missing integer parameter 'id'".to_string())?;
        if !ctx.background.kill(id) {
            return Err(format!("unknown background process #{id}"));
        }
        let tail = ctx
            .background
            .poll(id)
            .map(|(_, r)| format!("\n{r}"))
            .unwrap_or_default();
        Ok(format!("Killed background process #{id}.{tail}"))
    }
}

/// Tool: `web_fetch` (workspace-local direct HTTP/HTTPS fetch)
pub struct WebFetchTool;

impl AgentTool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fast local HTTP/HTTPS fetch (clean Markdown/text or raw, max_chars <= 16000)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "minLength": 1, "description": "Public HTTP or HTTPS URL to fetch." },
                "format": { "type": "string", "enum": ["text", "raw"], "description": "Output format: 'text' (default, clean text/Markdown from HTML) or 'raw' (unparsed body)." },
                "max_chars": { "type": "integer", "minimum": 200, "maximum": 16000, "description": "Maximum characters to return (default 12000, range 200..16000)." }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let url = str_arg(args, "url")?;
        let format = args.get("format").and_then(Value::as_str).unwrap_or("text");
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(crate::web::DEFAULT_WEB_MAX_CHARS);
        crate::web::web_fetch(url, format, max_chars, &ctx.cancel)
    }
}

/// Tool: `browser_fetch` (stateless Chrome CDP tab fetch)
pub struct BrowserFetchTool;

impl AgentTool for BrowserFetchTool {
    fn name(&self) -> &'static str {
        "browser_fetch"
    }

    fn description(&self) -> &'static str {
        "Stateless Chrome JS-rendered page text read (max_chars <= 16000, wait_ms <= 10000)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "minLength": 1, "description": "Public HTTP or HTTPS URL to open in Chrome." },
                "account_id": { "type": "string", "minLength": 1, "description": "Optional browser account ID (defaults to the active agent account)." },
                "wait_ms": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "Extra milliseconds to wait after load completes for JS rendering (default 1000, max 10000)." },
                "max_chars": { "type": "integer", "minimum": 200, "maximum": 16000, "description": "Maximum characters to return (default 12000, range 200..16000)." }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let Some(fetcher) = &ctx.browser_fetch else {
            return Err(
                "browser_fetch is unavailable in this agent context (no browser runtime attached)"
                    .into(),
            );
        };
        let url = str_arg(args, "url")?;
        let account_id = args.get("account_id").and_then(Value::as_str);
        let wait_ms = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(1000)
            .min(10_000) as u32;
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(crate::web::DEFAULT_WEB_MAX_CHARS);
        fetcher(url, account_id, wait_ms, max_chars, &ctx.cancel)
    }
}

/// Stateful browser commands shared with the terminal and RPC host.
pub struct BrowserControlTool;
impl AgentTool for BrowserControlTool {
    fn name(&self) -> &'static str {
        "browser_control"
    }
    fn description(&self) -> &'static str {
        "Interactive browser session. Call with {\"action\":\"help\"} alone first."
    }
    fn parameters_schema(&self) -> Value {
        crate::browser_control::schema()
    }
    fn initial_parameters_schema(&self) -> Value {
        crate::browser_control::help_schema()
    }
    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        self.execute_content(args, ctx).map(|result| result.text)
    }
    fn execute_content(&self, args: &Value, ctx: &ToolContext) -> Result<ToolContent, String> {
        crate::browser_control::validate_args(args)?;
        if args["action"] == "help" {
            return Ok(crate::browser_control::agent_help().into());
        }
        if !ctx.browser_control_help_ready.load(Ordering::Relaxed) {
            return Err("First call browser_control with {\"action\":\"help\"} on its own and read the result before requesting browser actions. This action was not executed.".into());
        }
        let controller = ctx.browser_control.as_ref().ok_or(
            "browser_control is unavailable in this agent context (no browser runtime attached)",
        )?;
        controller(args, &ctx.cancel)
    }
}

/// Tool: `spawn_agent` (delegate a focused sub-task to a `Worker` `TempChat` tab)
pub struct SpawnAgentTool;

impl AgentTool for SpawnAgentTool {
    fn name(&self) -> &'static str {
        "spawn_agent"
    }

    fn description(&self) -> &'static str {
        "Delegate isolated broad exploration or 2+ parallel sub-tasks to a worker tab (read_only=true by default)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Self-contained instructions for the worker sub-agent, including what files/modules to inspect and what concise findings to return."
                },
                "label": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 60,
                    "description": "Short 2-5 word role or task title shown in the live TUI worker status line (e.g. 'Trace auth flow', 'Inspect RPC tests')."
                },
                "read_only": {
                    "type": "boolean",
                    "description": "When true (default), restricts the sub-agent to read-only exploration tools so parallel workers cannot conflict on files. Set false only if the sub-agent must edit files or run shell commands."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let Some(runner) = &ctx.spawn_agent else {
            return Err(
                "spawn_agent is unavailable in this agent context (worker sub-agents cannot recursively spawn sub-agents)"
                    .into(),
            );
        };
        let task = str_arg(args, "task")?.trim();
        if task.is_empty() {
            return Err("'task' must not be empty".into());
        }
        let default_label: String = task
            .lines()
            .next()
            .unwrap_or("worker")
            .chars()
            .take(36)
            .collect();
        let label = args
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_label.as_str());
        let read_only = args
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let outcome = runner(label, task, read_only, &ctx.cancel)?;
        if !outcome.files_changed.is_empty() {
            ctx.record_relative_changes(&outcome.files_changed);
        }
        Ok(outcome.summary)
    }
}
