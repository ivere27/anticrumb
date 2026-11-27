//! Bounded transcript buffer, entry styling, and plain-text projection.
use super::{
    highlight::HighlightCache,
    inspector::{self, BlockId, BlockRowKind, ToolBlockStore},
    layout::{AfterTurnPolicy, HorizontalSplit},
    markdown::{self, Face},
    reducer::PendingToolApproval,
};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use unicode_width::UnicodeWidthChar;

pub(super) fn clip(text: &str, width: usize) -> String {
    let cleaned = markdown::clean(text);
    let mut used = 0;
    markdown::strip_controls(&cleaned, false)
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

pub(super) fn inline_notice_parts(text: &str, base: Face) -> Vec<(String, Face)> {
    let accent_cmd = Face::ACCENT_BOLD;
    let key_badge = Face::HIGHLIGHT_BOLD;
    const COMMAND_PREFIXES: &[&str] = &[
        "/browser clear-history",
        "/browser refresh",
        "/browser status",
        "/browser login",
        "/browser show",
        "/browser hide",
        "/model refresh",
        "/permissions",
        "/inspector",
        "/debug trace",
        "/debug info",
        "/debug off",
        "/browser",
        "/provider",
        "/status",
        "/resume",
        "/resume clear-history",
        "/usage",
        "/model",
        "/theme",
        "/debug",
        "/clear",
        "/help",
        "/copy",
        "/exit",
        "/quit",
        "/new",
    ];
    const KEY_BADGES: &[&str] = &[
        "Shift+Enter",
        "Alt+Enter",
        "Alt+PgUp",
        "Alt+PgDn",
        "Alt+↑",
        "Alt+↓",
        "Alt+I",
        "Alt+L",
        "Alt+E",
        "Ctrl+O",
        "Ctrl+C",
        "Ctrl+D",
        "Ctrl+V",
        "DELETE",
        "Enter",
        "Tab",
        "Esc",
    ];
    let boundary_before = |pos: usize| {
        pos == 0
            || matches!(
                text[..pos].chars().next_back(),
                Some(' ' | '\t' | '(' | '[' | '"' | '·' | ':')
            )
    };
    let boundary_after = |end: usize| {
        end == text.len()
            || matches!(
                text[end..].chars().next(),
                Some(' ' | '\t' | '.' | ',' | ':' | ';' | ')' | ']' | '"' | '!' | '?' | '·' | '/')
            )
    };
    let mut parts: Vec<(String, Face)> = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if let Some(after_tick) = rest.strip_prefix('`') {
            if let Some(close) = after_tick.find('`') {
                if close > 0 {
                    if i > plain_start {
                        parts.push((text[plain_start..i].into(), base));
                    }
                    parts.push((after_tick[..close].into(), accent_cmd));
                    i += 1 + close + 1;
                    plain_start = i;
                    continue;
                }
            }
        }
        if rest.starts_with('/') && boundary_before(i) {
            if let Some(cmd) = COMMAND_PREFIXES
                .iter()
                .find(|&&c| rest.starts_with(c) && boundary_after(i + c.len()))
            {
                if i > plain_start {
                    parts.push((text[plain_start..i].into(), base));
                }
                parts.push(((*cmd).into(), accent_cmd));
                i += cmd.len();
                plain_start = i;
                continue;
            }
        }
        if boundary_before(i) {
            if let Some(badge) = KEY_BADGES
                .iter()
                .find(|&&k| rest.starts_with(k) && boundary_after(i + k.len()))
            {
                if i > plain_start {
                    parts.push((text[plain_start..i].into(), base));
                }
                parts.push(((*badge).into(), key_badge));
                i += badge.len();
                plain_start = i;
                continue;
            }
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    if plain_start < text.len() {
        parts.push((text[plain_start..].into(), base));
    }
    parts
}

pub(super) enum Entry {
    Markdown(String),
    Prompt(String),
    Heading(String),
    Command {
        key: String,
        description: String,
        highlight_key: bool,
    },
    Notice(String),
    Spans(Vec<(String, Face)>),
    Error(String),
    ToolBlock(BlockId),
    StatusRow {
        primary: bool,
        text: String,
    },
}

impl Entry {
    pub(super) fn byte_len(&self) -> usize {
        match self {
            Entry::Markdown(s)
            | Entry::Prompt(s)
            | Entry::Heading(s)
            | Entry::Notice(s)
            | Entry::Error(s)
            | Entry::StatusRow { text: s, .. } => s.len(),
            Entry::Spans(parts) => parts.iter().map(|(s, _)| s.len()).sum(),
            Entry::Command {
                key, description, ..
            } => key.len() + description.len(),
            // Count only lightweight summary overhead so heavy tool outputs never
            // prematurely evict conversation prose from the 128 KiB transcript budget.
            Entry::ToolBlock(_) => 64,
        }
    }

    pub(super) fn render_with_highlights(
        &self,
        width: usize,
        highlights: &HighlightCache,
    ) -> (Vec<markdown::Line>, bool) {
        match self {
            Entry::Markdown(text) => {
                markdown::lines_with_highlights(text, width, Some(highlights), true)
            }
            other => (other.render(width), false),
        }
    }

    pub(super) fn render(&self, width: usize) -> Vec<markdown::Line> {
        match self {
            Entry::Markdown(text) => markdown::lines(text, width),
            Entry::Prompt(text) => {
                let mut lines = Vec::new();
                let body = if let Some(rest) = text.strip_prefix('\n') {
                    lines.push(Vec::new());
                    rest
                } else {
                    text.as_str()
                };
                if width > 0 {
                    lines.extend(markdown::literal(&"─".repeat(width), Face::SUBTLE, width));
                }
                let mut is_first_visual_row = true;
                for raw_line in body.split('\n') {
                    let raw_line = raw_line.trim_end_matches('\r');
                    let mut parts: Vec<(&str, Face)> = Vec::new();
                    let mut plain_start = 0;
                    let mut i = 0;
                    while i < raw_line.len() {
                        let rest = &raw_line[i..];
                        if rest.starts_with('@')
                            && crate::input::is_mention_boundary(raw_line[..i].chars().next_back())
                        {
                            if let Ok(Some((_, len))) = crate::input::parse_mention(rest) {
                                if len > 1 {
                                    if i > plain_start {
                                        parts.push((&raw_line[plain_start..i], Face::BOLD));
                                    }
                                    parts.push((&raw_line[i..i + len], Face::ACCENT));
                                    i += len;
                                    plain_start = i;
                                    continue;
                                }
                            }
                        }
                        i += rest.chars().next().map_or(1, char::len_utf8);
                    }
                    if plain_start < raw_line.len() {
                        parts.push((&raw_line[plain_start..], Face::BOLD));
                    }
                    if width <= 2 {
                        if is_first_visual_row {
                            parts.insert(0, ("> ", Face::ACCENT_BOLD));
                        }
                        lines.extend(markdown::spans(&parts, width));
                        is_first_visual_row = false;
                        continue;
                    }
                    let body_width = width - 2;
                    for mut wrapped_line in markdown::spans(&parts, body_width) {
                        if is_first_visual_row {
                            wrapped_line.insert(
                                0,
                                markdown::Span {
                                    text: "> ".into(),
                                    face: Face::ACCENT_BOLD,
                                },
                            );
                            is_first_visual_row = false;
                        } else if !wrapped_line.is_empty() {
                            wrapped_line.insert(
                                0,
                                markdown::Span {
                                    text: "  ".into(),
                                    face: Face::TEXT,
                                },
                            );
                        }
                        lines.push(wrapped_line);
                    }
                }
                lines
            }
            Entry::Heading(text) => {
                let mut lines = vec![Vec::new()];
                lines.extend(markdown::literal(text, Face::ACCENT_BOLD, width));
                lines
            }
            Entry::Command {
                key,
                description,
                highlight_key,
            } => {
                let key_face = if *highlight_key {
                    Face::HIGHLIGHT_BOLD
                } else {
                    Face::ACCENT_BOLD
                };
                let mut parts = vec![(key.clone(), key_face)];
                parts.extend(inline_notice_parts(description, Face::MUTED));
                let borrowed: Vec<(&str, Face)> =
                    parts.iter().map(|(s, f)| (s.as_str(), *f)).collect();
                markdown::spans(&borrowed, width)
            }
            Entry::Notice(text) => {
                let mut lines = Vec::new();
                let mut trimmed = text.as_str();
                while let Some(rest) = trimmed.strip_prefix('\n') {
                    lines.push(Vec::new());
                    trimmed = rest;
                }
                for (index, raw_line) in trimmed.split('\n').enumerate() {
                    let mut parts = Vec::new();
                    if raw_line.starts_with("  ") {
                        parts.extend(inline_notice_parts(raw_line, Face::MUTED));
                    } else if index == 0 {
                        parts.push(("· ".into(), Face::ACCENT));
                        parts.extend(inline_notice_parts(raw_line, Face::MUTED));
                    } else {
                        parts.push(("  ".into(), Face::ACCENT));
                        parts.extend(inline_notice_parts(raw_line, Face::MUTED));
                    }
                    let borrowed: Vec<(&str, Face)> =
                        parts.iter().map(|(s, f)| (s.as_str(), *f)).collect();
                    lines.extend(markdown::spans(&borrowed, width));
                }
                lines
            }
            Entry::Spans(parts) => {
                let mut lines = Vec::new();
                let mut current: Vec<(&str, Face)> = Vec::new();
                for (segment, face) in parts {
                    let mut pieces = segment.split('\n');
                    if let Some(first) = pieces.next() {
                        if !first.is_empty() {
                            current.push((first, *face));
                        }
                    }
                    for next in pieces {
                        lines.extend(markdown::spans(&current, width));
                        current.clear();
                        if !next.is_empty() {
                            current.push((next, *face));
                        }
                    }
                }
                if !current.is_empty() || lines.is_empty() {
                    lines.extend(markdown::spans(&current, width));
                }
                lines
            }
            Entry::Error(text) => {
                let (prefix, body, body_face) = if let Some(body) = text.strip_prefix("! ") {
                    ("! ", body, Face::DANGER)
                } else if let Some(body) = text.strip_prefix("    ! ") {
                    ("    ! ", body, Face::MUTED)
                } else {
                    ("! ", text.as_str(), Face::DANGER)
                };
                let mut parts = vec![(prefix.to_owned(), Face::DANGER_BOLD)];
                parts.extend(inline_notice_parts(body, body_face));
                let borrowed: Vec<(&str, Face)> =
                    parts.iter().map(|(s, f)| (s.as_str(), *f)).collect();
                markdown::spans(&borrowed, width)
            }
            Entry::ToolBlock(_) => Vec::new(),
            Entry::StatusRow { primary, text } => {
                if *primary {
                    markdown::prefixed("· ", Face::ACCENT, text, Face::TEXT, width)
                } else {
                    markdown::prefixed("  ", Face::MUTED, text, Face::MUTED, width)
                }
            }
        }
    }
}

struct TranscriptItem {
    plain: String,
    entry: Entry,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SplitCacheKey {
    pub block_left_width: usize,
    pub split_active: bool,
    pub inline_split_left: u16,
    pub inline_split_right: u16,
    pub after_turn: AfterTurnPolicy,
    pub min_block_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BlockSpan {
    pub block_id: BlockId,
    pub start_line: usize,
    pub end_line: usize,
    pub row_kinds: Vec<BlockRowKind>,
    pub has_bottom_border: bool,
}

/// Wrapped transcript lines shared between the render cache and the last
/// painted frame. Cloning is O(1); only selection start copies the lines.
#[derive(Clone, Default)]
pub(super) struct TranscriptLines {
    items: Rc<Vec<markdown::Line>>,
    active_tail: Rc<Vec<markdown::Line>>,
    stream: Rc<Vec<markdown::Line>>,
    block_spans: Rc<Vec<BlockSpan>>,
    active_tail_span: Option<BlockSpan>,
    visible_stream_len: usize,
    evicted_lines: u64,
}

impl TranscriptLines {
    pub(super) fn len(&self) -> usize {
        self.items.len() + self.active_tail.len() + self.stream.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(super) fn visible_stream_len(&self) -> usize {
        self.visible_stream_len
    }

    pub(super) fn evicted_lines(&self) -> u64 {
        self.evicted_lines
    }

    pub(super) fn block_spans(&self) -> impl Iterator<Item = &BlockSpan> {
        self.block_spans.iter().chain(self.active_tail_span.iter())
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Copy `start..end` (clamped) without materializing the whole transcript.
    pub(super) fn slice(&self, start: usize, end: usize) -> Vec<markdown::Line> {
        let end = end.min(self.len());
        let start = start.min(end);
        let items_end = self.items.len();
        let tail_end = items_end + self.active_tail.len();
        let mut out = Vec::with_capacity(end - start);
        if start < items_end {
            out.extend_from_slice(&self.items[start..end.min(items_end)]);
        }
        if end > items_end && start < tail_end {
            let s = start.max(items_end) - items_end;
            let e = end.min(tail_end) - items_end;
            out.extend_from_slice(&self.active_tail[s..e]);
        }
        if end > tail_end {
            let s = start.max(tail_end) - tail_end;
            let e = end - tail_end;
            out.extend_from_slice(&self.stream[s..e]);
        }
        out
    }

    pub(super) fn to_vec(&self) -> Vec<markdown::Line> {
        self.slice(0, self.len())
    }
}

/// Incremental wrap cache. Sealed items are immutable after `push`, so only newly
/// appended items are rendered; width, split-layout, or `block_layout_rev` changes
/// invalidate, while active tool output chunks and selection marker moves leave the
/// sealed `items` cache untouched.
#[derive(Default)]
struct LineCache {
    width: usize,
    split_key: SplitCacheKey,
    block_layout_rev: u64,
    highlight_rev: u64,
    rendered_items: usize,
    item_line_counts: Vec<usize>,
    pending_highlight_items: Vec<usize>,
    items: Rc<Vec<markdown::Line>>,
    block_spans: Rc<Vec<BlockSpan>>,
    evicted_lines: u64,
    stream_text: String,
    stream: Rc<Vec<markdown::Line>>,
    stream_pending_highlight: bool,
    stream_highlight_rev: u64,
    visible_stream_len: usize,
}

impl LineCache {
    fn reset(
        &mut self,
        width: usize,
        split_key: SplitCacheKey,
        block_layout_rev: u64,
        highlight_rev: u64,
    ) {
        *self = Self {
            width,
            split_key,
            block_layout_rev,
            highlight_rev,
            ..Self::default()
        };
    }
}

#[derive(Default)]
pub(super) struct Transcript {
    items: Vec<TranscriptItem>,
    bytes: usize,
    revision: u64,
    cache: RefCell<LineCache>,
}

impl Transcript {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    /// Push a transcript entry and return any evicted `ToolBlock` IDs so
    /// `ToolBlockStore` can drop their backing state immediately.
    pub(super) fn push(&mut self, plain: &str, entry: Entry, max_bytes: usize) -> Vec<BlockId> {
        let keep_bytes = (max_bytes * 3) / 4;
        self.revision = self.revision.wrapping_add(1);
        let accounted_len = match &entry {
            Entry::ToolBlock(_) => entry.byte_len(),
            _ => entry.byte_len().max(plain.len()),
        };
        self.bytes += accounted_len + 1;
        self.items.push(TranscriptItem {
            plain: plain.to_owned(),
            entry,
        });
        let mut evicted_blocks = Vec::new();
        if self.bytes > max_bytes {
            let mut drained = 0;
            while drained < self.items.len() && self.bytes > keep_bytes {
                let item = &self.items[drained];
                let item_len = match &item.entry {
                    Entry::ToolBlock(_) => item.entry.byte_len(),
                    _ => item.entry.byte_len().max(item.plain.len()),
                };
                self.bytes = self.bytes.saturating_sub(item_len + 1);
                drained += 1;
            }
            if drained > 0 {
                for removed in self.items.drain(..drained) {
                    if let Entry::ToolBlock(id) = removed.entry {
                        evicted_blocks.push(id);
                    }
                }
                let cache = self.cache.get_mut();
                let drained_rendered = drained.min(cache.rendered_items);
                if drained_rendered > 0 {
                    let drained_lines: usize =
                        cache.item_line_counts.drain(..drained_rendered).sum();
                    cache.evicted_lines = cache.evicted_lines.saturating_add(drained_lines as u64);
                    cache.rendered_items -= drained_rendered;
                    cache.pending_highlight_items.retain_mut(|idx| {
                        if *idx < drained_rendered {
                            false
                        } else {
                            *idx -= drained_rendered;
                            true
                        }
                    });
                    if cache.rendered_items == 0 {
                        cache.items = Rc::new(Vec::new());
                        cache.block_spans = Rc::new(Vec::new());
                    } else {
                        let cached_items = Rc::make_mut(&mut cache.items);
                        let drop_lines = drained_lines.min(cached_items.len());
                        cached_items.drain(..drop_lines);
                        let spans = Rc::make_mut(&mut cache.block_spans);
                        spans.retain_mut(|span| {
                            if span.end_line <= drop_lines {
                                false
                            } else {
                                span.start_line = span.start_line.saturating_sub(drop_lines);
                                span.end_line = span.end_line.saturating_sub(drop_lines);
                                true
                            }
                        });
                    }
                }
            }
        }
        evicted_blocks
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
        self.revision = self.revision.wrapping_add(1);
        let cache = self.cache.get_mut();
        let (width, split_key, rev, hl_rev) = (
            cache.width,
            cache.split_key,
            cache.block_layout_rev,
            cache.highlight_rev,
        );
        cache.reset(width, split_key, rev, hl_rev);
    }

    /// Wrapped lines for `width`, reusing previously wrapped sealed items and
    /// appending any active `ToolBlock` tail before `stream`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn lines_with_blocks(
        &self,
        stream: &str,
        width: usize,
        tool_blocks: &ToolBlockStore,
        pending_approvals: &VecDeque<PendingToolApproval>,
        split_key: SplitCacheKey,
        inline_split: Option<HorizontalSplit>,
        viewport: usize,
        spinner: &str,
        highlights: &HighlightCache,
    ) -> TranscriptLines {
        let w = width.saturating_sub(1);
        let block_layout_rev = tool_blocks.layout_rev();
        let hl_rev = highlights.rev();
        let mut cache_ref = self.cache.borrow_mut();
        let cache = &mut *cache_ref;
        if cache.width != w
            || cache.split_key != split_key
            || cache.block_layout_rev != block_layout_rev
        {
            cache.reset(w, split_key, block_layout_rev, hl_rev);
        }
        // Refresh only sealed items that were waiting on async syntax highlights.
        if cache.highlight_rev != hl_rev {
            cache.highlight_rev = hl_rev;
            if !cache.pending_highlight_items.is_empty() {
                let pending = std::mem::take(&mut cache.pending_highlight_items);
                let items = Rc::make_mut(&mut cache.items);
                let spans = Rc::make_mut(&mut cache.block_spans);
                let mut still_pending = Vec::new();
                for idx in pending {
                    if idx >= cache.rendered_items || idx >= self.items.len() {
                        continue;
                    }
                    let start_line: usize = cache.item_line_counts[..idx].iter().sum();
                    let old_len = cache.item_line_counts[idx];
                    let (new_lines, needs_more) =
                        self.items[idx].entry.render_with_highlights(w, highlights);
                    let new_len = new_lines.len();
                    if start_line + old_len <= items.len() {
                        items.splice(start_line..start_line + old_len, new_lines);
                        cache.item_line_counts[idx] = new_len;
                        if new_len != old_len {
                            let end_old = start_line + old_len;
                            for span in spans.iter_mut() {
                                if span.start_line >= end_old {
                                    span.start_line = span.start_line + new_len - old_len;
                                    span.end_line = span.end_line + new_len - old_len;
                                }
                            }
                        }
                    }
                    if needs_more {
                        still_pending.push(idx);
                    }
                }
                cache.pending_highlight_items = still_pending;
            }
        }
        if cache.rendered_items < self.items.len() {
            let start = cache.rendered_items;
            let items = Rc::make_mut(&mut cache.items);
            let spans = Rc::make_mut(&mut cache.block_spans);
            let empty_approvals = VecDeque::new();
            for (offset, item) in self.items[start..].iter().enumerate() {
                let item_idx = start + offset;
                let before = items.len();
                match &item.entry {
                    Entry::ToolBlock(block_id) => {
                        if let Some(block) = tool_blocks.get(*block_id) {
                            let rendered = inspector::render_block_left(
                                block,
                                split_key.block_left_width.max(1),
                                inline_split,
                                split_key.split_active,
                                &empty_approvals,
                                split_key.after_turn,
                                viewport,
                                None,
                                spinner,
                                highlights,
                            );
                            items.extend(rendered.lines);
                            let after = items.len();
                            if after > before {
                                spans.push(BlockSpan {
                                    block_id: *block_id,
                                    start_line: before,
                                    end_line: after,
                                    row_kinds: rendered.row_kinds,
                                    has_bottom_border: rendered.has_bottom_border,
                                });
                            }
                        }
                    }
                    other => {
                        let (rendered_lines, needs_hl) =
                            other.render_with_highlights(w, highlights);
                        items.extend(rendered_lines);
                        if needs_hl {
                            cache.pending_highlight_items.push(item_idx);
                        }
                    }
                }
                cache.item_line_counts.push(items.len() - before);
            }
            cache.rendered_items = self.items.len();
        }
        let stream_hl_dirty =
            cache.stream_pending_highlight && cache.stream_highlight_rev != hl_rev;
        if cache.stream_text != stream || stream_hl_dirty {
            if stream.is_empty() {
                cache.stream = Rc::new(Vec::new());
                cache.visible_stream_len = 0;
                cache.stream_pending_highlight = false;
            } else {
                let prose = crate::tool_codec::visible_prose(stream);
                cache.visible_stream_len = prose.len();
                let (rendered_stream, needs_hl) =
                    markdown::lines_with_highlights(&prose, w, Some(highlights), false);
                cache.stream = Rc::new(rendered_stream);
                cache.stream_pending_highlight = needs_hl;
            }
            cache.stream_highlight_rev = hl_rev;
            cache.stream_text.clear();
            cache.stream_text.push_str(stream);
        }

        let (active_tail, active_tail_span) = if let Some(active) = tool_blocks.active_block() {
            if active.items.is_empty() {
                (Rc::new(Vec::new()), None)
            } else {
                let active_min = inspector::active_min_block_rows(
                    viewport,
                    cache.items.len(),
                    cache.stream.len(),
                );
                let rendered = inspector::render_block_left(
                    active,
                    split_key.block_left_width.max(1),
                    inline_split,
                    split_key.split_active,
                    pending_approvals,
                    split_key.after_turn,
                    viewport,
                    Some(active_min),
                    spinner,
                    highlights,
                );
                let tail_start = cache.items.len();
                let tail_end = tail_start + rendered.lines.len();
                let span = BlockSpan {
                    block_id: active.id,
                    start_line: tail_start,
                    end_line: tail_end,
                    row_kinds: rendered.row_kinds,
                    has_bottom_border: rendered.has_bottom_border,
                };
                (Rc::new(rendered.lines), Some(span))
            }
        } else {
            (Rc::new(Vec::new()), None)
        };

        TranscriptLines {
            items: Rc::clone(&cache.items),
            active_tail,
            stream: Rc::clone(&cache.stream),
            block_spans: Rc::clone(&cache.block_spans),
            active_tail_span,
            visible_stream_len: cache.visible_stream_len,
            evicted_lines: cache.evicted_lines,
        }
    }

    #[cfg(test)]
    pub(super) fn lines(&self, stream: &str, width: usize) -> TranscriptLines {
        let empty_store = ToolBlockStore::default();
        let empty_approvals = VecDeque::new();
        let empty_hl = HighlightCache::default();
        self.lines_with_blocks(
            stream,
            width,
            &empty_store,
            &empty_approvals,
            SplitCacheKey::default(),
            None,
            24,
            "⠋",
            &empty_hl,
        )
    }

    #[cfg(test)]
    pub(super) fn render_lines(&self, stream: &str, width: usize) -> Vec<markdown::Line> {
        self.lines(stream, width).to_vec()
    }

    #[cfg(test)]
    pub(super) fn plain_text(&self) -> String {
        let mut out = String::with_capacity(self.bytes);
        for item in &self.items {
            out.push_str(&item.plain);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Reference: wrap every item from scratch, as before the cache existed.
    fn uncached(
        t: &Transcript,
        stream: &str,
        width: usize,
        blocks: &ToolBlockStore,
    ) -> Vec<markdown::Line> {
        let w = width.saturating_sub(1);
        let empty_approvals = VecDeque::new();
        let empty_hl = HighlightCache::default();
        let mut lines = Vec::new();
        for item in &t.items {
            match &item.entry {
                Entry::ToolBlock(id) => {
                    if let Some(block) = blocks.get(*id) {
                        lines.extend(
                            inspector::render_block_left(
                                block,
                                w,
                                None,
                                false,
                                &empty_approvals,
                                AfterTurnPolicy::Fit,
                                24,
                                None,
                                "⠋",
                                &empty_hl,
                            )
                            .lines,
                        );
                    }
                }
                other => lines.extend(other.render(w)),
            }
        }
        if !stream.is_empty() {
            lines.extend(markdown::lines(
                &crate::tool_codec::visible_prose(stream),
                w,
            ));
        }
        lines
    }

    fn push_md(t: &mut Transcript, text: &str, max: usize) {
        t.push(text, Entry::Markdown(text.into()), max);
    }

    #[test]
    fn cached_lines_match_full_render_across_pushes_width_stream_and_clear() {
        let mut t = Transcript::default();
        let mut blocks = ToolBlockStore::default();
        let empty_approvals = VecDeque::new();
        let empty_hl = HighlightCache::default();
        let max = 1 << 20;
        let render = |t: &Transcript, stream: &str, w: usize, blocks: &ToolBlockStore| {
            let split_key = SplitCacheKey {
                block_left_width: w.saturating_sub(1),
                ..SplitCacheKey::default()
            };
            t.lines_with_blocks(
                stream,
                w,
                blocks,
                &empty_approvals,
                split_key,
                None,
                24,
                "⠋",
                &empty_hl,
            )
        };

        push_md(
            &mut t,
            "first **bold** paragraph that wraps at narrow widths",
            max,
        );
        assert_eq!(
            render(&t, "", 40, &blocks).to_vec(),
            uncached(&t, "", 40, &blocks)
        );

        // Incremental append while the previous snapshot is still held.
        let held = render(&t, "", 40, &blocks);
        blocks.on_tool_started(1, 0, "run_shell".into(), json!({"command": "ls -la"}), true);
        blocks.on_tool_finished(1, 0, "run_shell", true, "file.rs".into(), 5);
        let (bid, plain) = blocks
            .seal_active_block(AfterTurnPolicy::Fit, false)
            .unwrap();
        t.push(&plain, Entry::ToolBlock(bid), max);
        push_md(&mut t, "second", max);
        let full = uncached(&t, "", 40, &blocks);
        assert_eq!(render(&t, "", 40, &blocks).to_vec(), full);
        assert_eq!(held.len(), full.len() - 3);

        // Streaming tail changes without re-wrapping items.
        assert_eq!(
            render(&t, "stream *one*", 40, &blocks).to_vec(),
            uncached(&t, "stream *one*", 40, &blocks)
        );
        assert_eq!(
            render(&t, "stream *one* two", 40, &blocks).to_vec(),
            uncached(&t, "stream *one* two", 40, &blocks)
        );

        // Width change invalidates.
        assert_eq!(
            render(&t, "tail", 17, &blocks).to_vec(),
            uncached(&t, "tail", 17, &blocks)
        );

        t.clear();
        blocks.clear();
        assert!(render(&t, "", 17, &blocks).is_empty());
        push_md(&mut t, "after clear", max);
        assert_eq!(
            render(&t, "", 17, &blocks).to_vec(),
            uncached(&t, "", 17, &blocks)
        );
    }

    #[test]
    fn cached_lines_follow_overflow_trimming_and_return_evicted_block_ids() {
        let mut t = Transcript::default();
        let blocks = ToolBlockStore::default();
        let max = 200;
        let evicted_first = t.push("[tools:1] read_file", Entry::ToolBlock(BlockId(99)), max);
        assert!(evicted_first.is_empty());
        let mut saw_evicted_99 = false;
        let mut total_pushed_lines = 0u64;
        for i in 0..40 {
            let text = format!("entry number {i:02} with some text");
            total_pushed_lines += markdown::lines(&text, 29).len() as u64;
            let held = t.lines("", 30);
            let held_copy = held.to_vec();
            let evicted = t.push(&text, Entry::Markdown(text.clone()), max);
            if evicted.contains(&BlockId(99)) {
                saw_evicted_99 = true;
            }
            // Overflow trimming must not mutate an already-held snapshot.
            assert_eq!(held.to_vec(), held_copy, "held snapshot after push {i}");
            drop(held);
            let lines = t.lines("", 30);
            assert_eq!(
                lines.to_vec(),
                uncached(&t, "", 30, &blocks),
                "after push {i}"
            );
            assert_eq!(
                lines.evicted_lines() + lines.len() as u64,
                total_pushed_lines,
                "virtual line count conserved after push {i}"
            );
        }
        assert!(saw_evicted_99, "overflow must report evicted ToolBlock IDs");
        assert!(t.bytes <= max);
        assert!(t.lines("", 30).evicted_lines() > 0);
        // Width change resets evicted_lines cleanly for the new width.
        assert_eq!(t.lines("", 50).evicted_lines(), 0);
        assert_eq!(t.render_lines("", 50), uncached(&t, "", 50, &blocks));
    }

    #[test]
    fn slice_spans_items_and_stream() {
        let mut t = Transcript::default();
        let blocks = ToolBlockStore::default();
        for i in 0..5 {
            push_md(&mut t, &format!("line {i}"), 1 << 20);
        }
        let stream = "s1\n\ns2\n\ns3";
        let lines = t.lines(stream, 80);
        let full = uncached(&t, stream, 80, &blocks);
        assert_eq!(lines.len(), full.len());
        for start in 0..=full.len() {
            for end in start..=full.len() + 2 {
                assert_eq!(
                    lines.slice(start, end),
                    full[start.min(full.len())..end.min(full.len())].to_vec(),
                    "{start}..{end}"
                );
            }
        }
    }

    #[test]
    fn visible_stream_len_tracks_cached_stream_changes_and_resets() {
        let mut t = Transcript::default();
        assert_eq!(t.lines("", 80).visible_stream_len(), 0);
        let prose = "Working\n";
        let with_hidden_tool = "Working\n[TOOL_CALL]\n{\"tool\":\"read\"}";
        assert_eq!(
            t.lines(with_hidden_tool, 80).visible_stream_len(),
            prose.len()
        );
        // Repeated lookup at same width reuses cached visible_stream_len.
        assert_eq!(
            t.lines(with_hidden_tool, 80).visible_stream_len(),
            prose.len()
        );
        // Width change resets cache and recomputes visible_stream_len accurately.
        assert_eq!(
            t.lines(with_hidden_tool, 40).visible_stream_len(),
            prose.len()
        );
        // Simultaneous width change + empty stream must reset visible_stream_len to 0.
        assert_eq!(t.lines("", 60).visible_stream_len(), 0);
        // Clear while a non-empty stream is cached must reset visible_stream_len to 0.
        assert_eq!(t.lines(prose, 60).visible_stream_len(), prose.len());
        t.clear();
        assert_eq!(t.lines("", 60).visible_stream_len(), 0);
        assert_eq!(t.lines("", 60).evicted_lines(), 0);
    }

    #[test]
    fn clip_strips_ansi_sequences_and_respects_cjk_width() {
        let ansi = "\u{1b}[31m한글\u{1b}[0mabc";
        // 한(2) + 글(2) = 4, a(1) = 5
        assert_eq!(clip(ansi, 5), "한글a");
        assert_eq!(clip(ansi, 3), "한");
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn markdown_fenced_code_and_diff_upgrade_asynchronously_without_reflow() {
        use super::super::{
            highlight::HighlightWorker,
            markdown::{DiffBg, SyntaxRole},
            worker::Update,
        };
        use std::time::{Duration, Instant};

        let plain_of = |lines: &[markdown::Line]| -> String {
            lines
                .iter()
                .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut hl = HighlightCache::default();
        hl.attach_worker(HighlightWorker::spawn(tx));

        let md = concat!(
            "Summary:\n",
            "```rust\n",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            "```\n",
            "```diff\n",
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "@@ -1 +1 @@\n",
            "-let x = 1;\n",
            "+let x = \"hi\";\n",
            "```"
        );
        let mut t = Transcript::default();
        t.push(md, Entry::Markdown(md.into()), 1 << 20);
        let blocks = ToolBlockStore::default();
        let empty_approvals = VecDeque::new();

        // First render: plain text immediately (zero blocking), schedules both fences in parallel.
        let first = t
            .lines_with_blocks(
                "",
                44,
                &blocks,
                &empty_approvals,
                SplitCacheKey::default(),
                None,
                24,
                "⠋",
                &hl,
            )
            .to_vec();
        let first_text = plain_of(&first);
        assert!(
            first
                .iter()
                .flatten()
                .all(|s| s.face.syntax == SyntaxRole::None),
            "first render must not block for syntax tokens"
        );

        // Wait for both background jobs (rust + diff) to finish.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && hl.rev() < 2 {
            while let Ok(Update::HighlightReady { key, doc }) = rx.try_recv() {
                hl.apply_result(key, doc);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(hl.rev() >= 2);

        // Second render: splices highlighted lines in place with identical line count & text!
        let second = t
            .lines_with_blocks(
                "",
                44,
                &blocks,
                &empty_approvals,
                SplitCacheKey::default(),
                None,
                24,
                "⠋",
                &hl,
            )
            .to_vec();
        assert_eq!(second.len(), first.len(), "no reflow on highlight landing");
        assert_eq!(plain_of(&second), first_text, "exact text preserved");
        assert!(second
            .iter()
            .flatten()
            .any(|s| s.text == "fn" && s.face.syntax == SyntaxRole::Keyword));
        assert!(second.iter().flatten().any(|s| s.text.contains("hi")
            && s.face.syntax == SyntaxRole::String
            && s.face.diff_bg == DiffBg::Added));
    }
}
