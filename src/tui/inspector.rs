//! Tool Inspector domain model, state transitions, lazy detail rendering, and layout helpers.
use super::{
    highlight::{self, HighlightCache, HighlightLang, HighlightedDoc, Lookup},
    layout::{AfterTurnPolicy, HorizontalSplit, Rect},
    markdown::{self, DiffBg, Face, Span},
    reducer::PendingToolApproval,
    transcript::clip,
};
use crate::{
    agent::{AgentStepRecord, OutputStream},
    tree_sitter_diff::SupportedLanguage,
};
use serde_json::{json, Value};
#[cfg(test)]
use std::cell::Cell;
use std::{cell::RefCell, collections::HashMap, path::Path, rc::Rc, sync::Arc, time::Instant};
use unicode_width::UnicodeWidthStr;

pub(super) const MAX_LIVE_BYTES: usize = 64 * 1024;
pub(super) const MAX_SUBAGENT_LOGS: usize = 64;
pub(super) const MAX_SUBAGENT_LOG_BYTES: usize = 16 * 1024;
pub(super) const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const ITEM_CACHE_CAP: usize = 24;
const FEED_PRIOR_MAX_LINES: usize = 40;
const FEED_PRIOR_HEAD: usize = 20;
const FEED_PRIOR_TAIL: usize = 16;
const FEED_LATEST_MAX_LINES: usize = 120;
const FEED_LATEST_HEAD: usize = 12;
const FEED_LATEST_TAIL: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct BlockId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolStatus {
    Pending,
    Running,
    Done(bool),
    Rejected,
    Cancelled,
}

