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
use super::events::{ToolCallRequest, ToolOutput, DEFAULT_SHELL_TIMEOUT_SECS};
use super::process::{format_report, BackgroundProcesses, CapturedProcess};

const MAX_TOOL_OUTPUT_CHARS: usize = 16_000;
const DEFAULT_READ_LINES: usize = 2_000;
const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;
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
    browser_control_help_ready: AtomicBool,
    /// Optional runtime bridge for `spawn_agent` (delegating tasks to the `Worker` `TempChat` tab pool).
    pub spawn_agent: Option<SpawnAgentFn>,
    changed_files: Mutex<BTreeSet<String>>,
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
            browser_control_help_ready: AtomicBool::new(false),
            spawn_agent: None,
            changed_files: Mutex::new(BTreeSet::new()),
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
        let res = catch_unwind(AssertUnwindSafe(|| tool.execute_content(&call.args, ctx)))
            .unwrap_or_else(|_| Err("tool panicked".to_string()));
        match res {
            Ok(raw) => {
                let mut out = make(true, truncate_tool_output(&raw.text, MAX_TOOL_OUTPUT_CHARS));
                out.artifacts = raw.artifacts;
                out
            }
            Err(err) => make(
                false,
                format!(
                    "error: {}",
                    truncate_tool_output(&err, MAX_TOOL_OUTPUT_CHARS)
                ),
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
    /// `on_start` / `on_done` fire from the executing thread as each call begins / completes.
    pub fn execute_batch(
        &self,
        calls: &[ToolCallRequest],
        ctx: &ToolContext,
        on_start: &(dyn Fn(usize, &ToolCallRequest) + Sync),
        on_done: &(dyn Fn(usize, &ToolCallRequest, &ToolOutput) + Sync),
    ) -> Vec<ToolOutput> {
        let mut results: Vec<Option<ToolOutput>> = vec![None; calls.len()];
        let mut idx = 0;
        while idx < calls.len() {
            let mut end = idx + 1;
            if self.is_read_only(&calls[idx].tool) {
                while end < calls.len() && self.is_read_only(&calls[end].tool) {
                    end += 1;
                }
            }
            if end - idx == 1 {
                on_start(idx, &calls[idx]);
                let out = self.execute_call(&calls[idx], ctx);
                on_done(idx, &calls[idx], &out);
                results[idx] = Some(out);
            } else {
                let outs: Vec<(usize, ToolOutput)> = std::thread::scope(|s| {
                    let handles: Vec<_> = (idx..end)
                        .map(|i| {
                            let call = &calls[i];
                            s.spawn(move || {
                                on_start(i, call);
                                let out = self.execute_call(call, ctx);
                                on_done(i, call, &out);
                                (i, out)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .enumerate()
                        .map(|(off, h)| {
                            h.join().unwrap_or_else(|_| {
                                (
                                    idx + off,
                                    ToolOutput {
                                        tool: calls[idx + off].tool.clone(),
                                        ok: false,
                                        output: "error: tool worker thread panicked".into(),
                                        artifacts: Vec::new(),
                                        elapsed_ms: 0,
                                    },
                                )
                            })
                        })
                        .collect()
                });
                for (i, out) in outs {
                    results[i] = Some(out);
                }
            }
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

    let clean_desc = description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

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

fn collect_workspace_files(
    root: &Path,
    dir: &Path,
    pattern: &str,
    max_results: usize,
    cancel: &CancelToken,
    out: &mut Vec<PathBuf>,
) {
    if out.len() >= max_results || cancel.is_cancelled() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if out.len() >= max_results {
            break;
        }
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            if !is_ignored_dir(&fname) {
                collect_workspace_files(root, &path, pattern, max_results, cancel, out);
            }
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if wildcard_match(pattern, &rel) || wildcard_match(pattern, &fname) {
                out.push(path);
            }
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

/// Tool: `grep_search` (literal, not regex)
pub struct GrepSearchTool;

impl AgentTool for GrepSearchTool {
    fn name(&self) -> &'static str {
        "grep_search"
    }

    fn description(&self) -> &'static str {
        "Search literal text across workspace files (case-insensitive by default)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Literal text to search for." },
                "dir": { "type": "string", "description": "Optional subdirectory inside workspace to limit the search." },
                "file_pattern": { "type": "string", "description": "Optional filename/glob filter (e.g. '*.rs')." },
                "case_sensitive": { "type": "boolean", "description": "Whether search is case-sensitive (default false)." },
                "context_lines": { "type": "integer", "description": "Number of context lines before/after each match (default 0)." },
                "max_matches": { "type": "integer", "description": "Maximum matches to return (default 50)." }
            },
            "required": ["query"]
        })
    }

    fn execute(&self, args: &Value, ctx: &ToolContext) -> Result<String, String> {
        let query = str_arg(args, "query")?;
        if query.is_empty() {
            return Err("'query' must not be empty".into());
        }
        let file_pattern = args
            .get("file_pattern")
            .and_then(Value::as_str)
            .unwrap_or("*");
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(5) as usize;
        let max_matches = args
            .get("max_matches")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let start_dir =
            ctx.resolve_workspace_path(args.get("dir").and_then(Value::as_str).unwrap_or("."))?;

        let canon_root = ctx.canon_root();
        let mut files = Vec::new();
        collect_workspace_files(
            &canon_root,
            &start_dir,
            file_pattern,
            2000,
            &ctx.cancel,
            &mut files,
        );

        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };

        let mut hits = Vec::new();
        for file_path in files {
            if hits.len() >= max_matches || ctx.cancel.is_cancelled() {
                break;
            }
            if fs::metadata(&file_path)
                .map(|m| m.len() > MAX_GREP_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(content) = fs::read_to_string(&file_path) else {
                continue; // skip binary or unreadable files
            };
            let rel = ctx.rel_to_root(&file_path);
            let lines: Vec<&str> = content.lines().collect();
            for (idx, &line) in lines.iter().enumerate() {
                let matched = if case_sensitive {
                    line.contains(&needle)
                } else {
                    line.to_lowercase().contains(&needle)
                };
                if !matched {
                    continue;
                }
                if context_lines == 0 {
                    hits.push(format!("{rel}:{}: {line}", idx + 1));
                } else {
                    let start = idx.saturating_sub(context_lines);
                    let end = (idx + context_lines + 1).min(lines.len());
                    let mut block = Vec::new();
                    for (ci, &cline) in lines[start..end].iter().enumerate() {
                        let line_no = start + ci + 1;
                        let sep = if start + ci == idx { ":" } else { "-" };
                        block.push(format!("{rel}{sep}{line_no}{sep} {cline}"));
                    }
                    hits.push(block.join("\n"));
                }
                if hits.len() >= max_matches {
                    break;
                }
            }
        }

        if hits.is_empty() {
            Ok(format!("No matches found for '{query}'."))
        } else {
            Ok(format!(
                "Found {} match(es) for '{query}':\n{}",
                hits.len(),
                hits.join("\n")
            ))
        }
    }
}

/// Tool: `read_file`
pub struct ReadFileTool;

impl AgentTool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read file lines (1-based start_line..end_line, default: first 2000 lines)."
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
        let mut out = format!(
            "File: {raw_path} (lines {start}..={end} of {total})\n{}",
            numbered.join("\n")
        );
        if end < total {
            out.push_str(&format!(
                "\n... [{} more line(s); call read_file with start_line={}]",
                total - end,
                end + 1
            ));
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
        loop {
            if let Some(code) = proc.poll_exit() {
                if proc.readers_finished() {
                    proc.join_readers();
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
