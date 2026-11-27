use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cancel::CancelToken;
use super::events::ToolCallRequest;

/// Per-call authorization decision used by [`ToolApprovalPolicy`].
///
/// Designed so saved/user-defined permission rules can selectively auto-allow or deny
/// specific tools and argument patterns while still falling back to interactive batch `Y/N`
/// confirmation for unapproved calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    #[default]
    Ask,
    Deny {
        #[serde(default)]
        reason: String,
    },
}

/// Argument predicate attached to a [`ToolPermissionRule`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArgMatcher {
    #[default]
    Any,
    PathPrefix {
        prefix: String,
    },
    CommandPrefix {
        prefix: String,
    },
    UrlHost {
        host: String,
    },
    FieldEquals {
        field: String,
        value: Value,
    },
}

impl ToolArgMatcher {
    pub fn matches(&self, args: &Value) -> bool {
        match self {
            Self::Any => true,
            Self::PathPrefix { prefix } => {
                let Some(normalized) = normalize_match_path(prefix) else {
                    return false;
                };
                args.get("path")
                    .or_else(|| args.get("dir"))
                    .and_then(Value::as_str)
                    .and_then(normalize_match_path)
                    .is_some_and(|candidate| {
                        has_root_or_prefix(&candidate) == has_root_or_prefix(&normalized)
                            && candidate.starts_with(&normalized)
                    })
            }
            Self::CommandPrefix { prefix } => args
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|cmd| {
                    !contains_shell_metachar(cmd) && command_prefix_matches(cmd, prefix)
                }),
            Self::UrlHost { host } => args
                .get("url")
                .and_then(Value::as_str)
                .and_then(|raw| crate::url_lite::Url::parse(raw).ok())
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|actual| actual.eq_ignore_ascii_case(host.trim())),
            Self::FieldEquals { field, value } => args.get(field) == Some(value),
        }
    }
}

/// Declarative rule matching a tool name (or `"*"`) and argument predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPermissionRule {
    pub tool: String,
    #[serde(default)]
    pub matcher: ToolArgMatcher,
    pub decision: PermissionDecision,
}

impl ToolPermissionRule {
    pub fn matches(&self, call: &ToolCallRequest) -> bool {
        if self.tool != "*" && self.tool != call.tool {
            return false;
        }
        if matches!(self.decision, PermissionDecision::Deny { .. }) {
            if let ToolArgMatcher::CommandPrefix { prefix } = &self.matcher {
                return call
                    .args
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|cmd| command_any_segment_matches_prefix(cmd, prefix));
            }
        }
        self.matcher.matches(&call.args)
    }
}

/// Policy governing whether tool batches auto-execute, prompt for `Y/N`, or deny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolApprovalPolicy {
    pub default_decision: PermissionDecision,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ToolPermissionRule>,
}

impl Default for ToolApprovalPolicy {
    fn default() -> Self {
        Self::auto()
    }
}

impl ToolApprovalPolicy {
    /// Auto-approve all tool calls (`--yolo` or internal non-interactive harness defaults).
    pub fn auto() -> Self {
        Self {
            default_decision: PermissionDecision::Allow,
            rules: Vec::new(),
        }
    }

    /// Require explicit batch `Y/N` confirmation before executing tool calls (`/mode agent`).
    pub fn ask_all() -> Self {
        Self {
            default_decision: PermissionDecision::Ask,
            rules: Vec::new(),
        }
    }

    pub fn evaluate_call(&self, call: &ToolCallRequest) -> PermissionDecision {
        for rule in &self.rules {
            if rule.matches(call) {
                return rule.decision.clone();
            }
        }
        self.default_decision.clone()
    }

    /// Evaluate every call in a step batch. All `Ask` calls are collected into one batch so the
    /// user reviews and approves or rejects them together in a single `Y/N` decision.
    pub fn evaluate_batch(&self, calls: &[ToolCallRequest]) -> BatchApprovalPlan {
        let decisions: Vec<PermissionDecision> =
            calls.iter().map(|call| self.evaluate_call(call)).collect();
        let prompt_indices: Vec<usize> = decisions
            .iter()
            .enumerate()
            .filter_map(|(idx, d)| matches!(d, PermissionDecision::Ask).then_some(idx))
            .collect();
        BatchApprovalPlan {
            decisions,
            prompt_indices,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchApprovalPlan {
    pub decisions: Vec<PermissionDecision>,
    pub prompt_indices: Vec<usize>,
}

impl BatchApprovalPlan {
    pub fn needs_user_approval(&self) -> bool {
        !self.prompt_indices.is_empty()
    }

    pub fn prompt_calls(&self, calls: &[ToolCallRequest]) -> Vec<ToolCallRequest> {
        self.prompt_indices
            .iter()
            .filter_map(|&idx| calls.get(idx).cloned())
            .collect()
    }
}

#[derive(Debug, Default)]
struct GateState {
    /// `None` = pending user decision, `Some(approved)` = resolved.
    entries: HashMap<String, Option<bool>>,
}

/// RAII guard returned by [`ToolApprovalGate::register_guard`].
/// Removes an unresolved `approval_id` on drop (e.g. panic or early return).
#[derive(Debug)]
pub struct ApprovalGuard {
    gate: ToolApprovalGate,
    approval_id: String,
}

impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        self.gate.unregister(&self.approval_id);
    }
}

/// Thread-safe coordinator between blocking agent workers waiting for batch tool approval
/// and UI/RPC callers submitting `Y/N` decisions or cancellation.
///
/// Keyed by unique `approval_id` so concurrent sub-agents never overwrite one another.
#[derive(Debug, Clone, Default)]
pub struct ToolApprovalGate {
    inner: Arc<(Mutex<GateState>, Condvar)>,
}

impl ToolApprovalGate {
    /// Register an approval request BEFORE emitting the `ToolApprovalRequested` event
    /// so a fast responder cannot observe the event before `approval_id` is registered.
    pub fn register(&self, approval_id: &str) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.insert(approval_id.to_owned(), None);
        cvar.notify_all();
    }