impl ToolStatus {
    pub(super) fn icon(self, spinner: &str) -> (&str, Face) {
        match self {
            Self::Pending => ("?", Face::HEADING),
            Self::Running => (spinner, Face::ACCENT_BOLD),
            Self::Done(true) => ("✓", Face::HIGHLIGHT_BOLD),
            Self::Done(false) | Self::Rejected | Self::Cancelled => ("✗", Face::DANGER_BOLD),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done(true) => "ok",
            Self::Done(false) => "failed",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ToolItem {
    pub step: usize,
    pub index: usize,
    pub approval_id: Option<String>,
    pub source: Option<String>,
    pub subagent_id: Option<String>,
    pub tool: String,
    pub args: Value,
    pub status: ToolStatus,
    pub started_at: Option<Instant>,
    pub elapsed_ms: Option<u64>,
    pub live_chunks: Vec<(OutputStream, String)>,
    pub live_bytes: usize,
    pub live_capped: bool,
    pub subagent_logs: Vec<String>,
    pub subagent_log_bytes: usize,
    pub output: Option<String>,
    pub ok: Option<bool>,
    pub payload_pruned: bool,
    pub rev: u64,
}

fn json_value_bytes(val: &Value) -> usize {
    match val {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(_) => 8,
        Value::String(s) => s.len().saturating_add(2),
        Value::Array(arr) => 2usize.saturating_add(
            arr.iter()
                .map(|v| json_value_bytes(v).saturating_add(1))
                .sum(),
        ),
        Value::Object(map) => 2usize.saturating_add(
            map.iter()
                .map(|(k, v)| {
                    k.len()
                        .saturating_add(4)
                        .saturating_add(json_value_bytes(v))
                })
                .sum(),
        ),
    }
}

impl ToolItem {
    fn estimated_payload_bytes(&self) -> usize {
        json_value_bytes(&self.args)
            + self.live_bytes
            + self.subagent_log_bytes
            + self.output.as_ref().map_or(0, String::len)
    }

    pub(super) fn target_summary(&self) -> String {
        let args = &self.args;
        match self.tool.as_str() {
            "edit_file" | "write_file" | "read_file" | "code_outline" => {
                args["path"].as_str().unwrap_or("").to_owned()
            }
            "run_shell" | "start_process" => args["command"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_owned(),
            "process_output" | "stop_process" => args["id"]
                .as_u64()
                .map(|id| format!("#{id}"))
                .unwrap_or_default(),
            "grep_search" => args["pattern"].as_str().unwrap_or("").to_owned(),
            "find_files" => args["pattern"].as_str().unwrap_or("*").to_owned(),
            "find_symbol" | "find_references" => args["name"].as_str().unwrap_or("").to_owned(),
            "web_fetch" | "browser_fetch" => args["url"].as_str().unwrap_or("").to_owned(),
            "browser_control" => args["action"].as_str().unwrap_or("").to_owned(),
            "spawn_agent" => args["label"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("worker")
                .to_owned(),
            _ => args["path"]
                .as_str()
                .or_else(|| args["command"].as_str())
                .or_else(|| args["url"].as_str())
                .or_else(|| args["query"].as_str())
                .or_else(|| args["pattern"].as_str())
                .or_else(|| args["name"].as_str())
                .or_else(|| args["label"].as_str())
                .unwrap_or("")
                .to_owned(),
        }
    }

    /// Compact key call-option suffix shown on left list rows and feed headers
    /// (e.g., `:100-250` for `read_file`, ` (all)` for `edit_file`, ` (body)` for `find_symbol`).
    pub(super) fn compact_option_suffix(&self) -> String {
        let args = &self.args;
        match self.tool.as_str() {
            "read_file" => {
                if let Some(parsed) = self.output.as_deref().and_then(parse_read_file_header) {
                    return format!(":{}-{}", parsed.start, parsed.end);
                }
                match (args["start_line"].as_u64(), args["end_line"].as_u64()) {
                    (Some(s), Some(e)) => format!(":{s}-{e}"),
                    (Some(s), None) => format!(":{s}-"),
                    (None, Some(e)) => format!(":1-{e}"),
                    (None, None) => ":1-500".into(),
                }
            }
            "edit_file" => {
                if args["replace_all"].as_bool() == Some(true) {
                    " (all)".into()
                } else {
                    String::new()
                }
            }
            "grep_search" => {
                let mut opts = Vec::new();
                if let Some(p) = args["path"].as_str().filter(|s| !s.is_empty() && *s != ".") {
                    opts.push(format!("in {p}"));
                }
                if let Some(g) = args["glob"].as_str().filter(|s| !s.is_empty()) {
                    opts.push(g.to_owned());
                }
                if let Some(mode) = args["output_mode"]
                    .as_str()
                    .filter(|m| !m.is_empty() && *m != "content")
                {
                    opts.push(match mode {
                        "files_with_matches" => "files".into(),
                        other => other.to_owned(),
                    });
                }
                if opts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", opts.join(", "))
                }
            }
            "find_files" => args["dir"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != ".")
                .map(|d| format!(" in {d}"))
                .unwrap_or_default(),
            "find_symbol" => {
                let mut opts = Vec::new();
                if let Some(p) = args["path"].as_str().filter(|s| !s.is_empty() && *s != ".") {
                    opts.push(format!("in {p}"));
                }
                if args["include_body"].as_bool() == Some(true) {
                    opts.push("body".into());
                }
                if opts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", opts.join(", "))
                }
            }
            "find_references" => {
                let mut opts = Vec::new();
                if let Some(p) = args["path"].as_str().filter(|s| !s.is_empty() && *s != ".") {
                    opts.push(format!("in {p}"));
                }
                if args["include_tests"].as_bool() == Some(true) {
                    opts.push("+tests".into());
                }
                if opts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", opts.join(", "))
                }
            }
            "run_shell" | "start_process" => args["cwd"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != ".")
                .map(|cwd| format!(" (cwd={cwd})"))
                .unwrap_or_default(),
            "process_output" => args["wait_secs"]
                .as_u64()
                .filter(|&w| w > 0)
                .map(|w| format!(" (wait={w}s)"))
                .unwrap_or_default(),
            "web_fetch" | "browser_fetch" => {
                if args["format"].as_str() == Some("raw") {
                    " (raw)".into()
                } else {
                    String::new()
                }
            }
            "browser_control" => {
                let detail = args["url"]
                    .as_str()
                    .or_else(|| args["ref"].as_str())
                    .or_else(|| args["selector"].as_str())
                    .or_else(|| args["key"].as_str())
                    .or_else(|| args["text"].as_str())
                    .filter(|s| !s.is_empty());
                detail
                    .map(|d| format!(" {}", clip(d, 24)))
                    .unwrap_or_default()
            }
            "spawn_agent" => {
                if args["read_only"].as_bool() == Some(false) {
                    " (rw)".into()
                } else {
                    " (ro)".into()
                }
            }
            _ => String::new(),
        }
    }

    /// Descriptive line-range label for `read_file` in Inspector headers & body.
    fn read_file_range_label(&self) -> Option<String> {
        if self.tool != "read_file" {
            return None;
        }
        if let Some(parsed) = self.output.as_deref().and_then(parse_read_file_header) {
            return Some(format!(
                "lines {}..={} of {}",
                parsed.start, parsed.end, parsed.total
            ));
        }
        Some(
            match (
                self.args["start_line"].as_u64(),
                self.args["end_line"].as_u64(),
            ) {
                (Some(s), Some(e)) => format!("lines {s}..={e}"),
                (Some(s), None) => format!("lines {s}.. (up to +500)"),
                (None, Some(e)) => format!("lines 1..={e}"),
                (None, None) => "lines 1..=500 (default)".into(),
            },
        )
    }

    fn elapsed_display(&self) -> Option<String> {
        if let Some(ms) = self.elapsed_ms {
            return Some(format_elapsed_ms(ms));
        }
        if self.status == ToolStatus::Running {
            if let Some(started) = self.started_at {
                return Some(format_elapsed_ms(started.elapsed().as_millis() as u64));
            }
        }
        None
    }

    fn prune_heavy_payload(&mut self) -> usize {
        let before = self.estimated_payload_bytes();
        self.live_chunks.clear();
        self.live_bytes = 0;
        self.subagent_logs.clear();
        self.subagent_log_bytes = 0;
        self.output = None;
        self.args = compact_args_for_retention(&self.args);
        self.payload_pruned = true;
        self.rev = self.rev.wrapping_add(1);
        before.saturating_sub(self.estimated_payload_bytes())
    }
}

fn compact_args_for_retention(args: &Value) -> Value {
    const KEEP: &[&str] = &[
        "path",
        "dir",
        "command",
        "url",
        "pattern",
        "name",
        "label",
        "action",
        "id",
        "read_only",
        "replace_all",
        "query",
        "cwd",
        "glob",
        "output_mode",
        "symbol",
        "start_line",
        "end_line",
        "include_body",
        "include_tests",
        "max_results",
        "timeout_secs",
        "format",
    ];
    let Some(map) = args.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for &key in KEEP {
        let Some(val) = map.get(key) else {
            continue;
        };
        let compacted = match val {
            Value::String(s) => Value::String(clip_utf8_bytes(s, 256)),
            Value::Bool(_) | Value::Number(_) | Value::Null => val.clone(),
            _ => continue,
        };
        out.insert(key.to_owned(), compacted);
    }
    Value::Object(out)
}

fn clip_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn format_elapsed_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ToolBlock {
    pub id: BlockId,
    pub items: Vec<ToolItem>,
    pub selected: usize,
    pub follow_tool: bool,
    pub scroll_from_bottom: usize,
    pub active: bool,
    pub collapsed: bool,
    pub expanded_override: Option<bool>,
    pub pruned: bool,
}

impl ToolBlock {
    pub(super) fn is_collapsed(&self) -> bool {
        if self.active {
            return false;
        }
        match self.expanded_override {
            Some(expanded) => !expanded,
            None => self.collapsed,
        }
    }

    pub(super) fn selected_item(&self) -> Option<&ToolItem> {
        self.items.get(self.selected).or_else(|| self.items.last())
    }

    pub(super) fn step_range_label(&self) -> String {
        let mut min_step = usize::MAX;
        let mut max_step = 0usize;
        for item in &self.items {
            if item.step > 0 {
                min_step = min_step.min(item.step);
                max_step = max_step.max(item.step);
            }
        }
        if min_step == usize::MAX {
            "Tools".into()
        } else if min_step == max_step {
            format!("Tools · step {min_step}")
        } else {
            format!("Tools · step {min_step}..{max_step}")
        }
    }

    pub(super) fn total_elapsed_ms(&self) -> u64 {
        self.items
            .iter()
            .map(|item| {
                item.elapsed_ms.unwrap_or_else(|| {
                    if item.status == ToolStatus::Running {
                        item.started_at
                            .map_or(0, |s| s.elapsed().as_millis() as u64)
                    } else {
                        0
                    }
                })
            })
            .sum()
    }

    pub(super) fn plain_summary(&self) -> String {
        let count = self.items.len();
        let names: Vec<String> = self
            .items
            .iter()
            .take(4)
            .map(|i| {
                let t = i.target_summary();
                if t.is_empty() {
                    i.tool.clone()
                } else {
                    format!("{} {}", i.tool, clip(&t, 40))
                }
            })
            .collect();
        let suffix = if count > 4 {
            format!(" (+{} more)", count - 4)
        } else {
            String::new()
        };
        format!("[tools:{count}] {}{suffix}", names.join(", "))
    }

    pub(super) fn collapsed_label(&self) -> String {
        let n = self.items.len();
        let noun = if n == 1 { "tool" } else { "tools" };
        let elapsed = format_elapsed_ms(self.total_elapsed_ms());
        format!("▸ {n} {noun} · {elapsed} · click or ←/→")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InspectorBadgeAction {
    ToggleFollow,
    JumpToBottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockRowKind {
    CollapsedHeader,
    Header,
    ToolItem(usize),
    Preview,
    ApprovalFooter,
    Padding,
    BottomBorder,
}

#[derive(Clone, Debug)]
pub(super) struct RenderedBlockLeft {
    pub lines: Vec<markdown::Line>,
    pub row_kinds: Vec<BlockRowKind>,
    pub has_bottom_border: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PaintedToolBlock {
    pub block_id: BlockId,
    /// Screen row span of the block (excluding bottom border).
    pub vis_row_start: u16,
    pub vis_row_end: u16,
    /// Left column span (full width in single-column, left pane in split).
    pub left_col_start: u16,
    pub left_col_end: u16,
    /// Right inspector rect if inline split is active for this block.
    pub inspector_rect: Option<Rect>,
    /// Divider column if inline split is active for this block.
    pub divider_x: Option<u16>,
    /// Screen row -> BlockRowKind mapping for visible left rows.
    pub row_map: Vec<(u16, BlockRowKind)>,
    /// Optional clickable footer badge hit range `(row, col_start, col_end, action)`.
    pub live_badge: Option<(u16, u16, u16, InspectorBadgeAction)>,
    /// Number of header rows at the top of `inspector_rect` (1 when split).
    pub inspector_header_rows: u16,
    /// Number of visible body rows in `inspector_rect`.
    pub inspector_body_rows: usize,
    /// First detail line index currently painted in `inspector_rect`.
    pub inspector_body_start: usize,
    /// Cached detail lines snapshot painted in `inspector_rect`.
    pub inspector_detail_lines: Option<Rc<Vec<markdown::Line>>>,
}

#[derive(Clone, Debug)]
pub(super) struct PaintedInspectorPane {
    pub block_id: BlockId,
    pub rect: Rect,
    pub header_rows: u16,
    pub body_rows: usize,
    pub body_start: usize,
    pub detail_lines: Rc<Vec<markdown::Line>>,
    pub live_badge: Option<(u16, u16, u16, InspectorBadgeAction)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DetailCacheKey {
    block_id: BlockId,
    /// `None` = continuous live feed (`follow_tool && items.len() > 1`); `Some(i)` = single tool `i`.
    selected: Option<usize>,
    item_rev: u64,
    inner_width: usize,
}

#[derive(Default)]
struct DetailCache {
    key: Option<DetailCacheKey>,
    highlight_rev: u64,
    had_pending: bool,
    lines: Rc<Vec<markdown::Line>>,
}

type ItemCacheEntry = (u64, usize, u64, bool, Rc<Vec<markdown::Line>>);

pub(super) struct ToolBlockStore {
    blocks: HashMap<BlockId, ToolBlock>,
    order: Vec<BlockId>,
    next_id: u64,
    active_block: Option<BlockId>,
    focused: Option<BlockId>,
    layout_rev: u64,
    content_rev: u64,
    payload_bytes: usize,
    max_payload_bytes: usize,
    detail_cache: RefCell<DetailCache>,
    /// Per-item wrapped-lines cache keyed by `(block_id, item_idx) -> (item_rev, width, hl_rev, had_pending, lines)`.
    /// Prevents re-wrapping completed earlier tools on every live `tool_output_chunk` in Feed mode.
    item_detail_cache: RefCell<HashMap<(BlockId, usize), ItemCacheEntry>>,
    #[cfg(test)]
    detail_cache_hits: Cell<u64>,
    #[cfg(test)]
    detail_cache_misses: Cell<u64>,
    #[cfg(test)]
    item_wrap_count: Cell<u64>,
}

impl Default for ToolBlockStore {
    fn default() -> Self {
        Self {
            blocks: HashMap::new(),
            order: Vec::new(),
            next_id: 1,
            active_block: None,
            focused: None,
            layout_rev: 0,
            content_rev: 0,
            payload_bytes: 0,
            max_payload_bytes: MAX_PAYLOAD_BYTES,
            detail_cache: RefCell::new(DetailCache::default()),
            item_detail_cache: RefCell::new(HashMap::new()),
            #[cfg(test)]
            detail_cache_hits: Cell::new(0),
            #[cfg(test)]
            detail_cache_misses: Cell::new(0),
            #[cfg(test)]
            item_wrap_count: Cell::new(0),
        }
    }
}

impl ToolBlockStore {
    pub(super) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub(super) fn layout_rev(&self) -> u64 {
        self.layout_rev
    }

    pub(super) fn content_rev(&self) -> u64 {
        self.content_rev
    }

    pub(super) fn focused_id(&self) -> Option<BlockId> {
        self.focused
            .filter(|id| self.blocks.contains_key(id))
            .or(self.active_block.filter(|id| self.blocks.contains_key(id)))
            .or_else(|| self.order.last().copied())
    }

    pub(super) fn get(&self, id: BlockId) -> Option<&ToolBlock> {
        self.blocks.get(&id)
    }

    pub(super) fn active_block(&self) -> Option<&ToolBlock> {
        self.active_block.and_then(|id| self.blocks.get(&id))
    }

    pub(super) fn focused_block(&self) -> Option<&ToolBlock> {
        self.focused_id().and_then(|id| self.blocks.get(&id))
    }

    pub(super) fn set_focused(&mut self, id: BlockId) {
        if self.blocks.contains_key(&id) {
            self.focused = Some(id);
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    pub(super) fn bump_layout_rev(&mut self) {
        self.layout_rev = self.layout_rev.wrapping_add(1);
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    pub(super) fn has_running_tools(&self) -> bool {
        self.active_block
            .and_then(|id| self.blocks.get(&id))
            .is_some_and(|block| {
                block
                    .items
                    .iter()
                    .any(|item| item.status == ToolStatus::Running)
            })
    }

    pub(super) fn clear(&mut self) {
        self.blocks.clear();
        self.order.clear();
        self.active_block = None;
        self.focused = None;
        self.payload_bytes = 0;
        self.layout_rev = self.layout_rev.wrapping_add(1);
        self.content_rev = self.content_rev.wrapping_add(1);
        let mut cache = self.detail_cache.borrow_mut();
        cache.key = None;
        cache.lines = Rc::new(Vec::new());
        self.item_detail_cache.borrow_mut().clear();
    }

    pub(super) fn ensure_active_block(&mut self, follow_default: bool) -> BlockId {
        if let Some(id) = self.active_block {
            if self.blocks.contains_key(&id) {
                return id;
            }
        }
        let id = BlockId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let block = ToolBlock {
            id,
            items: Vec::new(),
            selected: 0,
            follow_tool: follow_default,
            scroll_from_bottom: 0,
            active: true,
            collapsed: false,
            expanded_override: None,
            pruned: false,
        };
        self.blocks.insert(id, block);
        self.order.push(id);
        self.active_block = Some(id);
        if follow_default || self.focused.is_none() {
            self.focused = Some(id);
        }
        // Active blocks render into TranscriptLines::active_tail, never sealed LineCache.items.
        self.content_rev = self.content_rev.wrapping_add(1);
        id
    }

    pub(super) fn on_approval_requested(
        &mut self,
        approval_id: String,
        step: usize,
        calls: &[Value],
        call_indices: &[usize],
        source: Option<String>,
        follow_default: bool,
    ) -> BlockId {
        let block_id = self.ensure_active_block(follow_default);
        let mut added_bytes = 0usize;
        if let Some(block) = self.blocks.get_mut(&block_id) {
            let first_new = block.items.len();
            for (i, call) in calls.iter().enumerate() {
                let tool = call["tool"].as_str().unwrap_or("tool").to_owned();
                let args = call["args"].clone();
                let index = call_indices.get(i).copied().unwrap_or(i);
                let item = ToolItem {
                    step,
                    index,
                    approval_id: Some(approval_id.clone()),
                    source: source.clone(),
                    subagent_id: None,
                    tool,
                    args,
                    status: ToolStatus::Pending,
                    started_at: None,
                    elapsed_ms: None,
                    live_chunks: Vec::new(),
                    live_bytes: 0,
                    live_capped: false,
                    subagent_logs: Vec::new(),
                    subagent_log_bytes: 0,
                    output: None,
                    ok: None,
                    payload_pruned: false,
                    rev: 1,
                };
                added_bytes = added_bytes.saturating_add(item.estimated_payload_bytes());
                block.items.push(item);
            }
            if block.follow_tool && !calls.is_empty() {
                block.selected = first_new;
                block.scroll_from_bottom = 0;
            }
        }
        self.payload_bytes = self.payload_bytes.saturating_add(added_bytes);
        if follow_default || self.focused.is_none() {
            self.focused = Some(block_id);
        }
        self.content_rev = self.content_rev.wrapping_add(1);
        block_id
    }

    pub(super) fn on_approval_resolved(&mut self, approval_id: &str, approved: bool) {
        let mut changed = false;
        let mut hit_sealed = false;
        let ids: Vec<BlockId> = self
            .active_block
            .into_iter()
            .chain(self.order.iter().rev().copied())
            .collect();
        for id in ids {
            let Some(block) = self.blocks.get_mut(&id) else {
                continue;
            };
            let mut block_hit = false;
            for item in &mut block.items {
                if item.approval_id.as_deref() == Some(approval_id)
                    && item.status == ToolStatus::Pending
                {
                    block_hit = true;
                    if !approved {
                        item.status = ToolStatus::Rejected;
                        item.ok = Some(false);
                        item.rev = item.rev.wrapping_add(1);
                        changed = true;
                    } else if item.source.is_some() {
                        // Subagent ToolStarted/ToolFinished are not forwarded to parent;
                        // mark approved subagent items Done(true) so they never stick Pending.
                        item.status = ToolStatus::Done(true);
                        item.ok = Some(true);
                        item.rev = item.rev.wrapping_add(1);
                        changed = true;
                    }
                }
            }
            if block_hit {
                if Some(id) != self.active_block {
                    hit_sealed = true;
                }
                break;
            }
        }
        if changed {
            if hit_sealed {
                self.layout_rev = self.layout_rev.wrapping_add(1);
            }
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    pub(super) fn on_tool_started(
        &mut self,
        step: usize,
        index: usize,
        tool: String,
        args: Value,
        follow_default: bool,
    ) -> BlockId {
        let block_id = self.ensure_active_block(follow_default);
        let mut added_bytes = 0usize;
        let mut sub_bytes = 0usize;
        if let Some(block) = self.blocks.get_mut(&block_id) {
            let matched_pos = block
                .items
                .iter()
                .position(|item| {
                    item.source.is_none()
                        && item.status == ToolStatus::Pending
                        && item.step == step
                        && item.index == index
                })
                .or_else(|| {
                    block.items.iter().position(|item| {
                        item.source.is_none()
                            && item.status == ToolStatus::Pending
                            && item.step == step
                            && item.tool == tool
                            && item.args == args
                    })
                });
            let pos = if let Some(pos) = matched_pos {
                let item = &mut block.items[pos];
                sub_bytes = item.estimated_payload_bytes();
                item.status = ToolStatus::Running;
                item.started_at = Some(Instant::now());
                item.args = args;
                item.rev = item.rev.wrapping_add(1);
                added_bytes = item.estimated_payload_bytes();
                pos
            } else {
                let item = ToolItem {
                    step,
                    index,
                    approval_id: None,
                    source: None,
                    subagent_id: None,
                    tool,
                    args,
                    status: ToolStatus::Running,
                    started_at: Some(Instant::now()),
                    elapsed_ms: None,
                    live_chunks: Vec::new(),
                    live_bytes: 0,
                    live_capped: false,
                    subagent_logs: Vec::new(),
                    subagent_log_bytes: 0,
                    output: None,
                    ok: None,
                    payload_pruned: false,
                    rev: 1,
                };
                added_bytes = item.estimated_payload_bytes();
                block.items.push(item);
                block.items.len() - 1
            };
            if block.follow_tool {
                block.selected = pos;
                block.scroll_from_bottom = 0;
            }
        }
        self.payload_bytes = self
            .payload_bytes
            .saturating_sub(sub_bytes)
            .saturating_add(added_bytes);
        if follow_default || self.focused.is_none() {
            self.focused = Some(block_id);
        }
        self.content_rev = self.content_rev.wrapping_add(1);
        block_id
    }

    pub(super) fn on_tool_output_chunk(
        &mut self,
        step: usize,
        index: usize,
        stream: OutputStream,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        let Some(block_id) = self.active_block else {
            return;
        };
        let mut added_bytes = 0usize;
        if let Some(block) = self.blocks.get_mut(&block_id) {
            let pos = block
                .items
                .iter()
                .rposition(|item| item.source.is_none() && item.step == step && item.index == index)
                .or_else(|| {
                    block.items.iter().rposition(|item| {
                        item.source.is_none() && item.status == ToolStatus::Running
                    })
                });
            if let Some(pos) = pos {
                let item = &mut block.items[pos];
                if item.live_bytes >= MAX_LIVE_BYTES {
                    if !item.live_capped {
                        item.live_capped = true;
                        item.rev = item.rev.wrapping_add(1);
                        self.content_rev = self.content_rev.wrapping_add(1);
                    }
                    return;
                }
                let remaining = MAX_LIVE_BYTES - item.live_bytes;
                let slice = if text.len() <= remaining {
                    text.to_owned()
                } else {
                    item.live_capped = true;
                    clip_utf8_bytes(text, remaining)
                };
                if !slice.is_empty() {
                    added_bytes = slice.len();
                    item.live_bytes += slice.len();
                    if let Some((last_stream, last_text)) = item.live_chunks.last_mut() {
                        if *last_stream == stream {
                            last_text.push_str(&slice);
                        } else {
                            item.live_chunks.push((stream, slice));
                        }
                    } else {
                        item.live_chunks.push((stream, slice));
                    }
                    item.rev = item.rev.wrapping_add(1);
                    self.content_rev = self.content_rev.wrapping_add(1);
                }
            }
        }
        self.payload_bytes = self.payload_bytes.saturating_add(added_bytes);
        self.enforce_payload_budget();
    }

    pub(super) fn on_tool_finished(
        &mut self,
        step: usize,
        index: usize,
        tool: &str,
        ok: bool,
        output: String,
        elapsed_ms: u64,
    ) {
        let Some(block_id) = self.active_block.or_else(|| self.order.last().copied()) else {
            return;
        };
        let mut delta_add = 0usize;
        let mut delta_sub = 0usize;
        if let Some(block) = self.blocks.get_mut(&block_id) {
            let pos = block
                .items
                .iter()
                .position(|item| {
                    item.source.is_none()
                        && item.step == step
                        && item.index == index
                        && matches!(item.status, ToolStatus::Running | ToolStatus::Pending)
                })
                .or_else(|| {
                    block.items.iter().rposition(|item| {
                        item.source.is_none() && item.step == step && item.index == index
                    })
                })
                .or_else(|| {
                    block.items.iter().rposition(|item| {
                        item.source.is_none()
                            && item.status == ToolStatus::Running
                            && item.tool == tool
                    })
                });
            if let Some(pos) = pos {
                let item = &mut block.items[pos];
                delta_sub = item.output.as_ref().map_or(0, String::len);
                delta_add = output.len();
                // Once final non-empty output arrives, release live streaming chunks immediately
                // so completed tools never double-store up to 64 KiB of duplicate RAM.
                if !output.is_empty() && !item.live_chunks.is_empty() {
                    delta_sub = delta_sub.saturating_add(item.live_bytes);
                    item.live_chunks.clear();
                    item.live_chunks.shrink_to_fit();
                    item.live_bytes = 0;
                    item.live_capped = false;
                }
                item.status = ToolStatus::Done(ok);
                item.ok = Some(ok);
                item.elapsed_ms = Some(elapsed_ms);
                item.output = Some(output);
                item.rev = item.rev.wrapping_add(1);
                self.content_rev = self.content_rev.wrapping_add(1);
            }
        }
        self.payload_bytes = self
            .payload_bytes
            .saturating_sub(delta_sub)
            .saturating_add(delta_add);
        self.enforce_payload_budget();
    }

    pub(super) fn on_subagent_started(&mut self, sub_id: &str, label: &str) {
        if sub_id.is_empty() {
            return;
        }
        let ids: Vec<BlockId> = self
            .active_block
            .into_iter()
            .chain(self.order.iter().rev().copied())
            .collect();
        for id in ids {
            let Some(block) = self.blocks.get_mut(&id) else {
                continue;
            };
            let pos = block
                .items
                .iter()
                .position(|item| {
                    item.tool == "spawn_agent"
                        && item.subagent_id.is_none()
                        && item.status == ToolStatus::Running
                        && item.args["label"].as_str() == Some(label)
                })
                .or_else(|| {
                    block.items.iter().position(|item| {
                        item.tool == "spawn_agent"
                            && item.subagent_id.is_none()
                            && item.status == ToolStatus::Running
                    })
                });
            if let Some(pos) = pos {
                let item = &mut block.items[pos];
                item.subagent_id = Some(sub_id.to_owned());
                let entry = format!("started [{label}]");
                let added = push_subagent_log(item, entry);
                item.rev = item.rev.wrapping_add(1);
                self.payload_bytes = self.payload_bytes.saturating_add(added);
                self.content_rev = self.content_rev.wrapping_add(1);
                return;
            }
        }
    }

    pub(super) fn on_subagent_progress(&mut self, sub_id: &str, phase: &str, detail: &str) {
        if sub_id.is_empty() {
            return;
        }
        let line = if detail.is_empty() {
            phase.to_owned()
        } else {
            format!("{phase} · {detail}")
        };
        let ids: Vec<BlockId> = self
            .active_block
            .into_iter()
            .chain(self.order.iter().rev().copied())
            .collect();
        for id in ids {
            let Some(block) = self.blocks.get_mut(&id) else {
                continue;
            };
            if let Some(item) = block
                .items
                .iter_mut()
                .find(|item| item.subagent_id.as_deref() == Some(sub_id))
            {
                let added = push_subagent_log(item, line);
                if added > 0 {
                    item.rev = item.rev.wrapping_add(1);
                    self.payload_bytes = self.payload_bytes.saturating_add(added);
                    self.content_rev = self.content_rev.wrapping_add(1);
                }
                return;
            }
        }
    }

    pub(super) fn on_subagent_finished(&mut self, sub_id: &str, ok: bool, elapsed_ms: u64) {
        if sub_id.is_empty() {
            return;
        }
        let line = format!(
            "finished · {} · {}",
            if ok { "ok" } else { "err" },
            format_elapsed_ms(elapsed_ms)
        );
        let ids: Vec<BlockId> = self
            .active_block
            .into_iter()
            .chain(self.order.iter().rev().copied())
            .collect();
        for id in ids {
            let Some(block) = self.blocks.get_mut(&id) else {
                continue;
            };
            if let Some(item) = block
                .items
                .iter_mut()
                .find(|item| item.subagent_id.as_deref() == Some(sub_id))
            {
                let added = push_subagent_log(item, line);
                if added > 0 {
                    item.rev = item.rev.wrapping_add(1);
                    self.payload_bytes = self.payload_bytes.saturating_add(added);
                    self.content_rev = self.content_rev.wrapping_add(1);
                }
                return;
            }
        }
    }

    pub(super) fn seal_active_block(
        &mut self,
        after_turn: AfterTurnPolicy,
        cancel_incomplete: bool,
    ) -> Option<(BlockId, String)> {
        let id = self.active_block.take()?;
        let is_empty = self
            .blocks
            .get(&id)
            .is_none_or(|block| block.items.is_empty());
        if is_empty {
            self.blocks.remove(&id);
            self.order.retain(|b| *b != id);
            if self.focused == Some(id) {
                self.focused = self.order.last().copied();
            }
            self.content_rev = self.content_rev.wrapping_add(1);
            return None;
        }
        let plain = if let Some(block) = self.blocks.get_mut(&id) {
            if cancel_incomplete {
                for item in &mut block.items {
                    if matches!(item.status, ToolStatus::Pending | ToolStatus::Running) {
                        item.status = ToolStatus::Cancelled;
                        item.ok = Some(false);
                        item.rev = item.rev.wrapping_add(1);
                    }
                }
            }
            block.active = false;
            if after_turn == AfterTurnPolicy::Collapse && block.expanded_override.is_none() {
                block.collapsed = true;
            }
            block.plain_summary()
        } else {
            return None;
        };
        // Newly sealed blocks are appended incrementally to Transcript::items by the caller,
        // so prior sealed LineCache items stay intact without a full transcript re-wrap.
        self.content_rev = self.content_rev.wrapping_add(1);
        self.enforce_payload_budget();
        Some((id, plain))
    }

    pub(super) fn build_from_turn_steps(
        &mut self,
        steps: &[AgentStepRecord],
        inspector_pruned: bool,
        after_turn: AfterTurnPolicy,
        follow_default: bool,
    ) -> Option<(BlockId, String)> {
        let mut items = Vec::new();
        let mut added_bytes = 0usize;
        for step in steps {
            for (idx, tc) in step.tool_calls.iter().enumerate() {
                let obs = step.observations.get(idx);
                let (status, ok, elapsed_ms, output) = match obs {
                    Some(obs) => (
                        ToolStatus::Done(obs.ok),
                        Some(obs.ok),
                        Some(obs.elapsed_ms),
                        (!inspector_pruned).then(|| obs.output.clone()),
                    ),
                    None => (ToolStatus::Cancelled, Some(false), None, None),
                };
                let item = ToolItem {
                    step: step.step,
                    index: idx,
                    approval_id: None,
                    source: None,
                    subagent_id: None,
                    tool: tc.tool.clone(),
                    args: tc.args.clone(),
                    status,
                    started_at: None,
                    elapsed_ms,
                    live_chunks: Vec::new(),
                    live_bytes: 0,
                    live_capped: false,
                    subagent_logs: Vec::new(),
                    subagent_log_bytes: 0,
                    output,
                    ok,
                    payload_pruned: inspector_pruned,
                    rev: 1,
                };
                added_bytes = added_bytes.saturating_add(item.estimated_payload_bytes());
                items.push(item);
            }
        }
        if items.is_empty() {
            return None;
        }
        let id = BlockId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let selected = items.len().saturating_sub(1);
        let block = ToolBlock {
            id,
            items,
            selected,
            follow_tool: follow_default,
            scroll_from_bottom: 0,
            active: false,
            collapsed: after_turn == AfterTurnPolicy::Collapse,
            expanded_override: None,
            pruned: inspector_pruned,
        };
        let plain = block.plain_summary();
        self.blocks.insert(id, block);
        self.order.push(id);
        self.focused = Some(id);
        self.payload_bytes = self.payload_bytes.saturating_add(added_bytes);
        self.content_rev = self.content_rev.wrapping_add(1);
        self.enforce_payload_budget();
        Some((id, plain))
    }

    pub(super) fn remove_blocks(&mut self, evicted: &[BlockId]) {
        let mut changed = false;
        for &id in evicted {
            if self.active_block == Some(id) {
                continue;
            }
            if let Some(block) = self.blocks.remove(&id) {
                let freed: usize = block
                    .items
                    .iter()
                    .map(ToolItem::estimated_payload_bytes)
                    .sum();
                self.payload_bytes = self.payload_bytes.saturating_sub(freed);
                self.order.retain(|b| *b != id);
                if self.focused == Some(id) {
                    self.focused = self.active_block.or_else(|| self.order.last().copied());
                }
                let mut cache = self.detail_cache.borrow_mut();
                if cache.key.is_some_and(|k| k.block_id == id) {
                    cache.key = None;
                    cache.lines = Rc::new(Vec::new());
                }
                self.item_detail_cache
                    .borrow_mut()
                    .retain(|(bid, _), _| *bid != id);
                changed = true;
            }
        }
        if changed {
            self.layout_rev = self.layout_rev.wrapping_add(1);
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    pub(super) fn select_item(&mut self, block_id: BlockId, item_idx: usize) {
        let mut grew_layout = false;
        if let Some(block) = self.blocks.get_mut(&block_id) {
            if item_idx < block.items.len() {
                block.selected = item_idx;
                block.follow_tool = false;
                block.scroll_from_bottom = 0;
                if !block.active && block.expanded_override != Some(true) {
                    block.expanded_override = Some(true);
                    grew_layout = true;
                }
            }
        }
        self.focused = Some(block_id);
        if grew_layout {
            self.layout_rev = self.layout_rev.wrapping_add(1);
        }
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    pub(super) fn toggle_collapse(&mut self, block_id: BlockId) {
        if let Some(block) = self.blocks.get_mut(&block_id) {
            if block.active {
                return;
            }
            let currently_collapsed = block.is_collapsed();
            block.expanded_override = Some(currently_collapsed);
            self.focused = Some(block_id);
            self.layout_rev = self.layout_rev.wrapping_add(1);
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    pub(super) fn select_prev_next(&mut self, next: bool) -> bool {
        let Some(id) = self.focused_id() else {
            return false;
        };
        let mut layout_changed = false;
        let changed = if let Some(block) = self.blocks.get_mut(&id) {
            if block.items.is_empty() {
                false
            } else {
                if block.is_collapsed() {
                    block.expanded_override = Some(true);
                    layout_changed = true;
                }
                let last_idx = block.items.len().saturating_sub(1);
                if next && block.active && !block.follow_tool && block.selected >= last_idx {
                    // Pressing next (`→`) past the final tool re-engages live Follow.
                    block.follow_tool = true;
                    block.scroll_from_bottom = 0;
                    true
                } else {
                    let next_idx = if next {
                        (block.selected + 1).min(last_idx)
                    } else {
                        block.selected.saturating_sub(1)
                    };
                    block.follow_tool = false;
                    block.scroll_from_bottom = 0;
                    let moved = next_idx != block.selected || layout_changed;
                    block.selected = next_idx;
                    moved
                }
            }
        } else {
            false
        };
        if layout_changed {
            self.layout_rev = self.layout_rev.wrapping_add(1);
        }
        if changed {
            self.content_rev = self.content_rev.wrapping_add(1);
        }
        changed
    }

    /// Toggle automatic tracking (`follow_tool`) on the active tool block:
    /// - If inspecting an older tool, scrolled up, or viewing a past block while a turn is active:
    ///   focus the active block, jump to its latest tool + live tail, and resume Follow.
    /// - If already actively following the latest tool at live tail: pause (`follow_tool = false`).
    pub(super) fn toggle_follow(&mut self) -> bool {
        if let Some(active_id) = self.active_block {
            let focused_is_active = self.focused_id() == Some(active_id);
            if let Some(block) = self.blocks.get_mut(&active_id) {
                let last_idx = block.items.len().saturating_sub(1);
                let already_live_following = focused_is_active
                    && block.follow_tool
                    && block.scroll_from_bottom == 0
                    && (block.items.is_empty() || block.selected >= last_idx);
                if already_live_following {
                    block.follow_tool = false;
                } else {
                    block.follow_tool = true;
                    block.selected = last_idx;
                    block.scroll_from_bottom = 0;
                    self.focused = Some(active_id);
                }
                self.content_rev = self.content_rev.wrapping_add(1);
                return true;
            }
        }
        if let Some(id) = self.focused_id() {
            if let Some(block) = self.blocks.get_mut(&id) {
                if block.scroll_from_bottom > 0 {
                    block.scroll_from_bottom = 0;
                    self.content_rev = self.content_rev.wrapping_add(1);
                    return true;
                }
            }
        }
        false
    }

    /// Scroll a block's right Inspector pane without touching `follow_tool`.
    pub(super) fn scroll_block(
        &mut self,
        block_id: BlockId,
        up: bool,
        lines: usize,
        max_scroll: usize,
    ) -> bool {
        let Some(block) = self.blocks.get_mut(&block_id) else {
            return false;
        };
        let current = block.scroll_from_bottom.min(max_scroll);
        let next = if up {
            current.saturating_add(lines).min(max_scroll)
        } else {
            current.saturating_sub(lines)
        };
        block.scroll_from_bottom = next;
        if next != current {
            self.content_rev = self.content_rev.wrapping_add(1);
            true
        } else {
            false
        }
    }

    pub(super) fn jump_to_live(&mut self, block_id: BlockId) {
        if let Some(block) = self.blocks.get_mut(&block_id) {
            block.scroll_from_bottom = 0;
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    pub(super) fn apply_after_turn_policy(&mut self, policy: AfterTurnPolicy) {
        let mut changed = false;
        for block in self.blocks.values_mut() {
            if !block.active && block.expanded_override.is_none() {
                let next_collapsed = policy == AfterTurnPolicy::Collapse;
                if block.collapsed != next_collapsed {
                    block.collapsed = next_collapsed;
                    changed = true;
                }
            }
        }
        if changed {
            self.layout_rev = self.layout_rev.wrapping_add(1);
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    pub(super) fn apply_follow_default(&mut self, follow: bool) {
        if let Some(id) = self.active_block {
            if let Some(block) = self.blocks.get_mut(&id) {
                block.follow_tool = follow;
                if follow && !block.items.is_empty() {
                    block.selected = block.items.len() - 1;
                }
                self.content_rev = self.content_rev.wrapping_add(1);
            }
        }
    }

    fn enforce_payload_budget(&mut self) {
        while self.payload_bytes > self.max_payload_bytes {
            let candidate = self
                .order
                .iter()
                .copied()
                .find(|id| {
                    Some(*id) != self.focused
                        && self.blocks.get(id).is_some_and(|b| !b.active && !b.pruned)
                })
                .or_else(|| {
                    self.order
                        .iter()
                        .copied()
                        .find(|id| self.blocks.get(id).is_some_and(|b| !b.active && !b.pruned))
                });
            let Some(victim_id) = candidate else {
                break;
            };
            let mut freed = 0usize;
            if let Some(block) = self.blocks.get_mut(&victim_id) {
                for item in &mut block.items {
                    freed = freed.saturating_add(item.prune_heavy_payload());
                }
                block.pruned = true;
            }
            self.payload_bytes = self.payload_bytes.saturating_sub(freed);
            let mut cache = self.detail_cache.borrow_mut();
            if cache.key.is_some_and(|k| k.block_id == victim_id) {
                cache.key = None;
                cache.lines = Rc::new(Vec::new());
            }
            self.item_detail_cache
                .borrow_mut()
                .retain(|(bid, _), _| *bid != victim_id);
            if freed == 0 {
                break;
            }
        }
    }

    /// Return cached wrapped lines for a single `(block_id, item_idx)` at `inner_width`,
    /// wrapping at most once per `(item.rev, inner_width)` (plus once more when pending async
    /// syntax highlights arrive).
    fn cached_item_lines(
        &self,
        block_id: BlockId,
        idx: usize,
        item: &ToolItem,
        inner_width: usize,
        highlights: &HighlightCache,
    ) -> (Rc<Vec<markdown::Line>>, bool) {
        let hl_rev = highlights.rev();
        {
            let map = self.item_detail_cache.borrow();
            if let Some((rev, w, cached_hl_rev, had_pending, lines)) = map.get(&(block_id, idx)) {
                if *rev == item.rev
                    && *w == inner_width
                    && (!*had_pending || *cached_hl_rev == hl_rev)
                {
                    return (Rc::clone(lines), *had_pending);
                }
            }
        }
        #[cfg(test)]
        self.item_wrap_count.set(self.item_wrap_count.get() + 1);
        let (rendered_vec, had_pending) = render_item_detail_lines(item, inner_width, highlights);
        let rendered = Rc::new(rendered_vec);
        let mut map = self.item_detail_cache.borrow_mut();
        if map.len() >= ITEM_CACHE_CAP && !map.contains_key(&(block_id, idx)) {
            map.retain(|(bid, _), _| *bid == block_id);
            if map.len() >= ITEM_CACHE_CAP {
                map.clear();
            }
        }
        map.insert(
            (block_id, idx),
            (
                item.rev,
                inner_width,
                hl_rev,
                had_pending,
                Rc::clone(&rendered),
            ),
        );
        (rendered, had_pending)
    }

    /// Lazily wrap and cache detail lines for `block_id`.
    /// - When `block.follow_tool && block.items.len() > 1`: renders a continuous scrolling feed
    ///   of recent tools in the block using per-item cached wrapped lines (zero re-wrap of completed items).
    /// - When `block.follow_tool == false` (or single item): renders ONLY the selected tool in full.
    pub(super) fn detail_lines(
        &self,
        block_id: BlockId,
        inner_width: usize,
        highlights: &HighlightCache,
    ) -> Rc<Vec<markdown::Line>> {
        const MAX_FEED_ITEMS: usize = 8;

        let inner_width = inner_width.max(1);
        let hl_rev = highlights.rev();
        let Some(block) = self.blocks.get(&block_id) else {
            return Rc::new(Vec::new());
        };
        let Some(item) = block.selected_item() else {
            return Rc::new(Vec::new());
        };
        let feed_mode = block.follow_tool && block.items.len() > 1;
        let selected_idx = block.selected.min(block.items.len().saturating_sub(1));
        let (selected_key, item_rev) = if feed_mode {
            let start_idx = block.items.len().saturating_sub(MAX_FEED_ITEMS);
            let mut h = (block.items.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            for it in &block.items[start_idx..] {
                h = h.rotate_left(7) ^ it.rev;
            }
            if block.pruned {
                h ^= 1u64 << 63;
            }
            (None, h)
        } else {
            (Some(selected_idx), item.rev)
        };
        let key = DetailCacheKey {
            block_id,
            selected: selected_key,
            item_rev,
            inner_width,
        };
        {
            let cache = self.detail_cache.borrow();
            if cache.key == Some(key) && (!cache.had_pending || cache.highlight_rev == hl_rev) {
                #[cfg(test)]
                self.detail_cache_hits.set(self.detail_cache_hits.get() + 1);
                return Rc::clone(&cache.lines);
            }
        }
        #[cfg(test)]
        self.detail_cache_misses
            .set(self.detail_cache_misses.get() + 1);
        let (rendered, had_pending) = if feed_mode {
            let (lines, pending) =
                self.render_block_feed_lines(block, inner_width, MAX_FEED_ITEMS, highlights);
            (Rc::new(lines), pending)
        } else {
            self.cached_item_lines(block_id, selected_idx, item, inner_width, highlights)
        };
        let mut cache = self.detail_cache.borrow_mut();
        cache.key = Some(key);
        cache.highlight_rev = hl_rev;
        cache.had_pending = had_pending;
        cache.lines = Rc::clone(&rendered);
        rendered
    }

    fn render_block_feed_lines(
        &self,
        block: &ToolBlock,
        width: usize,
        max_items: usize,
        highlights: &HighlightCache,
    ) -> (Vec<markdown::Line>, bool) {
        let w = width.max(1);
        let total = block.items.len();
        if total == 0 {
            return (Vec::new(), false);
        }
        let start_idx = total.saturating_sub(max_items);
        let mut out = Vec::new();
        let mut any_pending = false;
        if start_idx > 0 {
            out.extend(markdown::literal(
                &format!("… ({start_idx} earlier tool(s) · press ←/→ or click in list to inspect)"),
                Face::MUTED,
                w,
            ));
            out.push(Vec::new());
        }
        for (idx, item) in block.items.iter().enumerate().skip(start_idx) {
            if !out.is_empty() {
                out.push(Vec::new());
            }
            let (icon, icon_face) = item.status.icon("⠋");
            let mut header_parts: Vec<(String, Face)> = vec![
                (format!("── [{}/{}] ", idx + 1, total), Face::SUBTLE),
                (format!("{icon} "), icon_face),
                (item.tool.clone(), Face::ACCENT_BOLD),
            ];
            let target = item.target_summary();
            let opt_suffix = item.compact_option_suffix();
            if !target.is_empty() {
                header_parts.push((" · ".into(), Face::MUTED));
                let budget = (w / 2).saturating_sub(opt_suffix.width()).max(8);
                header_parts.push((clip_preserving_tail(&target, budget), Face::TEXT));
            }
            if !opt_suffix.is_empty() {
                header_parts.push((
                    opt_suffix,
                    if item.tool == "read_file" {
                        Face::ACCENT
                    } else {
                        Face::MUTED
                    },
                ));
            }
            if let Some(el) = item.elapsed_display() {
                header_parts.push((format!(" ({el})"), Face::MUTED));
            }
            header_parts.push((" ──".into(), Face::SUBTLE));
            let refs: Vec<(&str, Face)> =
                header_parts.iter().map(|(t, f)| (t.as_str(), *f)).collect();
            out.push(clip_line_spans(&refs, w));

            // Per-item cached lines (0 re-wraps of completed items); abbreviate only very long
            // items in Live Feed so RAM/CPU stay lean while single-tool selection (`←/→`) shows 100%.
            let (item_lines, pending) = self.cached_item_lines(block.id, idx, item, w, highlights);
            any_pending |= pending;
            let is_latest = idx + 1 == total;
            let (max_lines, head_keep, tail_keep) = if is_latest {
                (FEED_LATEST_MAX_LINES, FEED_LATEST_HEAD, FEED_LATEST_TAIL)
            } else {
                (FEED_PRIOR_MAX_LINES, FEED_PRIOR_HEAD, FEED_PRIOR_TAIL)
            };
            if item_lines.len() <= max_lines {
                out.extend(item_lines.iter().cloned());
            } else {
                let head = head_keep.min(item_lines.len());
                let tail = tail_keep.min(item_lines.len().saturating_sub(head));
                let omitted = item_lines.len().saturating_sub(head + tail);
                out.extend(item_lines[..head].iter().cloned());
                out.extend(markdown::literal(
                    &format!("… ({omitted} lines omitted in Live Feed · select tool with ←/→ for full output)"),
                    Face::MUTED,
                    w,
                ));
                if tail > 0 {
                    out.extend(item_lines[item_lines.len() - tail..].iter().cloned());
                }
            }
        }
        (out, any_pending)
    }

    #[cfg(test)]
    pub(super) fn set_max_payload_bytes_for_test(&mut self, max_bytes: usize) {
        self.max_payload_bytes = max_bytes;
    }

    #[cfg(test)]
    pub(super) fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    #[cfg(test)]
    pub(super) fn detail_cache_stats(&self) -> (u64, u64) {
        (self.detail_cache_hits.get(), self.detail_cache_misses.get())
    }

    #[cfg(test)]
    pub(super) fn item_wrap_count(&self) -> u64 {
        self.item_wrap_count.get()
    }
}

fn push_subagent_log(item: &mut ToolItem, line: String) -> usize {
    if item.subagent_logs.len() >= MAX_SUBAGENT_LOGS
        || item.subagent_log_bytes.saturating_add(line.len()) > MAX_SUBAGENT_LOG_BYTES
    {
        return 0;
    }
    let len = line.len();
    item.subagent_log_bytes += len;
    item.subagent_logs.push(line);
    len
}

/// Target content rows (excluding bottom `───┴───` border) for the live active inline block.
/// Uses remaining empty viewport space below prior transcript lines so initial tool runs
/// start spaciously, while keeping a comfortable ~65% floor when history already fills the screen.
pub(super) fn active_min_block_rows(
    viewport: usize,
    items_above: usize,
    stream_rows: usize,
) -> usize {
    if viewport == 0 {
        return 0;
    }
    let max_content = viewport.saturating_sub(1).max(1);
    let floor = ((viewport * 65) / 100).max(12).min(max_content);
    let unused_below = viewport.saturating_sub(items_above + stream_rows + 1);
    floor.max(unused_below).min(max_content)
}

/// Target content rows for a sealed inline block when expanded (`after_turn == Keep` or clicked).
pub(super) fn sealed_expanded_block_rows(viewport: usize) -> usize {
    if viewport == 0 {
        return 0;
    }
    let max_content = viewport.saturating_sub(2).max(3).min(viewport);
    ((viewport * 55) / 100).max(12).min(max_content)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_block_left(
    block: &ToolBlock,
    left_width: usize,
    inline_split: Option<HorizontalSplit>,
    split_active: bool,
    pending_approvals: &std::collections::VecDeque<PendingToolApproval>,
    after_turn: AfterTurnPolicy,
    viewport: usize,
    active_min_rows: Option<usize>,
    spinner: &str,
    highlights: &HighlightCache,
) -> RenderedBlockLeft {
    let w = left_width.max(1);
    if block.is_collapsed() {
        let label = clip(&format!("  {}", block.collapsed_label()), w);
        let line = vec![Span {
            text: label,
            face: Face::MUTED,
        }];
        return RenderedBlockLeft {
            lines: vec![line],
            row_kinds: vec![BlockRowKind::CollapsedHeader],
            has_bottom_border: false,
        };
    }

    let mut lines = Vec::new();
    let mut row_kinds = Vec::new();

    // Header row
    let header_text = clip(&format!("  {}", block.step_range_label()), w);
    lines.push(vec![Span {
        text: header_text,
        face: Face::MUTED,
    }]);
    row_kinds.push(BlockRowKind::Header);

    // Item rows (plus inline single-column approval preview when !split_active)
    let selected_idx = block.selected.min(block.items.len().saturating_sub(1));
    for (idx, item) in block.items.iter().enumerate() {
        lines.push(render_tool_item_row(item, idx == selected_idx, w, spinner));
        row_kinds.push(BlockRowKind::ToolItem(idx));

        if !split_active && item.status == ToolStatus::Pending {
            let preview_lines = render_single_col_approval_preview(item, w, highlights);
            for pl in preview_lines {
                lines.push(pl);
                row_kinds.push(BlockRowKind::Preview);
            }
        }
    }

    // Batch approval action footer (shown while active and pending approvals exist)
    if block.active {
        if let Some(front) = pending_approvals.front() {
            let count = front.call_count.max(1);
            let tool_noun = if count == 1 { "tool" } else { "tools" };
            let queued = if pending_approvals.len() > 1 {
                format!(" (+{} queued)", pending_approvals.len() - 1)
            } else {
                String::new()
            };
            let parts = [
                ("  ", Face::MUTED),
                ("[Y]", Face::HIGHLIGHT_BOLD),
                (&format!(" approve {count} {tool_noun} · "), Face::MUTED),
                ("[N]", Face::DANGER_BOLD),
                (&format!(" reject{queued}"), Face::MUTED),
            ];
            let line = clip_line_spans(&parts, w);
            lines.push(line);
            row_kinds.push(BlockRowKind::ApprovalFooter);
        }
    }

    // Inline split minimum height padding + bottom border
    let mut has_bottom_border = false;
    if let Some(split) = inline_split {
        let want_min = if block.active {
            active_min_rows.unwrap_or_else(|| active_min_block_rows(viewport, 0, 0))
        } else if after_turn == AfterTurnPolicy::Keep || block.expanded_override == Some(true) {
            sealed_expanded_block_rows(viewport)
        } else {
            0
        };
        while lines.len() < want_min {
            lines.push(Vec::new());
            row_kinds.push(BlockRowKind::Padding);
        }
        let border = format!(
            "{}┴{}",
            "─".repeat(usize::from(split.left.len)),
            "─".repeat(usize::from(split.right.len))
        );
        lines.push(vec![Span {
            text: border,
            face: Face::SUBTLE,
        }]);
        row_kinds.push(BlockRowKind::BottomBorder);
        has_bottom_border = true;
    }

    RenderedBlockLeft {
        lines,
        row_kinds,
        has_bottom_border,
    }
}

struct ParsedReadFileOutput<'a> {
    start: u64,
    end: u64,
    total: u64,
    header_note: &'a str,
    body: &'a str,
}

fn parse_read_file_header(output: &str) -> Option<ParsedReadFileOutput<'_>> {
    let (first_line, body) = match output.split_once('\n') {
        Some((h, rest)) => (h, rest),
        None => (output, ""),
    };
    let rest = first_line.strip_prefix("File: ")?;
    let (_, after_lparen) = rest.rsplit_once(" (lines ")?;
    let (range_part, after_rparen) = after_lparen.split_once(')')?;
    let (bounds, total_str) = range_part.split_once(" of ")?;
    let (start_str, end_str) = bounds.split_once("..=")?;
    let start = start_str.trim().parse::<u64>().ok()?;
    let end = end_str.trim().parse::<u64>().ok()?;
    let total = total_str.trim().parse::<u64>().ok()?;
    Some(ParsedReadFileOutput {
        start,
        end,
        total,
        header_note: after_rparen.trim(),
        body,
    })
}

/// For long file paths that overflow `max_width`, preserve the trailing filename (`…/render.rs`)
/// so the file identity and line-range suffix remain legible in narrow left panes.
fn clip_preserving_tail(text: &str, max_width: usize) -> String {
    let cleaned = markdown::clean_cow(text);
    if cleaned.width() <= max_width {
        return cleaned.into_owned();
    }
    if !(cleaned.contains('/') || cleaned.contains('\\')) || max_width < 4 {
        return clip(&cleaned, max_width);
    }
    let keep_budget = max_width.saturating_sub(1); // reserve 1 col for `…`
    let mut rev_chars = Vec::new();
    let mut used = 0usize;
    for ch in cleaned.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > keep_budget {
            break;
        }
        used += cw;
        rev_chars.push(ch);
    }
    rev_chars.reverse();
    let mut out = String::with_capacity(rev_chars.len() + 3);
    out.push('…');
    out.extend(rev_chars);
    out
}

pub(super) fn render_tool_item_row(
    item: &ToolItem,
    selected: bool,
    width: usize,
    spinner: &str,
) -> markdown::Line {
    let marker = if selected { "› " } else { "  " };
    let marker_face = if selected {
        Face::ACCENT_BOLD
    } else {
        Face::MUTED
    };
    let (icon, icon_face) = item.status.icon(spinner);
    let tool_face = if selected {
        Face::ACCENT_BOLD
    } else {
        Face::BOLD
    };
    let mut parts: Vec<(String, Face)> = vec![
        (marker.into(), marker_face),
        (format!("{icon} "), icon_face),
        (item.tool.clone(), tool_face),
    ];
    let prefix_w: usize = parts.iter().map(|(t, _)| t.width()).sum();
    let target = item.target_summary();
    let suffix = item.compact_option_suffix();
    let src_suffix = item
        .source
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    if !target.is_empty() || !suffix.is_empty() {
        parts.push(("  ".into(), Face::MUTED));
        let fixed_w = prefix_w + 2 + suffix.width() + src_suffix.width();
        let target_budget = width.saturating_sub(fixed_w);
        let fitted_target = if target_budget > 0 && target.width() > target_budget {
            clip_preserving_tail(&target, target_budget)
        } else {
            target
        };
        if !fitted_target.is_empty() {
            parts.push((
                fitted_target,
                if selected { Face::TEXT } else { Face::MUTED },
            ));
        }
        if !suffix.is_empty() {
            let opt_face = if item.tool == "read_file" {
                if selected {
                    Face::ACCENT
                } else {
                    Face::MUTED
                }
            } else {
                Face::MUTED
            };
            parts.push((suffix, opt_face));
        }
    }
    if !src_suffix.is_empty() {
        parts.push((src_suffix, Face::MUTED));
    }
    let refs: Vec<(&str, Face)> = parts.iter().map(|(t, f)| (t.as_str(), *f)).collect();
    clip_line_spans(&refs, width)
}

fn render_single_col_approval_preview(
    item: &ToolItem,
    width: usize,
    highlights: &HighlightCache,
) -> Vec<markdown::Line> {
    let call_val = json!({ "tool": item.tool, "args": item.args });
    let preview = crate::cli::describe_tool_approval_call(&call_val);
    let path_lang = item.args["path"]
        .as_str()
        .and_then(|p| SupportedLanguage::from_path(Path::new(p)));
    let mut out = Vec::new();
    if !preview.detail.is_empty() && (preview.detail_muted || preview.detail.contains('(')) {
        let face = if preview.detail_muted {
            Face::MUTED
        } else {
            Face::TEXT
        };
        let detail = preview.detail.trim_start_matches(": ").trim_start();
        out.push(clip_line_spans(
            &[("      ", Face::MUTED), (detail, face)],
            width,
        ));
    }
    for block in preview.blocks {
        let (face, diff_bg, block_lang) = match block.kind {
            crate::cli::ToolPreviewKind::Removed => (
                Face::DANGER.with_diff_bg(DiffBg::Removed),
                DiffBg::Removed,
                path_lang,
            ),
            crate::cli::ToolPreviewKind::Added => (
                Face::HIGHLIGHT.with_diff_bg(DiffBg::Added),
                DiffBg::Added,
                path_lang,
            ),
            crate::cli::ToolPreviewKind::Code => (Face::CODE, DiffBg::None, path_lang),
            crate::cli::ToolPreviewKind::Command => {
                (Face::CODE, DiffBg::None, Some(SupportedLanguage::Bash))
            }
            crate::cli::ToolPreviewKind::Muted => (Face::MUTED, DiffBg::None, None),
        };
        let cleaned = markdown::clean_cow(&block.text);
        let hl_doc = block_lang.and_then(|lang| {
            match highlights.lookup_or_schedule(HighlightLang::Code(lang), &cleaned) {
                Lookup::Ready(doc) => Some(doc),
                Lookup::Pending | Lookup::Unsupported => None,
            }
        });
        let token_lines = hl_doc.as_deref().and_then(HighlightedDoc::code_lines);
        let all_lines: Vec<&str> = cleaned.split('\n').collect();
        let cap = block.max_lines.min(6);
        if all_lines.is_empty() || (all_lines.len() == 1 && all_lines[0].is_empty()) {
            out.push(clip_line_spans(
                &[(block.prefix, face), ("(empty)", Face::MUTED)],
                width,
            ));
        } else {
            for (idx, line) in all_lines.iter().take(cap).enumerate() {
                if let Some(tokens) = token_lines.and_then(|tl| tl.get(idx)) {
                    let prefix_face = match block.kind {
                        crate::cli::ToolPreviewKind::Removed => {
                            Face::DANGER_BOLD.with_diff_bg(DiffBg::Removed)
                        }
                        crate::cli::ToolPreviewKind::Added => {
                            Face::HIGHLIGHT_BOLD.with_diff_bg(DiffBg::Added)
                        }
                        _ => face,
                    };
                    let mut parts = vec![(block.prefix, prefix_face)];
                    parts.extend(highlight::line_spans(line, tokens, diff_bg));
                    out.push(clip_line_spans(&parts, width));
                } else {
                    out.push(clip_line_spans(
                        &[(block.prefix, face), (*line, face)],
                        width,
                    ));
                }
            }
            if all_lines.len() > cap {
                let rem = format!("… ({} more line(s))", all_lines.len() - cap);
                out.push(clip_line_spans(
                    &[(block.prefix, face), (&rem, Face::MUTED)],
                    width,
                ));
            }
        }
    }
    out.push(clip_line_spans(
        &[("      Ctrl+O: full view", Face::MUTED)],
        width,
    ));
    out
}

pub(super) fn inspector_header_line(
    block: &ToolBlock,
    width: usize,
    spinner: &str,
) -> markdown::Line {
    let total = block.items.len();
    if block.follow_tool && total > 1 {
        let running = block
            .items
            .iter()
            .filter(|i| i.status == ToolStatus::Running)
            .count();
        let pending = block
            .items
            .iter()
            .filter(|i| i.status == ToolStatus::Pending)
            .count();
        let title = if block.active { "Live Feed" } else { "Feed" };
        let (status_text, status_face) = if running > 0 {
            (format!("{spinner} {running} running"), Face::ACCENT_BOLD)
        } else if pending > 0 {
            (format!("? {pending} pending"), Face::HIGHLIGHT_BOLD)
        } else {
            (format_elapsed_ms(block.total_elapsed_ms()), Face::MUTED)
        };
        let parts: Vec<(String, Face)> = vec![
            (" Inspector · ".into(), Face::MUTED),
            (title.into(), Face::ACCENT_BOLD),
            (format!(" · {total} tools · "), Face::MUTED),
            (status_text, status_face),
        ];
        let refs: Vec<(&str, Face)> = parts.iter().map(|(t, f)| (t.as_str(), *f)).collect();
        return clip_line_spans(&refs, width);
    }
    let Some(item) = block.selected_item() else {
        return clip_line_spans(&[(" Inspector", Face::MUTED)], width);
    };
    let idx = block.selected.min(total.saturating_sub(1)) + 1;
    let (_, icon_face) = item.status.icon(spinner);
    let status_text = match item.elapsed_display() {
        Some(el) => format!("{} {el}", item.status.label()),
        None => item.status.label().to_owned(),
    };
    let mut parts: Vec<(String, Face)> = vec![
        (format!(" Inspector · [{idx}/{total}] "), Face::MUTED),
        (item.tool.clone(), Face::ACCENT_BOLD),
    ];
    let target = item.target_summary();
    let range_label = item.read_file_range_label();
    let opt_suffix = if range_label.is_some() {
        String::new()
    } else {
        item.compact_option_suffix()
    };
    if !target.is_empty() {
        let target_budget = width / 3;
        if target_budget >= 8 {
            parts.push((" · ".into(), Face::MUTED));
            parts.push((clip_preserving_tail(&target, target_budget), Face::TEXT));
        }
    }
    if let Some(range) = range_label {
        parts.push((" · ".into(), Face::MUTED));
        parts.push((range, Face::ACCENT));
    } else if !opt_suffix.is_empty() {
        parts.push((opt_suffix, Face::MUTED));
    }
    parts.push((" · ".into(), Face::MUTED));
    parts.push((status_text, icon_face));
    let refs: Vec<(&str, Face)> = parts.iter().map(|(t, f)| (t.as_str(), *f)).collect();
    clip_line_spans(&refs, width)
}

pub(super) fn inspector_footer_line(
    block: &ToolBlock,
    has_active_block: bool,
    total_lines: usize,
    body_rows: usize,
    width: usize,
) -> (markdown::Line, Option<(usize, usize, InspectorBadgeAction)>) {
    let max_scroll = total_lines.saturating_sub(body_rows);
    let eff_scroll = block.scroll_from_bottom.min(max_scroll);
    let pct = if max_scroll == 0 {
        100
    } else {
        let bottom_shown = total_lines.saturating_sub(eff_scroll);
        ((bottom_shown * 100) / total_lines.max(1)).clamp(1, 100)
    };
    let (badge, badge_face, action) = if block.active {
        if !block.follow_tool {
            let label = if eff_scroll > 0 {
                format!("[▶ Follow {pct}%]")
            } else {
                "[▶ Follow]".to_owned()
            };
            (
                label,
                Face::ACCENT_BOLD,
                Some(InspectorBadgeAction::ToggleFollow),
            )
        } else if eff_scroll > 0 {
            (
                format!("[↓ Live {pct}%]"),
                Face::ACCENT_BOLD,
                Some(InspectorBadgeAction::JumpToBottom),
            )
        } else {
            (
                "[● Follow]".to_owned(),
                Face::HIGHLIGHT,
                Some(InspectorBadgeAction::ToggleFollow),
            )
        }
    } else if has_active_block {
        (
            "[▶ Follow]".to_owned(),
            Face::ACCENT_BOLD,
            Some(InspectorBadgeAction::ToggleFollow),
        )
    } else if eff_scroll > 0 {
        (
            format!("[↓ Bottom {pct}%]"),
            Face::ACCENT_BOLD,
            Some(InspectorBadgeAction::JumpToBottom),
        )
    } else {
        (format!("[{pct}%]"), Face::MUTED, None)
    };
    let left_hint = if (block.active && !block.follow_tool) || (!block.active && has_active_block) {
        " ←/→ tool · Alt+L follow · Ctrl+O"
    } else {
        " ←/→ tool · Alt+↑↓ · Ctrl+O"
    };
    let badge_clean = clip(&badge, width);
    let badge_w = badge_clean.width();
    let left_budget = width.saturating_sub(badge_w + 1);
    let left_text = if left_budget >= 8 {
        clip(left_hint, left_budget)
    } else {
        String::new()
    };
    let left_w = left_text.width();
    let pad = width.saturating_sub(left_w + badge_w);
    let mut line = Vec::new();
    if !left_text.is_empty() {
        line.push(Span {
            text: left_text,
            face: Face::MUTED,
        });
    }
    if pad > 0 {
        line.push(Span {
            text: " ".repeat(pad),
            face: Face::MUTED,
        });
    }
    let badge_start = left_w + pad;
    if !badge_clean.is_empty() {
        line.push(Span {
            text: badge_clean,
            face: badge_face,
        });
    }
    let hit = action.map(|act| (badge_start, badge_start + badge_w, act));
    (line, hit)
}

pub(super) fn clip_line_spans(parts: &[(&str, Face)], max_width: usize) -> markdown::Line {
    use unicode_width::UnicodeWidthChar;
    let mut out: markdown::Line = Vec::new();
    let mut used = 0usize;
    for (raw, face) in parts {
        let cleaned = markdown::clean_cow(raw);
        let mut kept = String::new();
        for ch in cleaned.chars() {
            let ch = if matches!(ch, '\n' | '\r' | '\t') {
                ' '
            } else {
                ch
            };
            if ch.is_control() {
                continue;
            }
            let cw = ch.width().unwrap_or(0);
            if used + cw > max_width {
                if !kept.is_empty() {
                    push_span(&mut out, &kept, *face);
                }
                return out;
            }
            used += cw;
            kept.push(ch);
        }
        if !kept.is_empty() {
            push_span(&mut out, &kept, *face);
        }
        if used >= max_width {
            break;
        }
    }
    out
}

pub(super) fn clip_rendered_line(line: &markdown::Line, max_width: usize) -> markdown::Line {
    let refs: Vec<(&str, Face)> = line.iter().map(|s| (s.text.as_str(), s.face)).collect();
    clip_line_spans(&refs, max_width)
}

fn push_span(out: &mut markdown::Line, text: &str, face: Face) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut().filter(|s| s.face == face) {
        last.text.push_str(text);
    } else {
        out.push(Span {
            text: text.to_owned(),
            face,
        });
    }
}

fn lookup_code_doc(
    lang: Option<SupportedLanguage>,
    cleaned: &str,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) -> Option<Arc<HighlightedDoc>> {
    let lang = lang?;
    match highlights.lookup_or_schedule(HighlightLang::Code(lang), cleaned) {
        Lookup::Ready(doc) => Some(doc),
        Lookup::Pending => {
            *had_pending = true;
            None
        }
        Lookup::Unsupported => None,
    }
}

fn push_highlighted_plain_code(
    out: &mut Vec<markdown::Line>,
    text: &str,
    fallback_face: Face,
    width: usize,
    lang: Option<SupportedLanguage>,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(text);
    let doc = lookup_code_doc(lang, &cleaned, highlights, had_pending);
    let token_lines = doc.as_deref().and_then(HighlightedDoc::code_lines);
    for (i, raw_line) in cleaned.split('\n').enumerate() {
        if let Some(tokens) = token_lines.and_then(|tl| tl.get(i)) {
            let parts = highlight::line_spans(raw_line, tokens, DiffBg::None);
            out.extend(markdown::spans(&parts, width));
        } else {
            out.extend(markdown::literal(raw_line, fallback_face, width));
        }
    }
}

fn push_numbered_code(
    out: &mut Vec<markdown::Line>,
    text: &str,
    start_line: usize,
    width: usize,
    lang: Option<SupportedLanguage>,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(text);
    let doc = lookup_code_doc(lang, &cleaned, highlights, had_pending);
    let token_lines = doc.as_deref().and_then(HighlightedDoc::code_lines);
    for (i, raw_line) in cleaned.split('\n').enumerate() {
        let line_no = start_line + i;
        let prefix = format!("{line_no:>4} │ ");
        if let Some(tokens) = token_lines.and_then(|tl| tl.get(i)) {
            let parts = highlight::line_spans(raw_line, tokens, DiffBg::None);
            out.extend(markdown::prefixed_spans(
                &prefix,
                Face::MUTED,
                &parts,
                width,
            ));
        } else {
            out.extend(markdown::prefixed(
                &prefix,
                Face::MUTED,
                raw_line,
                Face::CODE,
                width,
            ));
        }
    }
}

/// Renders `read_file` / `find_symbol` output: if lines already have `N | code` numbering
/// from `ReadFileTool`, style the existing gutter cleanly without double-numbering and style
/// trailing `... [N more line(s)...]` continuation hints in `Face::MUTED`.
#[allow(clippy::too_many_arguments)]
fn push_prenumbered_or_plain_code(
    out: &mut Vec<markdown::Line>,
    text: &str,
    start_line: usize,
    width: usize,
    lang: Option<SupportedLanguage>,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(text);
    let has_own_nums = cleaned.lines().next().is_some_and(|l| {
        l.split_once(" | ").is_some_and(|(num, _)| {
            !num.trim().is_empty() && num.trim().bytes().all(|b| b.is_ascii_digit())
        })
    });
    if !has_own_nums {
        push_numbered_code(
            out,
            &cleaned,
            start_line,
            width,
            lang,
            highlights,
            had_pending,
        );
        return;
    }

    enum ReadRow<'a> {
        Continuation(&'a str),
        Numbered { num: &'a str, code: &'a str },
        Raw(&'a str),
    }

    let mut rows = Vec::new();
    let mut pure_code_lines = Vec::new();
    for raw_line in cleaned.split('\n') {
        if raw_line.starts_with("... [") {
            rows.push(ReadRow::Continuation(raw_line));
        } else if let Some((num, code)) = raw_line.split_once(" | ") {
            let trimmed = num.trim();
            if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
                pure_code_lines.push(code);
                rows.push(ReadRow::Numbered { num: trimmed, code });
            } else {
                pure_code_lines.push(raw_line);
                rows.push(ReadRow::Raw(raw_line));
            }
        } else {
            pure_code_lines.push(raw_line);
            rows.push(ReadRow::Raw(raw_line));
        }
    }

    let doc = if lang.is_some() && !pure_code_lines.is_empty() {
        let joined = pure_code_lines.join("\n");
        lookup_code_doc(lang, &joined, highlights, had_pending)
    } else {
        None
    };
    let token_lines = doc.as_deref().and_then(HighlightedDoc::code_lines);
    let mut code_idx = 0usize;
    for row in rows {
        match row {
            ReadRow::Continuation(raw_line) => {
                out.extend(markdown::literal(raw_line, Face::MUTED, width));
            }
            ReadRow::Numbered { num, code } => {
                let prefix = format!("{num:>4} │ ");
                if let Some(tokens) = token_lines.and_then(|tl| tl.get(code_idx)) {
                    let parts = highlight::line_spans(code, tokens, DiffBg::None);
                    out.extend(markdown::prefixed_spans(
                        &prefix,
                        Face::MUTED,
                        &parts,
                        width,
                    ));
                } else {
                    out.extend(markdown::prefixed(
                        &prefix,
                        Face::MUTED,
                        code,
                        Face::CODE,
                        width,
                    ));
                }
                code_idx += 1;
            }
            ReadRow::Raw(raw_line) => {
                if let Some(tokens) = token_lines.and_then(|tl| tl.get(code_idx)) {
                    let parts = highlight::line_spans(raw_line, tokens, DiffBg::None);
                    out.extend(markdown::spans(&parts, width));
                } else {
                    out.extend(markdown::literal(raw_line, Face::CODE, width));
                }
                code_idx += 1;
            }
        }
    }
}

fn render_item_detail_lines(
    item: &ToolItem,
    width: usize,
    highlights: &HighlightCache,
) -> (Vec<markdown::Line>, bool) {
    let w = width.max(1);
    let mut out = Vec::new();
    let mut had_pending = false;
    if item.payload_pruned {
        let target = item.target_summary();
        if !target.is_empty() {
            out.extend(markdown::prefixed(
                "Target: ",
                Face::MUTED,
                &target,
                Face::TEXT,
                w,
            ));
        }
        let status_summary = match item.elapsed_display() {
            Some(el) => format!("{} ({el})", item.status.label()),
            None => item.status.label().to_owned(),
        };
        out.extend(markdown::prefixed(
            "Status: ",
            Face::MUTED,
            &status_summary,
            Face::TEXT,
            w,
        ));
        out.push(Vec::new());
        out.extend(markdown::literal(
            "Details pruned after retention",
            Face::MUTED,
            w,
        ));
        return (out, false);
    }

    let args = &item.args;
    match item.tool.as_str() {
        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let lang = SupportedLanguage::from_path(Path::new(path));
            let replace_all = args["replace_all"].as_bool().unwrap_or(false);
            let meta = if replace_all {
                format!("{path} (replace_all)")
            } else {
                path.to_owned()
            };
            if !meta.is_empty() {
                out.extend(markdown::literal(&meta, Face::ACCENT_BOLD, w));
            }
            if let Some(old_text) = args["old_text"].as_str() {
                let cleaned = markdown::clean_cow(old_text);
                let doc = lookup_code_doc(lang, &cleaned, highlights, &mut had_pending);
                let token_lines = doc.as_deref().and_then(HighlightedDoc::code_lines);
                let gutter_face = Face::DANGER_BOLD.with_diff_bg(DiffBg::Removed);
                let plain_face = Face::DANGER.with_diff_bg(DiffBg::Removed);
                for (i, line) in cleaned.split('\n').enumerate() {
                    if let Some(tokens) = token_lines.and_then(|tl| tl.get(i)) {
                        let parts = highlight::line_spans(line, tokens, DiffBg::Removed);
                        out.extend(markdown::prefixed_spans("- ", gutter_face, &parts, w));
                    } else {
                        out.extend(markdown::prefixed("- ", gutter_face, line, plain_face, w));
                    }
                }
            }
            if let Some(new_text) = args["new_text"].as_str() {
                let cleaned = markdown::clean_cow(new_text);
                let doc = lookup_code_doc(lang, &cleaned, highlights, &mut had_pending);
                let token_lines = doc.as_deref().and_then(HighlightedDoc::code_lines);
                let gutter_face = Face::HIGHLIGHT_BOLD.with_diff_bg(DiffBg::Added);
                let plain_face = Face::HIGHLIGHT.with_diff_bg(DiffBg::Added);
                for (i, line) in cleaned.split('\n').enumerate() {
                    if let Some(tokens) = token_lines.and_then(|tl| tl.get(i)) {
                        let parts = highlight::line_spans(line, tokens, DiffBg::Added);
                        out.extend(markdown::prefixed_spans("+ ", gutter_face, &parts, w));
                    } else {
                        out.extend(markdown::prefixed("+ ", gutter_face, line, plain_face, w));
                    }
                }
            }
            append_output_section(&mut out, item, w, "Result");
        }
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let lang = SupportedLanguage::from_path(Path::new(path));
            let content = args["content"].as_str().unwrap_or("");
            let line_count = content.lines().count();
            out.extend(markdown::literal(
                &format!("{path} · {line_count} lines · {} bytes", content.len()),
                Face::ACCENT_BOLD,
                w,
            ));
            if !content.is_empty() {
                push_numbered_code(&mut out, content, 1, w, lang, highlights, &mut had_pending);
            }
            append_output_section(&mut out, item, w, "Result");
        }
        "read_file" => {
            let mut meta = item.target_summary();
            let lang = SupportedLanguage::from_path(Path::new(&meta));
            let parsed_read = item.output.as_deref().and_then(parse_read_file_header);
            if let Some(range) = item.read_file_range_label() {
                if !meta.is_empty() {
                    meta.push_str(" · ");
                }
                meta.push_str(&range);
            }
            if !meta.is_empty() {
                out.extend(markdown::literal(&meta, Face::ACCENT_BOLD, w));
            }
            if let Some(parsed) = parsed_read {
                if !parsed.header_note.is_empty() {
                    out.extend(markdown::literal(parsed.header_note, Face::MUTED, w));
                }
                if !parsed.body.is_empty() {
                    push_prenumbered_or_plain_code(
                        &mut out,
                        parsed.body,
                        parsed.start as usize,
                        w,
                        lang,
                        highlights,
                        &mut had_pending,
                    );
                }
            } else if let Some(output) = &item.output {
                if item.ok == Some(false) {
                    out.extend(markdown::literal(output, Face::DANGER, w));
                } else {
                    let start = args["start_line"].as_u64().unwrap_or(1) as usize;
                    push_prenumbered_or_plain_code(
                        &mut out,
                        output,
                        start,
                        w,
                        lang,
                        highlights,
                        &mut had_pending,
                    );
                }
            } else {
                out.extend(markdown::literal(
                    match item.status {
                        ToolStatus::Pending => "(awaiting approval)",
                        ToolStatus::Running => "(reading…)",
                        _ => "(no output)",
                    },
                    Face::MUTED,
                    w,
                ));
            }
        }
        "find_symbol" => {
            let mut meta = vec![item.target_summary()];
            if let Some(path) = args["path"].as_str().filter(|s| !s.is_empty()) {
                meta.push(format!("path={path}"));
            }
            if args["include_body"].as_bool() == Some(true) {
                meta.push("include_body=true".into());
            }
            let meta_str = meta
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            if !meta_str.is_empty() {
                out.extend(markdown::literal(&meta_str, Face::ACCENT_BOLD, w));
            }
            if let Some(output) = &item.output {
                if item.ok == Some(false) {
                    out.extend(markdown::literal(output, Face::DANGER, w));
                } else {
                    render_ast_find_symbol(&mut out, output, w, highlights, &mut had_pending);
                }
            } else {
                out.extend(markdown::literal(
                    match item.status {
                        ToolStatus::Pending => "(awaiting approval)",
                        ToolStatus::Running => "(searching AST…)",
                        _ => "(no output)",
                    },
                    Face::MUTED,
                    w,
                ));
            }
        }
        "run_shell" => {
            let cmd = args["command"].as_str().unwrap_or("");
            let cleaned_cmd = markdown::clean_cow(cmd);
            let cmd_doc = lookup_code_doc(
                Some(SupportedLanguage::Bash),
                &cleaned_cmd,
                highlights,
                &mut had_pending,
            );
            let cmd_tokens = cmd_doc.as_deref().and_then(HighlightedDoc::code_lines);
            for (idx, line) in cleaned_cmd.split('\n').enumerate() {
                let pfx = if idx == 0 { "$ " } else { "  " };
                if let Some(tokens) = cmd_tokens.and_then(|tl| tl.get(idx)) {
                    let parts = highlight::line_spans(line, tokens, DiffBg::None);
                    out.extend(markdown::prefixed_spans(pfx, Face::ACCENT_BOLD, &parts, w));
                } else {
                    out.extend(markdown::prefixed(
                        pfx,
                        Face::ACCENT_BOLD,
                        line,
                        Face::CODE_BOLD,
                        w,
                    ));
                }
            }
            let mut shell_opts = Vec::new();
            if let Some(cwd) = args["cwd"].as_str().filter(|s| !s.is_empty()) {
                shell_opts.push(format!("cwd={cwd}"));
            }
            if let Some(timeout) = args["timeout_secs"].as_u64() {
                shell_opts.push(format!("timeout={timeout}s"));
            }
            if !shell_opts.is_empty() {
                out.extend(markdown::literal(&shell_opts.join(" · "), Face::MUTED, w));
            }
            if let Some(output) = &item.output {
                if !output.is_empty() {
                    let cleaned_out = markdown::clean_cow(output);
                    if item.ok != Some(false) && highlight::looks_like_unified_diff(&cleaned_out) {
                        let doc = match highlights
                            .lookup_or_schedule(HighlightLang::UnifiedDiff, &cleaned_out)
                        {
                            Lookup::Ready(doc) => Some(doc),
                            Lookup::Pending => {
                                had_pending = true;
                                None
                            }
                            Lookup::Unsupported => None,
                        };
                        highlight::push_unified_diff(&mut out, &cleaned_out, doc.as_deref(), w);
                    } else {
                        let mut in_stderr = false;
                        for raw_line in cleaned_out.split('\n') {
                            if raw_line.starts_with("STDERR:") || raw_line == "--- stderr ---" {
                                in_stderr = true;
                                out.extend(markdown::literal(raw_line, Face::DANGER_BOLD, w));
                                continue;
                            } else if raw_line.starts_with("STDOUT:")
                                || raw_line == "--- stdout ---"
                            {
                                in_stderr = false;
                                out.extend(markdown::literal(raw_line, Face::MUTED, w));
                                continue;
                            }
                            let face = if in_stderr
                                || item.ok == Some(false) && raw_line.starts_with("error")
                            {
                                Face::DANGER
                            } else {
                                Face::TEXT
                            };
                            out.extend(markdown::literal(raw_line, face, w));
                        }
                    }
                } else if !item.live_chunks.is_empty() {
                    append_live_chunks(&mut out, item, w);
                }
            } else if !item.live_chunks.is_empty() {
                append_live_chunks(&mut out, item, w);
            } else if item.status == ToolStatus::Running {
                out.extend(markdown::literal("(running…)", Face::MUTED, w));
            }
        }
        "start_process" | "process_output" | "stop_process" => {
            if let Some(cmd) = args["command"].as_str() {
                let cleaned_cmd = markdown::clean_cow(cmd);
                let cmd_doc = lookup_code_doc(
                    Some(SupportedLanguage::Bash),
                    &cleaned_cmd,
                    highlights,
                    &mut had_pending,
                );
                let cmd_tokens = cmd_doc.as_deref().and_then(HighlightedDoc::code_lines);
                for (idx, line) in cleaned_cmd.split('\n').enumerate() {
                    let pfx = if idx == 0 { "$ " } else { "  " };
                    if let Some(tokens) = cmd_tokens.and_then(|tl| tl.get(idx)) {
                        let parts = highlight::line_spans(line, tokens, DiffBg::None);
                        out.extend(markdown::prefixed_spans(pfx, Face::ACCENT_BOLD, &parts, w));
                    } else {
                        out.extend(markdown::prefixed(
                            pfx,
                            Face::ACCENT_BOLD,
                            line,
                            Face::CODE_BOLD,
                            w,
                        ));
                    }
                }
            } else if let Some(pid) = args["id"].as_u64() {
                let wait = args["wait_secs"]
                    .as_u64()
                    .map(|s| format!(" · wait={s}s"))
                    .unwrap_or_default();
                out.extend(markdown::literal(
                    &format!("process #{pid}{wait}"),
                    Face::ACCENT_BOLD,
                    w,
                ));
            }
            if let Some(cwd) = args["cwd"].as_str().filter(|s| !s.is_empty()) {
                out.extend(markdown::literal(&format!("cwd={cwd}"), Face::MUTED, w));
            }
            if let Some(output) = &item.output {
                let face = if item.ok == Some(false) {
                    Face::DANGER
                } else {
                    Face::CODE
                };
                out.extend(markdown::literal(output, face, w));
            }
        }
        "grep_search" | "find_files" => {
            let mut meta = Vec::new();
            if let Some(pat) = args["pattern"].as_str().filter(|s| !s.is_empty()) {
                meta.push(format!("pattern={pat:?}"));
            }
            if let Some(path) = args["path"]
                .as_str()
                .or_else(|| args["dir"].as_str())
                .filter(|s| !s.is_empty())
            {
                meta.push(format!("path={path}"));
            }
            if let Some(glob) = args["glob"].as_str().filter(|s| !s.is_empty()) {
                meta.push(format!("glob={glob}"));
            }
            if let Some(mode) = args["output_mode"].as_str().filter(|s| !s.is_empty()) {
                meta.push(format!("mode={mode}"));
            }
            if args["fixed_strings"].as_bool() == Some(true) {
                meta.push("fixed_strings=true".into());
            }
            if let Some(ctx) = args["context"].as_u64() {
                meta.push(format!("context={ctx}"));
            }
            if let Some(lim) = args["head_limit"]
                .as_u64()
                .or_else(|| args["limit"].as_u64())
            {
                meta.push(format!("limit={lim}"));
            }
            if let Some(output) = &item.output {
                let count = output.lines().filter(|l| !l.trim().is_empty()).count();
                meta.push(format!("{count} lines"));
            }
            if !meta.is_empty() {
                out.extend(markdown::literal(&meta.join(" · "), Face::MUTED, w));
            }
            if let Some(output) = &item.output {
                if item.ok == Some(false) {
                    out.extend(markdown::literal(output, Face::DANGER, w));
                } else if item.tool == "grep_search" {
                    render_grep_search(&mut out, args, output, w, highlights, &mut had_pending);
                } else {
                    out.extend(markdown::literal(output, Face::TEXT, w));
                }
            }
        }
        "code_outline" | "find_references" => {
            let mut meta = vec![item.target_summary()];
            if item.tool == "find_references" {
                if let Some(p) = args["path"].as_str().filter(|s| !s.is_empty()) {
                    meta.push(format!("path={p}"));
                }
                if args["include_tests"].as_bool() == Some(true) {
                    meta.push("include_tests=true".into());
                }
                if let Some(max) = args["max_results"].as_u64() {
                    meta.push(format!("max_results={max}"));
                }
            }
            let meta_str = meta
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            if !meta_str.is_empty() {
                out.extend(markdown::literal(&meta_str, Face::ACCENT_BOLD, w));
            }
            if let Some(output) = &item.output {
                if item.ok == Some(false) {
                    out.extend(markdown::literal(output, Face::DANGER, w));
                } else if item.tool == "code_outline" {
                    render_ast_code_outline(&mut out, output, w, highlights, &mut had_pending);
                } else {
                    render_ast_find_references(&mut out, output, w, highlights, &mut had_pending);
                }
            } else {
                out.extend(markdown::literal(
                    match item.status {
                        ToolStatus::Pending => "(awaiting approval)",
                        ToolStatus::Running => "(analyzing AST…)",
                        _ => "(no output)",
                    },
                    Face::MUTED,
                    w,
                ));
            }
        }
        "web_fetch" | "browser_fetch" | "browser_control" => {
            let mut meta = vec![item.target_summary()];
            if let Some(fmt) = args["format"].as_str().filter(|s| !s.is_empty()) {
                meta.push(format!("format={fmt}"));
            }
            if let Some(wait) = args["wait_ms"].as_u64() {
                meta.push(format!("wait={wait}ms"));
            }
            if let Some(max_c) = args["max_chars"].as_u64() {
                meta.push(format!("max_chars={max_c}"));
            }
            if let Some(acct) = args["account_id"].as_str().filter(|s| !s.is_empty()) {
                meta.push(format!("account={acct}"));
            }
            let meta_str = meta
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            if !meta_str.is_empty() {
                out.extend(markdown::literal(&meta_str, Face::ACCENT_BOLD, w));
            }
            if item.tool == "browser_control" && args.as_object().is_some_and(|m| m.len() > 1) {
                let pretty =
                    serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
                push_highlighted_plain_code(
                    &mut out,
                    &pretty,
                    Face::MUTED,
                    w,
                    Some(SupportedLanguage::Json),
                    highlights,
                    &mut had_pending,
                );
            }
            append_output_section(&mut out, item, w, "Response");
        }
        "spawn_agent" => {
            let label = args["label"].as_str().unwrap_or("worker");
            let ro = args["read_only"].as_bool().unwrap_or(true);
            out.extend(markdown::literal(
                &format!("worker [{label}] · read_only={ro}"),
                Face::ACCENT_BOLD,
                w,
            ));
            if let Some(task) = args["task"].as_str().filter(|s| !s.is_empty()) {
                out.extend(markdown::literal(task, Face::MUTED, w));
            }
            if !item.subagent_logs.is_empty() {
                out.push(Vec::new());
                out.extend(markdown::literal("Progress", Face::HEADING, w));
                for entry in &item.subagent_logs {
                    out.extend(markdown::prefixed(
                        "  · ",
                        Face::ACCENT,
                        entry,
                        Face::TEXT,
                        w,
                    ));
                }
            }
            append_output_section(&mut out, item, w, "Summary");
        }
        _ => {
            if !args.is_null() && args.as_object().is_none_or(|m| !m.is_empty()) {
                out.extend(markdown::literal("Arguments", Face::MUTED, w));
                let pretty =
                    serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
                push_highlighted_plain_code(
                    &mut out,
                    &pretty,
                    Face::CODE,
                    w,
                    Some(SupportedLanguage::Json),
                    highlights,
                    &mut had_pending,
                );
            }
            append_output_section(&mut out, item, w, "Output");
        }
    }

    if out.is_empty() {
        out.extend(markdown::literal("(no details)", Face::MUTED, w));
    }
    (out, had_pending)
}

fn split_grep_content_line(raw_line: &str, allow_context: bool) -> Option<(&str, bool, &str)> {
    let bytes = raw_line.as_bytes();
    let digit_end = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digit_end == 0 || digit_end >= bytes.len() {
        return None;
    }
    match bytes[digit_end] {
        b':' => Some((&raw_line[..digit_end], true, &raw_line[digit_end + 1..])),
        b'-' if allow_context => Some((&raw_line[..digit_end], false, &raw_line[digit_end + 1..])),
        _ => None,
    }
}

/// Splits `base` spans (which partition `0..line.len()`) to overlay `Face::SEARCH_MATCH`
/// onto every non-empty regex match without altering text or wrap geometry.
fn overlay_search_matches<'a>(
    line: &'a str,
    base: Vec<(&'a str, Face)>,
    re: &regex::Regex,
) -> Vec<(&'a str, Face)> {
    let matches: Vec<(usize, usize)> = re
        .find_iter(line)
        .filter(|m| m.start() < m.end())
        .map(|m| (m.start(), m.end()))
        .collect();
    if matches.is_empty() {
        return base;
    }
    let mut out = Vec::with_capacity(base.len() + matches.len() * 2);
    let mut m_idx = 0usize;
    let mut seg_start = 0usize;
    for (seg, face) in base {
        let seg_end = seg_start + seg.len();
        while m_idx < matches.len() && matches[m_idx].1 <= seg_start {
            m_idx += 1;
        }
        let mut pos = seg_start;
        while pos < seg_end {
            if m_idx >= matches.len() {
                out.push((&line[pos..seg_end], face));
                break;
            }
            let (ms, me) = matches[m_idx];
            if ms >= seg_end {
                out.push((&line[pos..seg_end], face));
                break;
            }
            if ms > pos {
                out.push((&line[pos..ms], face));
                pos = ms;
            }
            let ov_end = me.min(seg_end);
            if ov_end > pos {
                out.push((&line[pos..ov_end], Face::SEARCH_MATCH));
                pos = ov_end;
            }
            if me <= seg_end {
                m_idx += 1;
            }
        }
        seg_start = seg_end;
    }
    out
}

/// Structured rendering for `grep_search`:
/// - Highlights matched pattern substrings immediately with `Face::SEARCH_MATCH` (smart-case / regex / fixed_strings).
/// - Groups code lines per file header for async parallel Tree-sitter syntax highlighting.
fn render_grep_search(
    out: &mut Vec<markdown::Line>,
    args: &Value,
    output: &str,
    width: usize,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(output);
    let mode = args["output_mode"].as_str().unwrap_or("content");
    if mode == "files_with_matches" || mode == "files" {
        for raw_line in cleaned.split('\n') {
            if raw_line.is_empty() {
                out.push(Vec::new());
            } else if raw_line.starts_with('[')
                || raw_line.starts_with("Found ")
                || raw_line.starts_with("No matches ")
            {
                out.extend(markdown::literal(raw_line, Face::MUTED, width));
            } else {
                out.extend(markdown::literal(raw_line, Face::ACCENT, width));
            }
        }
        return;
    }
    if mode == "count" {
        for raw_line in cleaned.split('\n') {
            if raw_line.is_empty() {
                out.push(Vec::new());
            } else if raw_line.starts_with('[')
                || raw_line.starts_with("Found ")
                || raw_line.starts_with("No matches ")
            {
                out.extend(markdown::literal(raw_line, Face::MUTED, width));
            } else if let Some((path, count)) = raw_line.rsplit_once(':') {
                if !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
                    out.extend(markdown::spans(
                        &[
                            (path, Face::ACCENT_BOLD),
                            (":", Face::MUTED),
                            (count, Face::HIGHLIGHT_BOLD),
                        ],
                        width,
                    ));
                } else {
                    out.extend(markdown::literal(raw_line, Face::TEXT, width));
                }
            } else {
                out.extend(markdown::literal(raw_line, Face::TEXT, width));
            }
        }
        return;
    }

    let pattern = args["pattern"].as_str().unwrap_or("");
    let fixed = args["fixed_strings"].as_bool().unwrap_or(false);
    let allow_context = args["context"].as_u64().unwrap_or(0) > 0;
    let re = crate::agent::tools::compile_grep_regex(pattern, fixed);

    enum GrepRow<'a> {
        Empty,
        Notice(&'a str),
        GroupSep,
        FileHeader(&'a str),
        CodeLine {
            num: &'a str,
            is_match: bool,
            code: &'a str,
        },
    }

    let raw_lines: Vec<&str> = cleaned.split('\n').collect();
    let mut rows = Vec::with_capacity(raw_lines.len());
    let mut section_docs: Vec<Option<Arc<HighlightedDoc>>> = Vec::new();
    let mut line_lookup: Vec<Option<(usize, usize)>> = vec![None; raw_lines.len()];
    let mut cur_lang: Option<SupportedLanguage> = None;
    let mut cur_code: Vec<(usize, &str)> = Vec::new();
    let mut saw_file_header = false;

    let flush_code = |lang: Option<SupportedLanguage>,
                      code_lines: &mut Vec<(usize, &str)>,
                      docs: &mut Vec<Option<Arc<HighlightedDoc>>>,
                      lookup: &mut [Option<(usize, usize)>],
                      pending: &mut bool| {
        if code_lines.is_empty() {
            return;
        }
        let sec_id = docs.len();
        let joined = code_lines
            .iter()
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join("\n");
        let doc = lookup_code_doc(lang, &joined, highlights, pending);
        for (sub_idx, (line_idx, _)) in code_lines.drain(..).enumerate() {
            lookup[line_idx] = Some((sec_id, sub_idx));
        }
        docs.push(doc);
    };

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if raw_line.is_empty() {
            rows.push(GrepRow::Empty);
        } else if raw_line.starts_with('[')
            || raw_line.starts_with("Found ")
            || raw_line.starts_with("No matches ")
        {
            rows.push(GrepRow::Notice(raw_line));
        } else if allow_context && saw_file_header && raw_line == "--" {
            rows.push(GrepRow::GroupSep);
        } else if let Some((num, is_match, code)) =
            split_grep_content_line(raw_line, allow_context && saw_file_header)
        {
            cur_code.push((idx, code));
            rows.push(GrepRow::CodeLine {
                num,
                is_match,
                code,
            });
        } else {
            flush_code(
                cur_lang,
                &mut cur_code,
                &mut section_docs,
                &mut line_lookup,
                had_pending,
            );
            saw_file_header = true;
            cur_lang = SupportedLanguage::from_path(Path::new(raw_line.trim()));
            rows.push(GrepRow::FileHeader(raw_line));
        }
    }
    flush_code(
        cur_lang,
        &mut cur_code,
        &mut section_docs,
        &mut line_lookup,
        had_pending,
    );

    for (idx, row) in rows.into_iter().enumerate() {
        match row {
            GrepRow::Empty => out.push(Vec::new()),
            GrepRow::Notice(s) => out.extend(markdown::literal(s, Face::MUTED, width)),
            GrepRow::GroupSep => out.extend(markdown::literal("--", Face::SUBTLE, width)),
            GrepRow::FileHeader(s) => out.extend(markdown::literal(s, Face::ACCENT_BOLD, width)),
            GrepRow::CodeLine {
                num,
                is_match,
                code,
            } => {
                let (prefix, prefix_face, fallback_face) = if is_match {
                    (format!("{num:>4} │ "), Face::ACCENT, Face::TEXT)
                } else {
                    (format!("{num:>4} · "), Face::SUBTLE, Face::MUTED)
                };
                let tokens = line_lookup[idx].and_then(|(sec_id, sub_idx)| {
                    section_docs[sec_id]
                        .as_deref()
                        .and_then(HighlightedDoc::code_lines)
                        .and_then(|tl| tl.get(sub_idx))
                });
                let base_spans = match tokens {
                    Some(tokens) => highlight::line_spans(code, tokens, DiffBg::None),
                    None if code.is_empty() => Vec::new(),
                    None => vec![(code, fallback_face)],
                };
                let final_spans = match (is_match, re.as_ref()) {
                    (true, Some(regex)) => overlay_search_matches(code, base_spans, regex),
                    _ => base_spans,
                };
                out.extend(markdown::prefixed_spans(
                    &prefix,
                    prefix_face,
                    &final_spans,
                    width,
                ));
            }
        }
    }
}

fn is_line_span_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    match tok.split_once('-') {
        Some((a, b)) => {
            !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
        }
        None => tok.bytes().all(|c| c.is_ascii_digit()),
    }
}

fn parse_outline_header_lang(header: &str) -> Option<SupportedLanguage> {
    if let Some((path, rest)) = header.split_once(" (") {
        if let Some(lang_tok) = rest.split(',').next() {
            if let Some(lang) = SupportedLanguage::from_name(lang_tok) {
                return Some(lang);
            }
        }
        return SupportedLanguage::from_path(Path::new(path.trim()));
    }
    None
}

/// Structured rendering for `code_outline` (`<file> (lang, N lines)` headers,
/// `<indent><start-end>  <signature>  // <doc>` entries, and `[...]` folding notices).
fn render_ast_code_outline(
    out: &mut Vec<markdown::Line>,
    output: &str,
    width: usize,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(output);
    let raw_lines: Vec<&str> = cleaned.split('\n').collect();

    // Pre-collect per-file signature batches so each file's outline signatures highlight in 1 job.
    let mut section_docs: Vec<Option<Arc<HighlightedDoc>>> = Vec::new();
    let mut line_lookup: Vec<Option<(usize, usize)>> = vec![None; raw_lines.len()];
    let mut cur_lang: Option<SupportedLanguage> = None;
    let mut cur_sigs: Vec<(usize, &str)> = Vec::new();
    let flush_sigs = |lang: Option<SupportedLanguage>,
                      sigs: &mut Vec<(usize, &str)>,
                      docs: &mut Vec<Option<Arc<HighlightedDoc>>>,
                      lookup: &mut [Option<(usize, usize)>],
                      pending: &mut bool| {
        if sigs.is_empty() {
            return;
        }
        let sec_id = docs.len();
        let joined = sigs.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n");
        let doc = lookup_code_doc(lang, &joined, highlights, pending);
        for (sub_idx, (line_idx, _)) in sigs.drain(..).enumerate() {
            lookup[line_idx] = Some((sec_id, sub_idx));
        }
        docs.push(doc);
    };

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if raw_line.is_empty()
            || raw_line.starts_with('[')
            || raw_line.starts_with("No definitions")
            || raw_line.starts_with("No supported source")
        {
            continue;
        }
        let trimmed_start = raw_line.trim_start_matches(' ');
        let indent_len = raw_line.len() - trimmed_start.len();
        if let Some((span_tok, rest)) = trimmed_start.split_once("  ") {
            if is_line_span_token(span_tok) {
                let sig = rest.split_once("  // ").map_or(rest, |(s, _)| s);
                cur_sigs.push((idx, sig));
                continue;
            }
        }
        if indent_len == 0 && raw_line.contains(" lines") && raw_line.ends_with(')') {
            flush_sigs(
                cur_lang,
                &mut cur_sigs,
                &mut section_docs,
                &mut line_lookup,
                had_pending,
            );
            cur_lang = parse_outline_header_lang(raw_line);
        }
    }
    flush_sigs(
        cur_lang,
        &mut cur_sigs,
        &mut section_docs,
        &mut line_lookup,
        had_pending,
    );

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if raw_line.is_empty() {
            out.push(Vec::new());
            continue;
        }
        if raw_line.starts_with('[')
            || raw_line.starts_with("No definitions")
            || raw_line.starts_with("No supported source")
        {
            out.extend(markdown::literal(raw_line, Face::MUTED, width));
            continue;
        }
        let trimmed_start = raw_line.trim_start_matches(' ');
        let indent_len = raw_line.len() - trimmed_start.len();
        if let Some((span_tok, rest)) = trimmed_start.split_once("  ") {
            if is_line_span_token(span_tok) {
                let prefix = &raw_line[..indent_len + span_tok.len() + 2];
                let tokens = line_lookup[idx].and_then(|(sec_id, sub_idx)| {
                    section_docs[sec_id]
                        .as_deref()
                        .and_then(HighlightedDoc::code_lines)
                        .and_then(|tl| tl.get(sub_idx))
                });
                if let Some((sig, doc)) = rest.split_once("  // ") {
                    if let Some(tokens) = tokens {
                        let mut parts = vec![(prefix, Face::MUTED)];
                        parts.extend(highlight::line_spans(sig, tokens, DiffBg::None));
                        parts.push(("  // ", Face::MUTED));
                        parts.push((doc, Face::MUTED));
                        out.extend(markdown::spans(&parts, width));
                    } else {
                        out.extend(markdown::spans(
                            &[
                                (prefix, Face::MUTED),
                                (sig, Face::CODE),
                                ("  // ", Face::MUTED),
                                (doc, Face::MUTED),
                            ],
                            width,
                        ));
                    }
                } else if let Some(tokens) = tokens {
                    let parts = highlight::line_spans(rest, tokens, DiffBg::None);
                    out.extend(markdown::prefixed_spans(prefix, Face::MUTED, &parts, width));
                } else {
                    out.extend(markdown::prefixed(
                        prefix,
                        Face::MUTED,
                        rest,
                        Face::CODE,
                        width,
                    ));
                }
                continue;
            }
        }
        if indent_len == 0 && raw_line.contains(" lines") && raw_line.ends_with(')') {
            out.extend(markdown::literal(raw_line, Face::ACCENT_BOLD, width));
        } else {
            out.extend(markdown::literal(raw_line, Face::CODE, width));
        }
    }
}

/// Structured rendering for `find_symbol` (summary header, `path:span  Parent::child` headers,
/// single-gutter `N │ code` bodies with NO double line-numbering, and `[...]` notices).
fn render_ast_find_symbol(
    out: &mut Vec<markdown::Line>,
    output: &str,
    width: usize,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(output);
    let raw_lines: Vec<&str> = cleaned.split('\n').collect();

    // Group `N | code` body runs under each `<path>:<span-lines>` header for multi-line parsing.
    let mut section_docs: Vec<Option<Arc<HighlightedDoc>>> = Vec::new();
    let mut line_lookup: Vec<Option<(usize, usize)>> = vec![None; raw_lines.len()];
    let mut cur_lang: Option<SupportedLanguage> = None;
    let mut cur_body: Vec<(usize, &str)> = Vec::new();
    let flush_body = |lang: Option<SupportedLanguage>,
                      body: &mut Vec<(usize, &str)>,
                      docs: &mut Vec<Option<Arc<HighlightedDoc>>>,
                      lookup: &mut [Option<(usize, usize)>],
                      pending: &mut bool| {
        if body.is_empty() {
            return;
        }
        let sec_id = docs.len();
        let joined = body.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n");
        let doc = lookup_code_doc(lang, &joined, highlights, pending);
        for (sub_idx, (line_idx, _)) in body.drain(..).enumerate() {
            lookup[line_idx] = Some((sec_id, sub_idx));
        }
        docs.push(doc);
    };

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if let Some((num, code)) = raw_line.split_once(" | ") {
            let trimmed = num.trim();
            if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
                cur_body.push((idx, code));
                continue;
            }
        }
        let loc_part = raw_line.split_once("  ").map_or(raw_line, |(loc, _)| loc);
        if let Some((path_str, span)) = loc_part.rsplit_once(':') {
            if !path_str.is_empty() && is_line_span_token(span) {
                flush_body(
                    cur_lang,
                    &mut cur_body,
                    &mut section_docs,
                    &mut line_lookup,
                    had_pending,
                );
                cur_lang = SupportedLanguage::from_path(Path::new(path_str.trim()));
            }
        }
    }
    flush_body(
        cur_lang,
        &mut cur_body,
        &mut section_docs,
        &mut line_lookup,
        had_pending,
    );

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if raw_line.is_empty() {
            out.push(Vec::new());
            continue;
        }
        if raw_line.starts_with('[')
            || raw_line.starts_with("... [")
            || raw_line.starts_with("Found ")
            || raw_line.starts_with("No definitions")
        {
            out.extend(markdown::literal(raw_line, Face::MUTED, width));
            continue;
        }
        if let Some((num, code)) = raw_line.split_once(" | ") {
            let trimmed = num.trim();
            if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
                let prefix = format!("{trimmed:>4} │ ");
                let tokens = line_lookup[idx].and_then(|(sec_id, sub_idx)| {
                    section_docs[sec_id]
                        .as_deref()
                        .and_then(HighlightedDoc::code_lines)
                        .and_then(|tl| tl.get(sub_idx))
                });
                if let Some(tokens) = tokens {
                    let parts = highlight::line_spans(code, tokens, DiffBg::None);
                    out.extend(markdown::prefixed_spans(
                        &prefix,
                        Face::MUTED,
                        &parts,
                        width,
                    ));
                } else {
                    out.extend(markdown::prefixed(
                        &prefix,
                        Face::MUTED,
                        code,
                        Face::CODE,
                        width,
                    ));
                }
                continue;
            }
        }
        // Match header or signature row: `<path>:<span-lines>[  <qualified>][  <sig>][  // <doc>]`
        let (loc_part, rest_opt) = match raw_line.split_once("  ") {
            Some((loc, rest)) => (loc, Some(rest)),
            None => (raw_line, None),
        };
        let is_loc_header = loc_part
            .rsplit_once(':')
            .is_some_and(|(p, span)| !p.is_empty() && is_line_span_token(span));
        if is_loc_header {
            match rest_opt {
                Some(rest) => {
                    if let Some((code_part, doc)) = rest.split_once("  // ") {
                        out.extend(markdown::spans(
                            &[
                                (loc_part, Face::ACCENT_BOLD),
                                ("  ", Face::MUTED),
                                (code_part, Face::CODE),
                                ("  // ", Face::MUTED),
                                (doc, Face::MUTED),
                            ],
                            width,
                        ));
                    } else if let Some(doc) = rest.strip_prefix("// ") {
                        out.extend(markdown::spans(
                            &[
                                (loc_part, Face::ACCENT_BOLD),
                                ("  // ", Face::MUTED),
                                (doc, Face::MUTED),
                            ],
                            width,
                        ));
                    } else {
                        out.extend(markdown::spans(
                            &[
                                (loc_part, Face::ACCENT_BOLD),
                                ("  ", Face::MUTED),
                                (rest, Face::CODE),
                            ],
                            width,
                        ));
                    }
                }
                None => out.extend(markdown::literal(loc_part, Face::ACCENT_BOLD, width)),
            }
        } else {
            out.extend(markdown::literal(raw_line, Face::CODE, width));
        }
    }
}

/// Structured rendering for `find_references` (summary header, file group headers,
/// `  <line> [<enclosing_scope>]  <call_line>` entries, and `[...]` notices).
fn render_ast_find_references(
    out: &mut Vec<markdown::Line>,
    output: &str,
    width: usize,
    highlights: &HighlightCache,
    had_pending: &mut bool,
) {
    let cleaned = markdown::clean_cow(output);
    let raw_lines: Vec<&str> = cleaned.split('\n').collect();

    let mut section_docs: Vec<Option<Arc<HighlightedDoc>>> = Vec::new();
    let mut line_lookup: Vec<Option<(usize, usize)>> = vec![None; raw_lines.len()];
    let mut cur_lang: Option<SupportedLanguage> = None;
    let mut cur_calls: Vec<(usize, &str)> = Vec::new();
    let flush_calls = |lang: Option<SupportedLanguage>,
                       calls: &mut Vec<(usize, &str)>,
                       docs: &mut Vec<Option<Arc<HighlightedDoc>>>,
                       lookup: &mut [Option<(usize, usize)>],
                       pending: &mut bool| {
        if calls.is_empty() {
            return;
        }
        let sec_id = docs.len();
        let joined = calls.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n");
        let doc = lookup_code_doc(lang, &joined, highlights, pending);
        for (sub_idx, (line_idx, _)) in calls.drain(..).enumerate() {
            lookup[line_idx] = Some((sec_id, sub_idx));
        }
        docs.push(doc);
    };

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if raw_line.is_empty()
            || raw_line.starts_with('[')
            || raw_line.starts_with("Found ")
            || raw_line.starts_with("No references")
        {
            continue;
        }
        if let Some(after_indent) = raw_line.strip_prefix("  ") {
            if let Some((line_tok, after_num)) = after_indent.split_once(" [") {
                if !line_tok.is_empty() && line_tok.bytes().all(|b| b.is_ascii_digit()) {
                    if let Some((_, call_rest)) = after_num.split_once("]  ") {
                        cur_calls.push((idx, call_rest));
                    }
                }
            }
        } else {
            flush_calls(
                cur_lang,
                &mut cur_calls,
                &mut section_docs,
                &mut line_lookup,
                had_pending,
            );
            cur_lang = SupportedLanguage::from_path(Path::new(raw_line.trim()));
        }
    }
    flush_calls(
        cur_lang,
        &mut cur_calls,
        &mut section_docs,
        &mut line_lookup,
        had_pending,
    );

    for (idx, &raw_line) in raw_lines.iter().enumerate() {
        if raw_line.is_empty() {
            out.push(Vec::new());
            continue;
        }
        if raw_line.starts_with('[')
            || raw_line.starts_with("Found ")
            || raw_line.starts_with("No references")
        {
            out.extend(markdown::literal(raw_line, Face::MUTED, width));
            continue;
        }
        if let Some(after_indent) = raw_line.strip_prefix("  ") {
            if let Some((line_tok, after_num)) = after_indent.split_once(" [") {
                if !line_tok.is_empty() && line_tok.bytes().all(|b| b.is_ascii_digit()) {
                    if let Some((scope, call_rest)) = after_num.split_once("]  ") {
                        let line_prefix = format!("{line_tok:>4} │ ");
                        let scope_badge = format!("[{scope}] ");
                        let tokens = line_lookup[idx].and_then(|(sec_id, sub_idx)| {
                            section_docs[sec_id]
                                .as_deref()
                                .and_then(HighlightedDoc::code_lines)
                                .and_then(|tl| tl.get(sub_idx))
                        });
                        if let Some(tokens) = tokens {
                            let mut parts = vec![
                                (line_prefix.as_str(), Face::MUTED),
                                (scope_badge.as_str(), Face::ACCENT),
                            ];
                            parts.extend(highlight::line_spans(call_rest, tokens, DiffBg::None));
                            out.extend(markdown::spans(&parts, width));
                        } else {
                            out.extend(markdown::spans(
                                &[
                                    (line_prefix.as_str(), Face::MUTED),
                                    (scope_badge.as_str(), Face::ACCENT),
                                    (call_rest, Face::CODE),
                                ],
                                width,
                            ));
                        }
                        continue;
                    }
                }
            }
            out.extend(markdown::literal(raw_line, Face::CODE, width));
        } else {
            out.extend(markdown::literal(raw_line, Face::ACCENT_BOLD, width));
        }
    }
}

fn append_live_chunks(out: &mut Vec<markdown::Line>, item: &ToolItem, width: usize) {
    for (stream, text) in &item.live_chunks {
        let face = match stream {
            OutputStream::Stdout => Face::TEXT,
            OutputStream::Stderr => Face::DANGER,
        };
        out.extend(markdown::literal(text, face, width));
    }
    if item.live_capped {
        out.extend(markdown::literal(
            "… [live stream capped at 64 KiB]",
            Face::MUTED,
            width,
        ));
    }
}

fn append_output_section(
    out: &mut Vec<markdown::Line>,
    item: &ToolItem,
    width: usize,
    heading: &str,
) {
    let Some(output) = item.output.as_deref().filter(|s| !s.is_empty()) else {
        return;
    };
    if !out.is_empty() {
        out.push(Vec::new());
    }
    let heading_face = if item.ok == Some(false) {
        Face::DANGER_BOLD
    } else {
        Face::MUTED
    };
    out.extend(markdown::literal(heading, heading_face, width));
    let body_face = if item.ok == Some(false) {
        Face::DANGER
    } else {
        Face::TEXT
    };
    out.extend(markdown::literal(output, body_face, width));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ToolCallRequest, ToolOutput};

    #[test]
    fn approval_promotes_parallel_same_tool_calls_in_place_by_call_index() {
        let mut store = ToolBlockStore::default();
        let calls = vec![
            json!({"tool": "read_file", "args": {"path": "a.rs"}}),
            json!({"tool": "read_file", "args": {"path": "b.rs"}}),
        ];
        let block_id = store.on_approval_requested("apr_1".into(), 1, &calls, &[0, 2], None, true);
        assert_eq!(store.get(block_id).unwrap().items.len(), 2);
        assert_eq!(store.get(block_id).unwrap().items[0].index, 0);
        assert_eq!(store.get(block_id).unwrap().items[1].index, 2);

        // Main-agent approval keeps items Pending until ToolStarted promotes them.
        store.on_approval_resolved("apr_1", true);
        assert_eq!(
            store.get(block_id).unwrap().items[0].status,
            ToolStatus::Pending
        );
        assert_eq!(
            store.get(block_id).unwrap().items[1].status,
            ToolStatus::Pending
        );

        // Out-of-order parallel starts promote exact indices with zero duplicate rows.
        store.on_tool_started(1, 2, "read_file".into(), json!({"path": "b.rs"}), true);
        store.on_tool_started(1, 0, "read_file".into(), json!({"path": "a.rs"}), true);
        let block = store.get(block_id).unwrap();
        assert_eq!(block.items.len(), 2);
        assert_eq!(block.items[0].status, ToolStatus::Running);
        assert_eq!(block.items[1].status, ToolStatus::Running);

        store.on_tool_finished(1, 2, "read_file", true, "b body".into(), 12);
        store.on_tool_finished(1, 0, "read_file", true, "a body".into(), 8);
        let block = store.get(block_id).unwrap();
        assert_eq!(block.items[0].output.as_deref(), Some("a body"));
        assert_eq!(block.items[1].output.as_deref(), Some("b body"));
    }

    #[test]
    fn subagent_approval_transitions_immediately_to_done_or_rejected() {
        let mut store = ToolBlockStore::default();
        let calls = vec![json!({"tool": "run_shell", "args": {"command": "echo hi"}})];
        let block_id = store.on_approval_requested(
            "sub_apr_ok".into(),
            1,
            &calls,
            &[0],
            Some("worker: research".into()),
            true,
        );
        store.on_approval_resolved("sub_apr_ok", true);
        assert_eq!(
            store.get(block_id).unwrap().items[0].status,
            ToolStatus::Done(true)
        );

        store.on_approval_requested(
            "sub_apr_no".into(),
            2,
            &calls,
            &[0],
            Some("worker: research".into()),
            true,
        );
        store.on_approval_resolved("sub_apr_no", false);
        assert_eq!(
            store.get(block_id).unwrap().items[1].status,
            ToolStatus::Rejected
        );
    }

    #[test]
    fn seal_cancels_incomplete_and_spawn_agent_binds_capped_logs() {
        let mut store = ToolBlockStore::default();
        let block_id = store.on_tool_started(
            1,
            0,
            "spawn_agent".into(),
            json!({"label": "audit", "task": "check files", "read_only": true}),
            true,
        );
        store.on_subagent_started("sub_1", "audit");
        for i in 0..80 {
            store.on_subagent_progress("sub_1", &format!("step {i}"), "scanning");
        }
        store.on_subagent_finished("sub_1", true, 500);
        let item = &store.get(block_id).unwrap().items[0];
        assert_eq!(item.subagent_id.as_deref(), Some("sub_1"));
        assert!(item.subagent_logs.len() <= MAX_SUBAGENT_LOGS);
        assert!(item.subagent_log_bytes <= MAX_SUBAGENT_LOG_BYTES);

        let sealed = store.seal_active_block(AfterTurnPolicy::Collapse, true);
        assert!(sealed.is_some());
        let block = store.get(block_id).unwrap();
        assert!(!block.active);
        assert!(block.is_collapsed());
        assert_eq!(block.items[0].status, ToolStatus::Cancelled);
    }

    #[test]
    fn lazy_detail_cache_hits_and_invalidates_on_rev_or_width_change() {
        let mut store = ToolBlockStore::default();
        let hl = HighlightCache::default();
        let block_id = store.on_tool_started(
            1,
            0,
            "run_shell".into(),
            json!({"command": "cargo test"}),
            true,
        );
        let a = store.detail_lines(block_id, 50, &hl);
        let b = store.detail_lines(block_id, 50, &hl);
        assert!(Rc::ptr_eq(&a, &b));
        assert_eq!(store.detail_cache_stats(), (1, 1));

        // Streaming chunk bumps rev -> invalidates cache
        store.on_tool_output_chunk(1, 0, OutputStream::Stdout, "running tests\n");
        let c = store.detail_lines(block_id, 50, &hl);
        assert!(!Rc::ptr_eq(&a, &c));
        assert_eq!(store.detail_cache_stats(), (1, 2));

        // Width change invalidates cache
        let d = store.detail_lines(block_id, 40, &hl);
        assert!(!Rc::ptr_eq(&c, &d));
        assert_eq!(store.detail_cache_stats(), (1, 3));
    }

    #[test]
    fn payload_budget_prunes_oldest_sealed_block_keeping_headers() {
        let mut store = ToolBlockStore::default();
        let hl = HighlightCache::default();
        store.set_max_payload_bytes_for_test(200);

        let b1 = store.on_tool_started(1, 0, "run_shell".into(), json!({"command": "cmd1"}), true);
        store.on_tool_finished(1, 0, "run_shell", true, "x".repeat(150), 10);
        store.seal_active_block(AfterTurnPolicy::Fit, false);

        let b2 = store.on_tool_started(1, 0, "run_shell".into(), json!({"command": "cmd2"}), true);
        store.on_tool_finished(1, 0, "run_shell", true, "y".repeat(150), 20);
        store.seal_active_block(AfterTurnPolicy::Fit, false);

        assert!(store.get(b1).unwrap().pruned);
        assert!(store.get(b1).unwrap().items[0].payload_pruned);
        assert!(store.get(b1).unwrap().items[0].output.is_none());
        assert_eq!(store.get(b1).unwrap().items[0].tool, "run_shell");
        assert_eq!(store.get(b1).unwrap().items[0].args["command"], "cmd1");
        assert!(store.payload_bytes() <= 200);
        assert!(!store.get(b2).unwrap().pruned);

        let pruned_view = store.detail_lines(b1, 50, &hl);
        let text: String = pruned_view
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Details pruned after retention"));
    }

    #[test]
    fn build_from_turn_steps_restores_completed_cancelled_and_pruned() {
        let mut store = ToolBlockStore::default();
        let steps = vec![AgentStepRecord {
            step: 1,
            model_response: String::new(),
            tool_calls: vec![
                ToolCallRequest {
                    tool: "read_file".into(),
                    args: json!({"path": "src/lib.rs"}),
                },
                ToolCallRequest {
                    tool: "edit_file".into(),
                    args: json!({"path": "src/lib.rs", "old_text": "a", "new_text": "b"}),
                },
            ],
            observations: vec![ToolOutput {
                tool: "read_file".into(),
                ok: true,
                output: "fn main() {}".into(),
                artifacts: vec![],
                elapsed_ms: 5,
            }],
            notes: vec![],
        }];
        let (id, _) = store
            .build_from_turn_steps(&steps, false, AfterTurnPolicy::Fit, true)
            .unwrap();
        let block = store.get(id).unwrap();
        assert_eq!(block.items[0].status, ToolStatus::Done(true));
        assert_eq!(block.items[1].status, ToolStatus::Cancelled);

        let (pruned_id, _) = store
            .build_from_turn_steps(&steps, true, AfterTurnPolicy::Fit, true)
            .unwrap();
        assert!(store.get(pruned_id).unwrap().pruned);
        assert!(store.get(pruned_id).unwrap().items[0].payload_pruned);
        assert!(store.get(pruned_id).unwrap().items[0].output.is_none());
    }

    #[test]
    fn read_file_displays_line_range_in_list_header_and_body_without_double_numbering() {
        let mut store = ToolBlockStore::default();
        let b = store.on_tool_started(
            1,
            0,
            "read_file".into(),
            json!({"path": "src/very/deep/module/render.rs", "start_line": 100, "end_line": 102}),
            true,
        );
        // Before completion: left list row preserves `:100-102` even when narrow, and Inspector
        // header & body explicitly show `lines 100..=102`.
        let block = store.get(b).unwrap();
        let row_text: String = render_tool_item_row(&block.items[0], true, 36, "⠋")
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert!(
            row_text.contains("render.rs:100-102"),
            "narrow left row must keep filename and range: {row_text:?}"
        );
        let hdr_before: String = inspector_header_line(block, 90, "⠋")
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert!(hdr_before.contains("lines 100..=102"), "{hdr_before:?}");

        // After completion with real ReadFileTool output: shows `lines 100..=102 of 644`
        // and renders single clean line numbers (NO double numbering).
        store.on_tool_finished(
            1,
            0,
            "read_file",
            true,
            "File: src/very/deep/module/render.rs (lines 100..=102 of 644)\n 100 | fn alpha() {}\n 101 | fn beta() {}\n 102 | fn gamma() {}\n... [542 more line(s); call read_file with start_line=103]".into(),
            6,
        );
        let block = store.get(b).unwrap();
        let hdr_after: String = inspector_header_line(block, 90, "⠋")
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert!(
            hdr_after.contains("lines 100..=102 of 644"),
            "{hdr_after:?}"
        );

        let hl = HighlightCache::default();
        let body_text: String = store
            .detail_lines(b, 80, &hl)
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body_text.contains("lines 100..=102 of 644"), "{body_text}");
        assert!(body_text.contains(" 100 │ fn alpha() {}"), "{body_text}");
        assert!(
            !body_text.contains("100 │ File:"),
            "ReadFileTool header must not be double-numbered: {body_text}"
        );
        assert!(
            !body_text.contains("101 │  100 |"),
            "ReadFileTool code lines must not be double-numbered: {body_text}"
        );
        assert!(body_text.contains("... [542 more line(s)"), "{body_text}");

        // Default read_file (no explicit start/end args) still shows explicit default range before
        // output and actual resolved range after output.
        let b2 = store.on_tool_started(
            2,
            0,
            "read_file".into(),
            json!({"path": "Cargo.toml"}),
            true,
        );
        let default_item = &store.get(b2).unwrap().items[1];
        assert_eq!(default_item.compact_option_suffix(), ":1-500");
        assert_eq!(
            default_item.read_file_range_label().as_deref(),
            Some("lines 1..=500 (default)")
        );
        store.on_tool_finished(
            2,
            0,
            "read_file",
            true,
            "File: Cargo.toml (lines 1..=42 of 42)\n   1 | [package]".into(),
            3,
        );
        let resolved_item = &store.get(b2).unwrap().items[1];
        assert_eq!(resolved_item.compact_option_suffix(), ":1-42");
        assert_eq!(
            resolved_item.read_file_range_label().as_deref(),
            Some("lines 1..=42 of 42")
        );

        // Compact call options for other tools.
        let grep_b = store.on_tool_started(
            3,
            0,
            "grep_search".into(),
            json!({"pattern": "State", "path": "src/tui", "glob": "*.rs", "output_mode": "files_with_matches"}),
            true,
        );
        assert_eq!(
            store
                .get(grep_b)
                .unwrap()
                .items
                .last()
                .unwrap()
                .compact_option_suffix(),
            " (in src/tui, *.rs, files)"
        );
    }

    #[test]
    fn feed_mode_caches_completed_items_abbreviates_huge_items_and_single_view_shows_full() {
        let mut store = ToolBlockStore::default();
        let big_read = (1..=80)
            .map(|i| format!("line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let b = store.on_tool_started(1, 0, "read_file".into(), json!({"path": "a.rs"}), true);
        store.on_tool_finished(1, 0, "read_file", true, big_read, 5);
        store.on_tool_started(
            2,
            0,
            "run_shell".into(),
            json!({"command": "cargo test"}),
            true,
        );

        let hl = HighlightCache::default();
        // First Feed render wraps both items once (wrap_count == 2), abbreviates the 80-line
        // earlier item in Live Feed to keep CPU/RAM light, keeping head & tail visible.
        let feed1 = store.detail_lines(b, 60, &hl);
        assert_eq!(store.item_wrap_count(), 2);
        let text1: String = feed1
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text1.contains("line-01") && text1.contains("line-80"));
        assert!(
            text1.contains("lines omitted in Live Feed"),
            "very long completed item should abbreviate in Live Feed: {text1}"
        );

        // Streaming 5 live chunks into item #1 MUST NOT re-wrap completed item #0 (only +1 wrap per chunk)!
        for c in 1..=5 {
            store.on_tool_output_chunk(2, 0, OutputStream::Stdout, &format!("chunk-{c}\n"));
            let _ = store.detail_lines(b, 60, &hl);
        }
        assert_eq!(
            store.item_wrap_count(),
            2 + 5,
            "completed earlier tool must hit per-item cache with zero re-wraps on streaming chunks"
        );

        // Selecting item #0 directly (Single-Tool view) shows 100% of all 80 lines with 0 re-wraps!
        store.select_item(b, 0);
        let single = store.detail_lines(b, 60, &hl);
        assert_eq!(
            store.item_wrap_count(),
            2 + 5,
            "switching to single-tool view must reuse the per-item cache"
        );
        let single_text: String = single
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            single_text.contains("line-01")
                && single_text.contains("line-45")
                && single_text.contains("line-80"),
            "single-tool Inspector must show 100% of received content untruncated: {single_text}"
        );
        assert!(!single_text.contains("lines omitted"));
    }

    #[test]
    fn tool_finish_frees_live_chunks_and_active_mutations_keep_layout_rev_stable() {
        let mut store = ToolBlockStore::default();
        let rev0 = store.layout_rev();
        let b = store.on_tool_started(
            1,
            0,
            "run_shell".into(),
            json!({"command": "echo hi"}),
            true,
        );
        assert_eq!(
            store.layout_rev(),
            rev0,
            "active block tool start must not invalidate sealed transcript LineCache"
        );
        store.on_tool_output_chunk(1, 0, OutputStream::Stdout, &"a".repeat(2048));
        let bytes_while_live = store.payload_bytes();
        assert!(bytes_while_live >= 2048);

        store.on_tool_finished(1, 0, "run_shell", true, "done".into(), 12);
        let item = &store.get(b).unwrap().items[0];
        assert!(
            item.live_chunks.is_empty() && item.live_bytes == 0,
            "final non-empty output must release duplicate live_chunks RAM immediately"
        );
        assert!(
            store.payload_bytes() < 256,
            "payload_bytes must drop after freeing live_chunks: {}",
            store.payload_bytes()
        );

        assert!(store
            .seal_active_block(AfterTurnPolicy::Fit, false)
            .is_some());
        assert_eq!(
            store.layout_rev(),
            rev0,
            "sealing active block appends incrementally and must not reset sealed LineCache"
        );
    }

    #[test]
    fn ast_tools_render_structured_inspector_without_double_numbering() {
        let mut store = ToolBlockStore::default();

        // 1. find_symbol (include_body=true) must NOT stamp fake line numbers on headers or double-number body.
        let b = store.on_tool_started(
            1,
            0,
            "find_symbol".into(),
            json!({"name": "Grep::execute", "path": "src", "include_body": true}),
            false,
        );
        store.on_tool_finished(
            1,
            0,
            "find_symbol",
            true,
            "Found 1 definition(s) of 'Grep::execute':\nsrc/lib.rs:40-42  Grep::execute  // Runs grep\n  40 | fn execute() {\n  41 |     run()\n  42 | }\n... [10 more line(s); call read_file with start_line=43]".into(),
            8,
        );
        let hl = HighlightCache::default();
        let sym_lines = store.detail_lines(b, 90, &hl);
        let sym_text: String = sym_lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            sym_text.contains("Grep::execute · path=src · include_body=true"),
            "{sym_text}"
        );
        assert!(sym_text.contains("Found 1 definition(s)"), "{sym_text}");
        assert!(sym_text.contains("  40 │ fn execute() {"), "{sym_text}");
        assert!(
            !sym_text.contains("   1 │ Found"),
            "find_symbol header must NOT get fake line numbers: {sym_text}"
        );
        assert!(
            !sym_text.contains("   3 │   40 |"),
            "find_symbol body must NOT get double line numbers: {sym_text}"
        );

        // 2. code_outline structured rendering
        store.on_tool_started(
            1,
            1,
            "code_outline".into(),
            json!({"path": "src/lib.rs"}),
            false,
        );
        store.on_tool_finished(
            1,
            1,
            "code_outline",
            true,
            "src/lib.rs (rust, 120 lines, 2 definitions)\n10-30  pub struct App  // Main app\n  12-20  pub fn run(&self)".into(),
            5,
        );
        store.select_item(b, 1);
        let outline_lines = store.detail_lines(b, 90, &hl);
        assert!(
            outline_lines.iter().any(|l| l
                .iter()
                .any(|s| s.text.contains("src/lib.rs (rust") && s.face == Face::ACCENT_BOLD)),
            "code_outline file header should be ACCENT_BOLD"
        );
        assert!(
            outline_lines.iter().any(|l| l
                .iter()
                .any(|s| s.text == "10-30  " && s.face == Face::MUTED)
                && l.iter()
                    .any(|s| s.text == "pub struct App" && s.face == Face::CODE)),
            "code_outline entries should split line span and signature faces"
        );

        // 3. find_references structured rendering
        store.on_tool_started(
            1,
            2,
            "find_references".into(),
            json!({"name": "Grep::execute", "path": "src", "include_tests": true, "max_results": 30}),
            false,
        );
        store.on_tool_finished(
            1,
            2,
            "find_references",
            true,
            "Found 1 reference(s) to 'Grep::execute' in 1 file(s):\nsrc/main.rs\n  18 [fn main]  grep.execute()".into(),
            7,
        );
        store.select_item(b, 2);
        let ref_lines = store.detail_lines(b, 90, &hl);
        assert!(
            ref_lines.iter().any(|l| l
                .iter()
                .any(|s| s.text == "src/main.rs" && s.face == Face::ACCENT_BOLD)),
            "find_references file header should be ACCENT_BOLD"
        );
        assert!(
            ref_lines.iter().any(|l| l
                .iter()
                .any(|s| s.text.contains("[fn main]") && s.face == Face::ACCENT)
                && l.iter()
                    .any(|s| s.text.contains("grep.execute()") && s.face == Face::CODE)),
            "find_references rows should highlight enclosing scope and call line"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn inspector_async_highlighting_renders_plain_first_then_upgrades_without_reflow() {
        use super::super::{highlight::HighlightWorker, markdown::SyntaxRole, worker::Update};
        use std::time::Duration;

        let plain_of = |lines: &[markdown::Line]| -> String {
            lines
                .iter()
                .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let mut hl = HighlightCache::default();
        hl.attach_worker(HighlightWorker::spawn(tx));

        let mut store = ToolBlockStore::default();
        // 0: read_file (Rust with pre-numbered ReadFileTool output)
        let b = store.on_tool_started(
            1,
            0,
            "read_file".into(),
            json!({"path": "src/demo.rs", "start_line": 10, "end_line": 12}),
            false,
        );
        store.on_tool_finished(
            1,
            0,
            "read_file",
            true,
            "File: src/demo.rs (lines 10..=12 of 50)\n  10 | pub fn greet(name: &str) -> String {\n  11 |     // greeting\n  12 |     format!(\"hi {name}\")\n... [38 more line(s)]".into(),
            4,
        );
        // 1: edit_file (Rust diff with old_text + new_text)
        store.on_tool_started(
            1,
            1,
            "edit_file".into(),
            json!({
                "path": "src/demo.rs",
                "old_text": "let old = 1;",
                "new_text": "let new = \"two\";"
            }),
            false,
        );
        // 2: write_file (Python)
        store.on_tool_started(
            1,
            2,
            "write_file".into(),
            json!({
                "path": "scripts/run.py",
                "content": "def main():\n    return \"ok\"\n"
            }),
            false,
        );
        // 3: run_shell with unified diff output (`git diff`)
        store.on_tool_started(
            1,
            3,
            "run_shell".into(),
            json!({"command": "git diff src/demo.rs"}),
            false,
        );
        store.on_tool_finished(
            1,
            3,
            "run_shell",
            true,
            "diff --git a/src/demo.rs b/src/demo.rs\n--- a/src/demo.rs\n+++ b/src/demo.rs\n@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = \"hi\";".into(),
            9,
        );

        // Capture immediate plain first-frame outputs (schedules all items in background).
        let mut before = Vec::new();
        for idx in 0..4 {
            store.select_item(b, idx);
            let lines = store.detail_lines(b, 50, &hl);
            assert!(
                lines
                    .iter()
                    .flatten()
                    .all(|s| s.face.syntax == SyntaxRole::None),
                "first paint for item {idx} must be instant plain text without blocking"
            );
            before.push((lines.len(), plain_of(&lines)));
        }

        // Wait for parallel background worker jobs to finish and apply into HighlightCache.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            while let Ok(Update::HighlightReady { key, doc }) = rx.try_recv() {
                hl.apply_result(key, doc);
            }
            if hl.rev() >= 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Re-render every item: exact same line count and plain text (ZERO reflow), now syntax-colored!
        for (idx, (before_len, before_text)) in before.iter().enumerate() {
            store.select_item(b, idx);
            let after = store.detail_lines(b, 50, &hl);
            assert_eq!(
                after.len(),
                *before_len,
                "item {idx} wrapped line count must stay identical after highlight"
            );
            assert_eq!(
                plain_of(&after),
                *before_text,
                "item {idx} text must stay identical after highlight"
            );
            assert!(
                after.iter().flatten().any(
                    |s| s.face.syntax != SyntaxRole::None && s.face.syntax != SyntaxRole::Plain
                ),
                "item {idx} must now contain semantic syntax tokens"
            );
        }

        // Specifically verify edit_file keeps DiffBg::Removed / DiffBg::Added alongside syntax roles.
        store.select_item(b, 1);
        let edit_lines = store.detail_lines(b, 50, &hl);
        assert!(
            edit_lines.iter().flatten().any(|s| s.text == "let"
                && s.face.syntax == SyntaxRole::Keyword
                && s.face.diff_bg == DiffBg::Removed),
            "edit_file removed line must have Keyword fg + Removed diff bg"
        );
        assert!(
            edit_lines.iter().flatten().any(|s| s.text.contains("two")
                && s.face.syntax == SyntaxRole::String
                && s.face.diff_bg == DiffBg::Added),
            "edit_file added line must have String fg + Added diff bg"
        );
    }

    #[test]
    fn grep_search_highlights_matched_substrings_immediately_and_overlays_async_syntax() {
        let plain_of = |lines: &[markdown::Line]| -> String {
            lines
                .iter()
                .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut store = ToolBlockStore::default();
        let b = store.on_tool_started(
            1,
            0,
            "grep_search".into(),
            json!({"pattern": "render_\\w+", "path": "src", "context": 1}),
            false,
        );
        store.on_tool_finished(
            1,
            0,
            "grep_search",
            true,
            concat!(
                "Found 2 match(es) in 1 file(s):\n",
                "src/tui/render.rs\n",
                "10-pub fn setup() {}\n",
                "11:pub fn render_frame() { render_footer(); }\n",
                "--\n",
                "42:    // call render_header\n",
                "[stopped at head_limit=50; more results may exist - narrow pattern/path/glob]"
            )
            .into(),
            6,
        );

        // Attach async worker (real TUI flow): frame 1 STILL renders plain + Face::SEARCH_MATCH
        // immediately without blocking, and schedules background Tree-sitter highlighting.
        #[cfg(feature = "tree-sitter")]
        let (mut hl, mut rx) = {
            use super::super::highlight::HighlightWorker;
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let mut cache = HighlightCache::default();
            cache.attach_worker(HighlightWorker::spawn(tx));
            (cache, rx)
        };
        #[cfg(not(feature = "tree-sitter"))]
        let hl = HighlightCache::default();

        let first = store.detail_lines(b, 80, &hl);
        let first_text = plain_of(&first);
        assert!(first_text.contains("src/tui/render.rs"), "{first_text}");
        assert!(
            first_text.contains("  11 │ pub fn render_frame() { render_footer(); }"),
            "{first_text}"
        );
        assert!(
            first_text.contains("  10 · pub fn setup() {}"),
            "{first_text}"
        );
        let matched_texts: Vec<&str> = first
            .iter()
            .flatten()
            .filter(|s| s.face == Face::SEARCH_MATCH)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(
            matched_texts,
            vec!["render_frame", "render_footer", "render_header"]
        );

        // Once async Tree-sitter worker finishes: non-matched code upgrades to syntax colors
        // while matched substrings remain Face::SEARCH_MATCH with ZERO reflow!
        #[cfg(feature = "tree-sitter")]
        {
            use super::super::{markdown::SyntaxRole, worker::Update};
            use std::time::Duration;

            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && hl.rev() == 0 {
                while let Ok(Update::HighlightReady { key, doc }) = rx.try_recv() {
                    hl.apply_result(key, doc);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(hl.rev() > 0);
            let second = store.detail_lines(b, 80, &hl);
            assert_eq!(second.len(), first.len(), "no reflow on syntax upgrade");
            assert_eq!(plain_of(&second), first_text, "exact text preserved");
            assert!(
                second
                    .iter()
                    .flatten()
                    .any(|s| s.text == "fn" && s.face.syntax == SyntaxRole::Keyword),
                "surrounding Rust tokens must be syntax highlighted"
            );
            let matched_after: Vec<&str> = second
                .iter()
                .flatten()
                .filter(|s| s.face == Face::SEARCH_MATCH)
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(
                matched_after,
                vec!["render_frame", "render_footer", "render_header"]
            );
        }

        // Smart-case + fixed_strings
        store.on_tool_started(
            1,
            1,
            "grep_search".into(),
            json!({"pattern": "Foo[0]", "fixed_strings": true}),
            false,
        );
        store.on_tool_finished(
            1,
            1,
            "grep_search",
            true,
            "Found 1 match(es) in 1 file(s):\nnotes.txt\n3:value = Foo[0] and foo[0]".into(),
            2,
        );
        store.select_item(b, 1);
        let fixed_lines = store.detail_lines(b, 80, &hl);
        let fixed_matches: Vec<&str> = fixed_lines
            .iter()
            .flatten()
            .filter(|s| s.face == Face::SEARCH_MATCH)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(
            fixed_matches,
            vec!["Foo[0]"],
            "smart-case uppercase literal must match Foo[0] only"
        );
    }
}
