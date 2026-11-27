//! Asynchronous, parallel, content-addressed Tree-sitter syntax highlighting for the TUI.
//!
//! Design invariants:
//! - Main UI thread NEVER runs Tree-sitter parsing synchronously during rendering.
//! - First paint returns plain text immediately (zero input/scroll latency) and queues
//!   background parsing on a bounded parallel worker pool.
//! - Results are width- and theme-independent (`TokenSpan` byte ranges + semantic `SyntaxRole`),
//!   so terminal resizes, divider drags, and `/theme` switches require zero re-parsing.
//! - Converting plain text -> syntax-highlighted spans preserves exact text and line count,
//!   guaranteeing zero layout reflow or scroll jumps when highlights land.

use super::{
    markdown::{self, DiffBg, Face, SyntaxRole},
    worker::Update,
};
use crate::tree_sitter_diff::SupportedLanguage;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

#[cfg(feature = "tree-sitter")]
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Upper bounds protecting CPU and memory against huge/minified outputs.
const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 8_000;
const MAX_SINGLE_LINE_BYTES: usize = 4_096;
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;
#[cfg(feature = "tree-sitter")]
const MAX_PENDING_JOBS: usize = 256;
#[cfg(feature = "tree-sitter")]
const HIGHLIGHT_TIMEOUT_MICROS: u64 = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum HighlightLang {
    Code(SupportedLanguage),
    UnifiedDiff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct HighlightKey {
    pub lang: HighlightLang,
    pub digest: [u8; 16],
    pub len: usize,
}

impl HighlightKey {
    pub(super) fn new(lang: HighlightLang, cleaned_source: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(cleaned_source.as_bytes());
        let full = hasher.finalize();
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&full[..16]);
        Self {
            lang,
            digest,
            len: cleaned_source.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TokenSpan {
    pub start: u32,
    pub end: u32,
    pub role: SyntaxRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiffLineKind {
    FileHeader,
    HunkHeader,
    Context,
    Added,
    Removed,
    Meta,
}

#[cfg_attr(not(feature = "tree-sitter"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HighlightedDoc {
    Code {
        /// One entry per `\n`-split line of the cleaned source text.
        /// Contains sorted, non-overlapping non-Plain token spans with line-relative byte offsets.
        lines: Vec<Vec<TokenSpan>>,
    },
    Diff {
        /// One entry per `\n`-split line of the cleaned unified diff.
        /// For `Added`/`Removed`/`Context`, `TokenSpan` offsets are relative to the
        /// code slice AFTER the 1-byte `+`/`-`/` ` prefix.
        lines: Vec<(DiffLineKind, Vec<TokenSpan>)>,
    },
}

impl HighlightedDoc {
    fn estimated_bytes(&self) -> usize {
        match self {
            Self::Code { lines } => {
                lines.len() * std::mem::size_of::<Vec<TokenSpan>>()
                    + lines
                        .iter()
                        .map(|l| l.len() * std::mem::size_of::<TokenSpan>())
                        .sum::<usize>()
            }
            Self::Diff { lines } => {
                lines.len() * std::mem::size_of::<(DiffLineKind, Vec<TokenSpan>)>()
                    + lines
                        .iter()
                        .map(|(_, l)| l.len() * std::mem::size_of::<TokenSpan>())
                        .sum::<usize>()
            }
        }
    }

    pub(super) fn code_lines(&self) -> Option<&[Vec<TokenSpan>]> {
        match self {
            Self::Code { lines } => Some(lines),
            Self::Diff { .. } => None,
        }
    }

    pub(super) fn diff_lines(&self) -> Option<&[(DiffLineKind, Vec<TokenSpan>)]> {
        match self {
            Self::Diff { lines } => Some(lines),
            Self::Code { .. } => None,
        }
    }
}

pub(super) enum Lookup {
    Ready(Arc<HighlightedDoc>),
    Pending,
    Unsupported,
}

/// Checks whether a cleaned text payload is within safe bounds for Tree-sitter highlighting.
pub(super) fn within_bounds(cleaned_source: &str) -> bool {
    if cleaned_source.is_empty() || cleaned_source.len() > MAX_HIGHLIGHT_BYTES {
        return false;
    }
    let mut lines = 0usize;
    for line in cleaned_source.split('\n') {
        lines += 1;
        if lines > MAX_HIGHLIGHT_LINES || line.len() > MAX_SINGLE_LINE_BYTES {
            return false;
        }
    }
    true
}

/// Heuristic detecting unified diff output (e.g. `git diff`, `git show`, `diff -u`).
pub(super) fn looks_like_unified_diff(cleaned: &str) -> bool {
    let mut saw_minus_hdr = false;
    let mut saw_plus_hdr = false;
    let mut saw_hunk = false;
    for line in cleaned.split('\n').take(32) {
        if line.starts_with("diff --git ") {
            return true;
        }
        if line.starts_with("--- ") {
            saw_minus_hdr = true;
        } else if line.starts_with("+++ ") && saw_minus_hdr {
            saw_plus_hdr = true;
        } else if line.starts_with("@@ -") && saw_plus_hdr {
            saw_hunk = true;
            break;
        }
    }
    saw_hunk
}

/// Resolves a Markdown fence info string to either a supported Tree-sitter language or unified diff.
pub(super) fn lang_from_fence_tag(info: &str) -> Option<HighlightLang> {
    let token = info
        .trim()
        .split(|c: char| c.is_ascii_whitespace() || matches!(c, ',' | ';' | '{' | ':'))
        .next()?
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if matches!(token.as_str(), "diff" | "patch" | "udiff") {
        return Some(HighlightLang::UnifiedDiff);
    }
    SupportedLanguage::from_fence_tag(info).map(HighlightLang::Code)
}

/// Converts a single cleaned source line and its sorted non-overlapping `TokenSpan`s into
/// `(slice, Face)` parts covering the entire line with zero dropped or duplicated characters.
pub(super) fn line_spans<'a>(
    raw_line: &'a str,
    tokens: &[TokenSpan],
    diff_bg: DiffBg,
) -> Vec<(&'a str, Face)> {
    let plain_face = Face::syntax(SyntaxRole::Plain).with_diff_bg(diff_bg);
    if raw_line.is_empty() {
        return Vec::new();
    }
    if tokens.is_empty() {
        return vec![(raw_line, plain_face)];
    }
    let mut out = Vec::with_capacity(tokens.len() * 2 + 1);
    let mut cursor = 0usize;
    let len = raw_line.len();
    for tok in tokens {
        let start = (tok.start as usize).min(len);
        let end = (tok.end as usize).min(len);
        if start < cursor
            || start >= end
            || !raw_line.is_char_boundary(start)
            || !raw_line.is_char_boundary(end)
        {
            continue;
        }
        if start > cursor {
            out.push((&raw_line[cursor..start], plain_face));
        }
        out.push((
            &raw_line[start..end],
            Face::syntax(tok.role).with_diff_bg(diff_bg),
        ));
        cursor = end;
    }
    if cursor < len {
        out.push((&raw_line[cursor..], plain_face));
    }
    out
}

/// Classifies a single unified-diff line without running Tree-sitter (used for immediate plain
/// diff rendering before async syntax results arrive, and as fallback for unsupported files).
pub(super) fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("new file mode")
        || line.starts_with("deleted file mode")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("similarity index ")
        || line.starts_with("Binary files ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
    {
        DiffLineKind::FileHeader
    } else if line.starts_with("@@ ") {
        DiffLineKind::HunkHeader
    } else if line.starts_with("\\ ") {
        DiffLineKind::Meta
    } else if line.starts_with('+') {
        DiffLineKind::Added
    } else if line.starts_with('-') {
        DiffLineKind::Removed
    } else {
        DiffLineKind::Context
    }
}

/// Renders a single line of a unified diff with either cached Tree-sitter syntax tokens or
/// clean immediate diff coloring (always preserving exact text and wrap width).
pub(super) fn push_unified_diff_line(
    out: &mut Vec<markdown::Line>,
    raw_line: &str,
    highlighted: Option<&(DiffLineKind, Vec<TokenSpan>)>,
    width: usize,
) {
    let kind = highlighted.map_or_else(|| classify_diff_line(raw_line), |(k, _)| *k);
    let tokens = highlighted.map_or(&[][..], |(_, t)| t.as_slice());

    match kind {
        DiffLineKind::FileHeader => {
            let face = if raw_line.starts_with("diff --git ")
                || raw_line.starts_with("--- ")
                || raw_line.starts_with("+++ ")
            {
                Face::ACCENT_BOLD
            } else {
                Face::MUTED
            };
            out.extend(markdown::literal(raw_line, face, width));
        }
        DiffLineKind::HunkHeader => {
            if let Some(close) = raw_line[3..].find(" @@") {
                let split_at = 3 + close + 3;
                let (hdr, ctx) = raw_line.split_at(split_at);
                if ctx.is_empty() {
                    out.extend(markdown::literal(hdr, Face::ACCENT, width));
                } else {
                    out.extend(markdown::spans(
                        &[(hdr, Face::ACCENT), (ctx, Face::MUTED)],
                        width,
                    ));
                }
            } else {
                out.extend(markdown::literal(raw_line, Face::ACCENT, width));
            }
        }
        DiffLineKind::Meta => {
            out.extend(markdown::literal(raw_line, Face::MUTED, width));
        }
        DiffLineKind::Added => {
            let body = raw_line.strip_prefix('+').unwrap_or(raw_line);
            let gutter_face = Face::HIGHLIGHT_BOLD.with_diff_bg(DiffBg::Added);
            if !tokens.is_empty() {
                let parts = line_spans(body, tokens, DiffBg::Added);
                out.extend(markdown::prefixed_spans("+", gutter_face, &parts, width));
            } else {
                let body_face = Face::HIGHLIGHT.with_diff_bg(DiffBg::Added);
                out.extend(markdown::prefixed("+", gutter_face, body, body_face, width));
            }
        }
        DiffLineKind::Removed => {
            let body = raw_line.strip_prefix('-').unwrap_or(raw_line);
            let gutter_face = Face::DANGER_BOLD.with_diff_bg(DiffBg::Removed);
            if !tokens.is_empty() {
                let parts = line_spans(body, tokens, DiffBg::Removed);
                out.extend(markdown::prefixed_spans("-", gutter_face, &parts, width));
            } else {
                let body_face = Face::DANGER.with_diff_bg(DiffBg::Removed);
                out.extend(markdown::prefixed("-", gutter_face, body, body_face, width));
            }
        }
        DiffLineKind::Context => {
            if let Some(body) = raw_line.strip_prefix(' ') {
                if !tokens.is_empty() {
                    let parts = line_spans(body, tokens, DiffBg::None);
                    out.extend(markdown::prefixed_spans(" ", Face::MUTED, &parts, width));
                } else {
                    out.extend(markdown::prefixed(
                        " ",
                        Face::MUTED,
                        body,
                        Face::CODE,
                        width,
                    ));
                }
            } else if !tokens.is_empty() {
                let parts = line_spans(raw_line, tokens, DiffBg::None);
                out.extend(markdown::spans(&parts, width));
            } else {
                out.extend(markdown::literal(raw_line, Face::CODE, width));
            }
        }
    }
}

/// Renders a whole cleaned unified diff string using cached highlights if ready, or immediate
/// structural diff styling while background parsing completes.
pub(super) fn push_unified_diff(
    out: &mut Vec<markdown::Line>,
    cleaned: &str,
    doc: Option<&HighlightedDoc>,
    width: usize,
) {
    let diff_lines = doc.and_then(HighlightedDoc::diff_lines);
    for (idx, raw_line) in cleaned.split('\n').enumerate() {
        let hl = diff_lines.and_then(|lines| lines.get(idx));
        push_unified_diff_line(out, raw_line, hl, width);
    }
}

// ---------------------------------------------------------------------------
// Content-addressed bounded cache & async worker pool
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter")]
struct HighlightJob {
    key: HighlightKey,
    source: String,
}

#[cfg(feature = "tree-sitter")]
pub(super) struct HighlightWorker {
    queue: Arc<(Mutex<VecDeque<HighlightJob>>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

#[cfg(feature = "tree-sitter")]
impl HighlightWorker {
    pub(super) fn spawn(updates: tokio::sync::mpsc::Sender<Update>) -> Self {
        let queue = Arc::new((Mutex::new(VecDeque::<HighlightJob>::new()), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_count = thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
        let mut threads = Vec::with_capacity(worker_count);

        for idx in 0..worker_count {
            let queue_w = Arc::clone(&queue);
            let shutdown_w = Arc::clone(&shutdown);
            let tx_w = updates.clone();
            let handle = thread::Builder::new()
                .name(format!("anticrumb-hl-{idx}"))
                .spawn(move || loop {
                    let job = {
                        let (lock, cv) = &*queue_w;
                        let mut guard = match lock.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        while guard.is_empty() && !shutdown_w.load(Ordering::Acquire) {
                            guard = match cv.wait(guard) {
                                Ok(g) => g,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                        }
                        if shutdown_w.load(Ordering::Acquire) {
                            break;
                        }
                        // LIFO: prioritize the most recently rendered (visible) snippet first.
                        guard.pop_back()
                    };
                    let Some(job) = job else {
                        continue;
                    };
                    if shutdown_w.load(Ordering::Acquire) {
                        break;
                    }
                    let doc = compute_highlight(job.key.lang, &job.source).map(Arc::new);
                    let mut pending = Update::HighlightReady { key: job.key, doc };
                    loop {
                        if shutdown_w.load(Ordering::Acquire) {
                            return;
                        }
                        match tx_w.try_send(pending) {
                            Ok(()) => break,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                                pending = returned;
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                return;
                            }
                        }
                    }
                })
                .expect("spawn syntax highlight worker thread");
            threads.push(handle);
        }

        Self {
            queue,
            shutdown,
            threads,
        }
    }

    fn enqueue(&self, key: HighlightKey, source: String) -> bool {
        if self.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let (lock, cv) = &*self.queue;
        let Ok(mut guard) = lock.lock() else {
            return false;
        };
        if guard.len() >= MAX_PENDING_JOBS {
            return false;
        }
        guard.push_back(HighlightJob { key, source });
        cv.notify_one();
        true
    }
}

#[cfg(feature = "tree-sitter")]
impl Drop for HighlightWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let (lock, cv) = &*self.queue;
        if let Ok(mut guard) = lock.lock() {
            guard.clear();
            cv.notify_all();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(not(feature = "tree-sitter"))]
pub(super) struct HighlightWorker;

#[cfg(not(feature = "tree-sitter"))]
impl HighlightWorker {
    pub(super) fn spawn(_updates: tokio::sync::mpsc::Sender<Update>) -> Self {
        Self
    }

    fn enqueue(&self, _key: HighlightKey, _source: String) -> bool {
        false
    }
}

#[derive(Default)]
pub(super) struct HighlightCache {
    entries: HashMap<HighlightKey, Arc<HighlightedDoc>>,
    order: VecDeque<HighlightKey>,
    cache_bytes: usize,
    in_flight: RefCell<HashSet<HighlightKey>>,
    failed: HashSet<HighlightKey>,
    rev: u64,
    worker: Option<HighlightWorker>,
}

impl HighlightCache {
    pub(super) fn rev(&self) -> u64 {
        self.rev
    }

    pub(super) fn attach_worker(&mut self, worker: HighlightWorker) {
        self.worker = Some(worker);
    }

    pub(super) fn detach_worker(&mut self) {
        self.worker = None;
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.cache_bytes = 0;
        self.in_flight.borrow_mut().clear();
        self.failed.clear();
        self.rev = self.rev.wrapping_add(1);
    }

    /// Look up cached highlight spans for `(lang, cleaned_source)`.
    /// On cache miss with an attached async worker, schedules background parsing (deduplicated)
    /// and returns `Lookup::Pending` immediately without blocking.
    pub(super) fn lookup_or_schedule(&self, lang: HighlightLang, cleaned_source: &str) -> Lookup {
        if !within_bounds(cleaned_source) {
            return Lookup::Unsupported;
        }
        let key = HighlightKey::new(lang, cleaned_source);
        if let Some(doc) = self.entries.get(&key) {
            return Lookup::Ready(Arc::clone(doc));
        }
        if self.failed.contains(&key) {
            return Lookup::Unsupported;
        }
        let Some(worker) = self.worker.as_ref() else {
            return Lookup::Unsupported;
        };
        let mut in_flight = self.in_flight.borrow_mut();
        if in_flight.contains(&key) {
            return Lookup::Pending;
        }
        if worker.enqueue(key, cleaned_source.to_owned()) {
            in_flight.insert(key);
            Lookup::Pending
        } else {
            Lookup::Unsupported
        }
    }

    /// Inserts a completed background result. Returns `true` if visible state changed.
    pub(super) fn apply_result(
        &mut self,
        key: HighlightKey,
        doc: Option<Arc<HighlightedDoc>>,
    ) -> bool {
        self.in_flight.borrow_mut().remove(&key);
        let Some(doc) = doc else {
            if self.failed.len() >= MAX_CACHE_ENTRIES * 2 {
                self.failed.clear();
            }
            self.failed.insert(key);
            return false;
        };
        if self.entries.contains_key(&key) {
            return false;
        }
        let added_bytes = doc.estimated_bytes();
        self.entries.insert(key, doc);
        self.order.push_back(key);
        self.cache_bytes = self.cache_bytes.saturating_add(added_bytes);
        while self.entries.len() > MAX_CACHE_ENTRIES || self.cache_bytes > MAX_CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.cache_bytes = self.cache_bytes.saturating_sub(evicted.estimated_bytes());
            }
        }
        self.rev = self.rev.wrapping_add(1);
        true
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter highlighting engine (runs strictly on worker threads / tests)
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter")]
struct CompiledHighlightQuery {
    language: tree_sitter::Language,
    query: tree_sitter::Query,
    capture_roles: Vec<(SyntaxRole, u8)>,
}

#[cfg(feature = "tree-sitter")]
fn lang_slot(lang: SupportedLanguage) -> usize {
    match lang {
        SupportedLanguage::Rust => 0,
        SupportedLanguage::Python => 1,
        SupportedLanguage::JavaScript => 2,
        SupportedLanguage::TypeScript => 3,
        SupportedLanguage::Tsx => 4,
        SupportedLanguage::Go => 5,
        SupportedLanguage::C => 6,
        SupportedLanguage::Cpp => 7,
        SupportedLanguage::Java => 8,
        SupportedLanguage::CSharp => 9,
        SupportedLanguage::Bash => 10,
        SupportedLanguage::Ruby => 11,
        SupportedLanguage::Json => 12,
        SupportedLanguage::Dart => 13,
    }
}

#[cfg(feature = "tree-sitter")]
fn compiled_query(lang: SupportedLanguage) -> Option<&'static CompiledHighlightQuery> {
    static QUERIES: [OnceLock<Option<CompiledHighlightQuery>>; 14] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    QUERIES[lang_slot(lang)]
        .get_or_init(|| {
            let language = lang.language_fn();
            let query = tree_sitter::Query::new(&language, lang.highlights_query()).ok()?;
            let capture_roles = query
                .capture_names()
                .iter()
                .map(|name| map_capture_name(name))
                .collect();
            Some(CompiledHighlightQuery {
                language,
                query,
                capture_roles,
            })
        })
        .as_ref()
}

/// Maps upstream Tree-sitter capture names to our compact `SyntaxRole` + tie-breaking specificity.
/// Higher specificity beats lower specificity on identical byte ranges so generic fallback
/// captures (`@variable` -> spec 0) never override semantic captures (`@function`, `@type`, etc.)
/// regardless of query pattern ordering across languages.
#[cfg(feature = "tree-sitter")]
fn map_capture_name(name: &str) -> (SyntaxRole, u8) {
    let base = name.trim_end_matches('.');
    match base {
        "comment" | "comment.block" | "comment.documentation" | "comment.block.documentation" => {
            (SyntaxRole::Comment, 6)
        }
        "escape" | "string.escape" => (SyntaxRole::Constant, 6),
        "string" | "string.special" | "string.special.regex" | "string.special.symbol" => {
            (SyntaxRole::String, 5)
        }
        "string.special.key" => (SyntaxRole::Property, 5),
        "keyword.operator" => (SyntaxRole::Operator, 4),
        "keyword" => (SyntaxRole::Keyword, 5),
        "function.macro" | "attribute" => (SyntaxRole::Macro, 5),
        "constant" | "constant.builtin" | "boolean" | "number" | "number.float"
        | "variable.builtin" | "label" => (SyntaxRole::Constant, 4),
        "function"
        | "function.call"
        | "function.method"
        | "function.method.call"
        | "function.builtin"
        | "function.method.builtin"
        | "function.special" => (SyntaxRole::Function, 4),
        "type" | "type.builtin" | "type.definition" | "type.parameter" | "constructor"
        | "module" => (SyntaxRole::Type, 3),
        "property" | "property.definition" | "tag" => (SyntaxRole::Property, 3),
        "operator" | "punctuation.special" => (SyntaxRole::Operator, 2),
        _ => {
            if base.starts_with("comment.") {
                (SyntaxRole::Comment, 6)
            } else if base.starts_with("string.") {
                (SyntaxRole::String, 5)
            } else if base.starts_with("keyword.") {
                (SyntaxRole::Keyword, 5)
            } else if base.starts_with("function.") {
                (SyntaxRole::Function, 4)
            } else if base.starts_with("type.") {
                (SyntaxRole::Type, 3)
            } else if base.starts_with("constant.") || base.starts_with("number.") {
                (SyntaxRole::Constant, 4)
            } else if base.starts_with("property.") {
                (SyntaxRole::Property, 3)
            } else {
                (SyntaxRole::Plain, 0)
            }
        }
    }
}

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Copy)]
struct RawCapture {
    start: usize,
    end: usize,
    role: SyntaxRole,
    spec: u8,
    pattern_idx: u32,
}

#[cfg(feature = "tree-sitter")]
pub(super) fn compute_highlight(
    lang: HighlightLang,
    cleaned_source: &str,
) -> Option<HighlightedDoc> {
    if !within_bounds(cleaned_source) {
        return None;
    }
    match lang {
        HighlightLang::Code(ts_lang) => highlight_code_lines(ts_lang, cleaned_source)
            .map(|lines| HighlightedDoc::Code { lines }),
        HighlightLang::UnifiedDiff => highlight_unified_diff(cleaned_source),
    }
}

#[cfg(feature = "tree-sitter")]
fn highlight_code_lines(
    lang: SupportedLanguage,
    cleaned_source: &str,
) -> Option<Vec<Vec<TokenSpan>>> {
    use tree_sitter::StreamingIterator;

    let compiled = compiled_query(lang)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&compiled.language).ok()?;
    #[allow(deprecated)]
    parser.set_timeout_micros(HIGHLIGHT_TIMEOUT_MICROS);
    let tree = parser.parse(cleaned_source, None)?;

    let mut cursor = tree_sitter::QueryCursor::new();
    #[allow(deprecated)]
    cursor.set_timeout_micros(HIGHLIGHT_TIMEOUT_MICROS);

    let mut raw_captures = Vec::new();
    let mut captures =
        cursor.captures(&compiled.query, tree.root_node(), cleaned_source.as_bytes());
    while let Some((m, capture_idx)) = captures.next() {
        let Some(cap) = m.captures.get(*capture_idx) else {
            continue;
        };
        let node = cap.node;
        let start = node.start_byte().min(cleaned_source.len());
        let end = node.end_byte().min(cleaned_source.len());
        if start >= end {
            continue;
        }
        let Some(&(role, spec)) = compiled.capture_roles.get(cap.index as usize) else {
            continue;
        };
        raw_captures.push(RawCapture {
            start,
            end,
            role,
            spec,
            pattern_idx: m.pattern_index as u32,
        });
    }
    if cursor.did_exceed_match_limit() {
        return None;
    }

    let lines = resolve_captures_per_line(cleaned_source, &raw_captures);
    if lines.iter().all(Vec::is_empty) {
        return None;
    }
    Some(lines)
}

#[cfg(feature = "tree-sitter")]
fn resolve_captures_per_line(source: &str, captures: &[RawCapture]) -> Vec<Vec<TokenSpan>> {
    let mut line_ranges: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0usize;
    for part in source.split('\n') {
        let end = offset + part.len();
        line_ranges.push((offset, end));
        offset = end + 1;
    }

    let mut out = vec![Vec::new(); line_ranges.len()];
    if captures.is_empty() {
        return out;
    }

    // `QueryCursor::captures` yields captures in ascending `start_byte` order; sort defensively
    // in case overlapping patterns ever arrive slightly out of order.
    let mut sorted: Vec<RawCapture> = captures.to_vec();
    sorted.sort_unstable_by_key(|c| (c.start, std::cmp::Reverse(c.end), c.pattern_idx));

    let mut active: Vec<RawCapture> = Vec::new();
    let mut cap_idx = 0usize;
    let mut boundaries: Vec<usize> = Vec::new();

    for (line_idx, &(line_start, line_end)) in line_ranges.iter().enumerate() {
        active.retain(|c| c.end > line_start);
        while cap_idx < sorted.len() && sorted[cap_idx].start < line_end {
            let c = sorted[cap_idx];
            cap_idx += 1;
            if c.end > line_start {
                active.push(c);
            }
        }
        if line_start == line_end || active.is_empty() {
            continue;
        }

        // Fast skip if every active capture on this line is Plain (spec == 0).
        if active.iter().all(|c| c.role == SyntaxRole::Plain) {
            continue;
        }

        boundaries.clear();
        for c in &active {
            let s = c.start.max(line_start);
            let e = c.end.min(line_end);
            if s < e {
                boundaries.push(s);
                boundaries.push(e);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        if boundaries.len() < 2 {
            continue;
        }

        let line_spans_out = &mut out[line_idx];
        for pair in boundaries.windows(2) {
            let seg_start = pair[0];
            let seg_end = pair[1];
            if seg_start >= seg_end {
                continue;
            }
            // Winner: narrower capture beats enclosing parent (e.g. escape/interpolation inside string),
            // then higher semantic specificity beats generic fallback (`@variable`),
            // then later query pattern breaks remaining ties.
            let mut best: Option<RawCapture> = None;
            for &c in &active {
                if c.start <= seg_start && c.end >= seg_end {
                    let c_len = c.end - c.start;
                    let better = match best {
                        None => true,
                        Some(b) => {
                            let b_len = b.end - b.start;
                            (std::cmp::Reverse(c_len), c.spec, c.pattern_idx)
                                > (std::cmp::Reverse(b_len), b.spec, b.pattern_idx)
                        }
                    };
                    if better {
                        best = Some(c);
                    }
                }
            }
            if let Some(winner) = best {
                if winner.role != SyntaxRole::Plain {
                    let rel_start = (seg_start - line_start) as u32;
                    let rel_end = (seg_end - line_start) as u32;
                    if let Some(last) = line_spans_out
                        .last_mut()
                        .filter(|s| s.role == winner.role && s.end == rel_start)
                    {
                        last.end = rel_end;
                    } else {
                        line_spans_out.push(TokenSpan {
                            start: rel_start,
                            end: rel_end,
                            role: winner.role,
                        });
                    }
                }
            }
        }
    }

    out
}

#[cfg(feature = "tree-sitter")]
fn extract_diff_header_lang(line: &str) -> Option<SupportedLanguage> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        // Take the destination token (last path, stripping `b/` prefix and optional quotes).
        let raw_path = rest.rsplit_once(' ').map_or(rest, |(_, b)| b);
        let unquoted = raw_path.trim().trim_matches('"');
        let cleaned = unquoted.strip_prefix("b/").unwrap_or(unquoted);
        return SupportedLanguage::from_path(Path::new(cleaned));
    }
    if let Some(rest) = line
        .strip_prefix("+++ ")
        .or_else(|| line.strip_prefix("--- "))
    {
        let raw_path = rest
            .split('\t')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_matches('"');
        if raw_path == "/dev/null" {
            return None;
        }
        let cleaned = raw_path
            .strip_prefix("b/")
            .or_else(|| raw_path.strip_prefix("a/"))
            .unwrap_or(raw_path);
        return SupportedLanguage::from_path(Path::new(cleaned));
    }
    None
}

/// Highlights a unified diff by reconstructing per-file old-side (`context` + `-`) and
/// new-side (`context` + `+`) views and parsing each supported language with Tree-sitter.
#[cfg(feature = "tree-sitter")]
fn highlight_unified_diff(cleaned_diff: &str) -> Option<HighlightedDoc> {
    let raw_lines: Vec<&str> = cleaned_diff.split('\n').collect();
    let mut results: Vec<(DiffLineKind, Vec<TokenSpan>)> = raw_lines
        .iter()
        .map(|l| (classify_diff_line(l), Vec::new()))
        .collect();

    struct FileSection {
        lang: SupportedLanguage,
        old_lines: Vec<(usize, String)>,
        new_lines: Vec<(usize, String)>,
    }

    let mut sections: Vec<FileSection> = Vec::new();
    let mut current_lang: Option<SupportedLanguage> = None;

    for (idx, &raw) in raw_lines.iter().enumerate() {
        let kind = results[idx].0;
        match kind {
            DiffLineKind::FileHeader => {
                if raw.starts_with("diff --git ") {
                    current_lang = extract_diff_header_lang(raw);
                    if let Some(lang) = current_lang {
                        sections.push(FileSection {
                            lang,
                            old_lines: Vec::new(),
                            new_lines: Vec::new(),
                        });
                    }
                } else if let Some(lang) = extract_diff_header_lang(raw) {
                    if current_lang != Some(lang) || sections.is_empty() {
                        current_lang = Some(lang);
                        sections.push(FileSection {
                            lang,
                            old_lines: Vec::new(),
                            new_lines: Vec::new(),
                        });
                    }
                }
            }
            DiffLineKind::Added => {
                if let Some(sec) = sections.last_mut() {
                    let body = raw.strip_prefix('+').unwrap_or(raw);
                    sec.new_lines.push((idx, body.to_owned()));
                }
            }
            DiffLineKind::Removed => {
                if let Some(sec) = sections.last_mut() {
                    let body = raw.strip_prefix('-').unwrap_or(raw);
                    sec.old_lines.push((idx, body.to_owned()));
                }
            }
            DiffLineKind::Context => {
                if let Some(sec) = sections.last_mut() {
                    if let Some(body) = raw.strip_prefix(' ') {
                        sec.old_lines.push((idx, body.to_owned()));
                        sec.new_lines.push((idx, body.to_owned()));
                    }
                }
            }
            DiffLineKind::HunkHeader | DiffLineKind::Meta => {}
        }
    }

    let mut any_highlighted = false;
    for sec in sections {
        if !sec.old_lines.is_empty() {
            let joined = sec
                .old_lines
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(hl) = highlight_code_lines(sec.lang, &joined) {
                for ((orig_idx, _), spans) in sec.old_lines.iter().zip(hl) {
                    if results[*orig_idx].0 == DiffLineKind::Removed && !spans.is_empty() {
                        results[*orig_idx].1 = spans;
                        any_highlighted = true;
                    }
                }
            }
        }
        if !sec.new_lines.is_empty() {
            let joined = sec
                .new_lines
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(hl) = highlight_code_lines(sec.lang, &joined) {
                for ((orig_idx, _), spans) in sec.new_lines.iter().zip(hl) {
                    if matches!(
                        results[*orig_idx].0,
                        DiffLineKind::Added | DiffLineKind::Context
                    ) && !spans.is_empty()
                    {
                        results[*orig_idx].1 = spans;
                        any_highlighted = true;
                    }
                }
            }
        }
    }

    any_highlighted.then_some(HighlightedDoc::Diff { lines: results })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_tags_resolve_coding_languages_and_diff() {
        assert_eq!(
            lang_from_fence_tag("rust,no_run"),
            Some(HighlightLang::Code(SupportedLanguage::Rust))
        );
        assert_eq!(
            lang_from_fence_tag("rs"),
            Some(HighlightLang::Code(SupportedLanguage::Rust))
        );
        assert_eq!(
            lang_from_fence_tag("tsx title=\"App.tsx\""),
            Some(HighlightLang::Code(SupportedLanguage::Tsx))
        );
        assert_eq!(
            lang_from_fence_tag("python3"),
            Some(HighlightLang::Code(SupportedLanguage::Python))
        );
        assert_eq!(
            lang_from_fence_tag("c++"),
            Some(HighlightLang::Code(SupportedLanguage::Cpp))
        );
        assert_eq!(
            lang_from_fence_tag("diff"),
            Some(HighlightLang::UnifiedDiff)
        );
        assert_eq!(
            lang_from_fence_tag("patch"),
            Some(HighlightLang::UnifiedDiff)
        );
        assert_eq!(lang_from_fence_tag("plaintext"), None);
    }

    #[test]
    fn unified_diff_heuristic_distinguishes_diffs_from_plain_logs() {
        assert!(looks_like_unified_diff(
            "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;"
        ));
        assert!(looks_like_unified_diff(
            "--- a/foo.py\n+++ b/foo.py\n@@ -1 +1 @@\n-x = 1\n+x = 2"
        ));
        assert!(!looks_like_unified_diff("running 12 tests\ntest ok\n"));
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn highlights_rust_python_js_ts_and_resolves_overlaps_cleanly() {
        // Rust: keyword, function, type, string + escape, comment
        let rust_src = "pub fn greet(name: &str) -> String {\n    // comment\n    \"hi\\n\"\n}";
        let doc = compute_highlight(HighlightLang::Code(SupportedLanguage::Rust), rust_src)
            .expect("rust highlight");
        let lines = doc.code_lines().unwrap();
        assert_eq!(lines.len(), 4);
        let l0 = line_spans(
            "pub fn greet(name: &str) -> String {",
            &lines[0],
            DiffBg::None,
        );
        assert!(
            l0.iter()
                .any(|(t, f)| *t == "fn" && f.syntax == SyntaxRole::Keyword),
            "fn should be Keyword: {l0:?}"
        );
        assert!(
            l0.iter()
                .any(|(t, f)| *t == "greet" && f.syntax == SyntaxRole::Function),
            "greet should be Function: {l0:?}"
        );
        assert!(
            l0.iter()
                .any(|(t, f)| *t == "String" && f.syntax == SyntaxRole::Type),
            "String should be Type: {l0:?}"
        );
        let l1 = line_spans("    // comment", &lines[1], DiffBg::None);
        assert!(
            l1.iter().any(|(t, f)| t.contains("// comment")
                && f.syntax == SyntaxRole::Comment
                && f.italic),
            "comment should be italic Comment: {l1:?}"
        );
        let l2 = line_spans("    \"hi\\n\"", &lines[2], DiffBg::None);
        assert!(
            l2.iter()
                .any(|(t, f)| *t == "\\n" && f.syntax == SyntaxRole::Constant),
            "escape inside string should override string: {l2:?}"
        );

        // Python: `@function` must beat earlier `@variable` pattern on same byte range!
        let py_src = "def run(items):\n    return len(items)\n";
        let py_doc = compute_highlight(HighlightLang::Code(SupportedLanguage::Python), py_src)
            .expect("python highlight");
        let py_l1 = line_spans(
            "    return len(items)",
            &py_doc.code_lines().unwrap()[1],
            DiffBg::None,
        );
        assert!(
            py_l1
                .iter()
                .any(|(t, f)| *t == "return" && f.syntax == SyntaxRole::Keyword),
            "{py_l1:?}"
        );
        assert!(
            py_l1
                .iter()
                .any(|(t, f)| *t == "len" && f.syntax == SyntaxRole::Function),
            "Python builtin/function call must beat @variable: {py_l1:?}"
        );

        // Go: `@function` must beat later `@variable` pattern on same byte range!
        let go_src = "package main\nfunc add(a int) int { return len(\"x\") }\n";
        let go_doc = compute_highlight(HighlightLang::Code(SupportedLanguage::Go), go_src)
            .expect("go highlight");
        let go_l1 = line_spans(
            "func add(a int) int { return len(\"x\") }",
            &go_doc.code_lines().unwrap()[1],
            DiffBg::None,
        );
        assert!(
            go_l1
                .iter()
                .any(|(t, f)| *t == "add" && f.syntax == SyntaxRole::Function),
            "Go function must beat @variable: {go_l1:?}"
        );
        assert!(
            go_l1
                .iter()
                .any(|(t, f)| *t == "len" && f.syntax == SyntaxRole::Function),
            "Go call must beat @variable: {go_l1:?}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn highlights_unified_diff_per_file_and_preserves_exact_text() {
        let diff = concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,3 +1,3 @@ fn demo()\n",
            " pub fn demo() {\n",
            "-    let old = 1;\n",
            "+    let new = \"two\";\n",
            " }"
        );
        let doc = compute_highlight(HighlightLang::UnifiedDiff, diff).expect("diff highlight");
        let mut rendered = Vec::new();
        push_unified_diff(&mut rendered, diff, Some(&doc), 80);
        let plain: String = rendered
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(plain, diff);

        // Removed line carries DiffBg::Removed + Keyword `let`.
        assert!(
            rendered[5].iter().any(|s| s.text == "let"
                && s.face.syntax == SyntaxRole::Keyword
                && s.face.diff_bg == DiffBg::Removed),
            "removed line must carry syntax fg + removed bg: {:?}",
            rendered[5]
        );
        // Added line carries DiffBg::Added + String `"two"`.
        assert!(
            rendered[6].iter().any(|s| s.text.contains("two")
                && s.face.syntax == SyntaxRole::String
                && s.face.diff_bg == DiffBg::Added),
            "added line must carry syntax fg + added bg: {:?}",
            rendered[6]
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn async_worker_returns_pending_immediately_then_populates_cache_in_parallel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cache = HighlightCache::default();
        cache.attach_worker(HighlightWorker::spawn(tx));

        let src_a = "pub fn alpha() -> i32 { 42 }";
        let src_b = "def beta():\n    return 'ok'\n";
        assert!(matches!(
            cache.lookup_or_schedule(HighlightLang::Code(SupportedLanguage::Rust), src_a),
            Lookup::Pending
        ));
        // Duplicate request stays deduplicated while in flight.
        assert!(matches!(
            cache.lookup_or_schedule(HighlightLang::Code(SupportedLanguage::Rust), src_a),
            Lookup::Pending
        ));
        assert!(matches!(
            cache.lookup_or_schedule(HighlightLang::Code(SupportedLanguage::Python), src_b),
            Lookup::Pending
        ));

        let mut received = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while received < 2 && std::time::Instant::now() < deadline {
            if let Ok(Update::HighlightReady { key, doc }) = rx.try_recv() {
                assert!(cache.apply_result(key, doc));
                received += 1;
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(received, 2, "both parallel highlight jobs must complete");
        assert!(matches!(
            cache.lookup_or_schedule(HighlightLang::Code(SupportedLanguage::Rust), src_a),
            Lookup::Ready(_)
        ));
        assert!(matches!(
            cache.lookup_or_schedule(HighlightLang::Code(SupportedLanguage::Python), src_b),
            Lookup::Ready(_)
        ));
    }
}