    /// Register an approval request and return an RAII guard that removes it on drop.
    #[must_use]
    pub fn register_guard(&self, approval_id: &str) -> ApprovalGuard {
        self.register(approval_id);
        ApprovalGuard {
            gate: self.clone(),
            approval_id: approval_id.to_owned(),
        }
    }

    pub fn unregister(&self, approval_id: &str) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.entries.remove(approval_id).is_some() {
            cvar.notify_all();
        }
    }

    pub fn pending_count(&self) -> usize {
        let (lock, _) = &*self.inner;
        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.values().filter(|v| v.is_none()).count()
    }

    /// Block until `approval_id` is resolved with `Some(true|false)` or `None` if cancelled
    /// or unregistered.
    pub fn wait(&self, approval_id: &str, cancel: &CancelToken) -> Option<bool> {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match state.entries.get(approval_id).copied() {
                Some(Some(approved)) => {
                    state.entries.remove(approval_id);
                    return Some(approved);
                }
                None => return None,
                Some(None) => {}
            }
            if cancel.is_cancelled() {
                state.entries.remove(approval_id);
                return None;
            }
            let (guard, _) = cvar
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
    }

    /// Deliver a user decision (`approved = true/false`) for a pending `approval_id`.
    /// Returns `true` if the `approval_id` was currently pending.
    pub fn resolve(&self, approval_id: &str, approved: bool) -> bool {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = state.entries.get_mut(approval_id) else {
            return false;
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(approved);
        cvar.notify_all();
        true
    }

    /// Wake all threads waiting on tool approval (e.g. immediately upon cancellation).
    pub fn wake_all(&self) {
        let (lock, cvar) = &*self.inner;
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        cvar.notify_all();
    }
}

fn has_root_or_prefix(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(_) | Component::RootDir)
    )
}

fn normalize_match_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut components: Vec<Component<'_>> = Vec::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                components.push(comp);
            }
            Component::ParentDir => match components.last() {
                Some(Component::Normal(_)) => {
                    components.pop();
                }
                _ => return None,
            },
        }
    }
    let mut out = PathBuf::new();
    for comp in components {
        out.push(comp.as_os_str());
    }
    Some(out)
}

fn contains_shell_metachar(cmd: &str) -> bool {
    cmd.bytes()
        .any(|b| matches!(b, b';' | b'&' | b'|' | b'`' | b'\n' | b'\r' | b'<' | b'>'))
        || cmd.contains("$(")
        || cmd.contains("${")
}

fn command_prefix_matches(cmd: &str, prefix: &str) -> bool {
    let trimmed_start = prefix.trim_start();
    let requires_arg_ws = trimmed_start.ends_with(char::is_whitespace);
    let norm_prefix = trimmed_start.trim_end();
    if norm_prefix.is_empty() {
        return false;
    }
    let norm_cmd = cmd.trim_start();
    let Some(rest) = norm_cmd.strip_prefix(norm_prefix) else {
        return false;
    };
    if requires_arg_ws {
        rest.starts_with(char::is_whitespace)
    } else {
        rest.is_empty() || rest.starts_with(char::is_whitespace)
    }
}

fn strip_leading_env_assignments(mut segment: &str) -> &str {
    loop {
        let trimmed = segment.trim_start();
        let Some(token_end) = trimmed.find(char::is_whitespace) else {
            return trimmed;
        };
        let token = &trimmed[..token_end];
        if let Some((name, _)) = token.split_once('=') {
            let mut chars = name.chars();
            let valid_name = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if valid_name {
                segment = &trimmed[token_end..];
                continue;
            }
        }
        return trimmed;
    }
}

fn command_any_segment_matches_prefix(cmd: &str, prefix: &str) -> bool {
    cmd.split(|ch| {
        matches!(
            ch,
            ';' | '&' | '|' | '\n' | '\r' | '(' | ')' | '`' | '{' | '}'
        )
    })
    .any(|segment| {
        command_prefix_matches(segment, prefix)
            || command_prefix_matches(strip_leading_env_assignments(segment), prefix)
    })
}
