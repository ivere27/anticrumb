//! Bounded transcript buffer, entry styling, and plain-text projection.
use super::markdown::{self, Face};
use std::{cell::RefCell, rc::Rc};
use unicode_width::UnicodeWidthChar;

pub(super) fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    markdown::strip_controls(text, false)
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
        "Alt+E",
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
                Some(' ' | '\t' | '.' | ',' | ':' | ';' | ')' | ']' | '"' | '!' | '?' | '·')
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
    Tool {
        tool: String,
        detail: String,
    },
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
            Entry::Tool { tool, detail } => tool.len() + detail.len(),
        }
    }

    pub(super) fn render(&self, width: usize) -> Vec<markdown::Line> {
        match self {
            Entry::Markdown(text) => markdown::lines(text, width),
            Entry::Prompt(text) => {
                let mut lines = Vec::new();
                let mut trimmed = text.as_str();
                while let Some(rest) = trimmed.strip_prefix('\n') {
                    lines.push(Vec::new());
                    trimmed = rest;
                }
                if width > 0 {
                    lines.extend(markdown::literal(&"─".repeat(width), Face::SUBTLE, width));
                }
                let mut parts = vec![("> ".to_owned(), Face::ACCENT_BOLD)];
                let mut plain_start = 0;
                let mut i = 0;
                while i < trimmed.len() {
                    let rest = &trimmed[i..];
                    if rest.starts_with('@')
                        && crate::input::is_mention_boundary(trimmed[..i].chars().next_back())
                    {
                        if let Ok(Some((_, len))) = crate::input::parse_mention(rest) {
                            if len > 1 {
                                if i > plain_start {
                                    parts.push((trimmed[plain_start..i].into(), Face::BOLD));
                                }
                                parts.push((trimmed[i..i + len].into(), Face::ACCENT));
                                i += len;
                                plain_start = i;
                                continue;
                            }
                        }
                    }
                    i += rest.chars().next().map_or(1, char::len_utf8);
                }
                if plain_start < trimmed.len() {
                    parts.push((trimmed[plain_start..].into(), Face::BOLD));
                }
                let borrowed: Vec<(&str, Face)> =
                    parts.iter().map(|(s, f)| (s.as_str(), *f)).collect();
                lines.extend(markdown::spans(&borrowed, width));
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
            Entry::Tool { tool, detail } => {
                let mut parts = vec![("● ".into(), Face::HIGHLIGHT), (tool.clone(), Face::BOLD)];
                if !detail.is_empty() {
                    parts.push((" ".into(), Face::MUTED));
                    parts.push((detail.clone(), Face::MUTED));
                }
                let borrowed: Vec<(&str, Face)> =
                    parts.iter().map(|(s, f)| (s.as_str(), *f)).collect();
                markdown::spans(&borrowed, width)
            }
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

/// Wrapped transcript lines shared between the render cache and the last
/// painted frame. Cloning is O(1); only selection start copies the lines.
#[derive(Clone, Default)]
pub(super) struct TranscriptLines {
    items: Rc<Vec<markdown::Line>>,
    stream: Rc<Vec<markdown::Line>>,
}

impl TranscriptLines {
    pub(super) fn len(&self) -> usize {
        self.items.len() + self.stream.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Copy `start..end` (clamped) without materializing the whole transcript.
    pub(super) fn slice(&self, start: usize, end: usize) -> Vec<markdown::Line> {
        let end = end.min(self.len());
        let start = start.min(end);
        let split = self.items.len();
        let mut out = Vec::with_capacity(end - start);
        if start < split {
            out.extend_from_slice(&self.items[start..end.min(split)]);
        }
        if end > split {
            out.extend_from_slice(&self.stream[start.max(split) - split..end - split]);
        }
        out
    }

    pub(super) fn to_vec(&self) -> Vec<markdown::Line> {
        self.slice(0, self.len())
    }
}

/// Incremental wrap cache. Items are immutable after `push`, so only newly
/// appended items are rendered; width changes, overflow trimming and `clear`
/// invalidate. The streaming tail is re-wrapped only when its text changes.
#[derive(Default)]
struct LineCache {
    width: usize,
    rendered_items: usize,
    items: Rc<Vec<markdown::Line>>,
    stream_text: String,
    stream: Rc<Vec<markdown::Line>>,
}

impl LineCache {
    fn reset(&mut self, width: usize) {
        *self = Self {
            width,
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

    pub(super) fn push(&mut self, plain: &str, entry: Entry, max_bytes: usize) {
        let keep_bytes = (max_bytes * 3) / 4;
        self.revision = self.revision.wrapping_add(1);
        self.bytes += entry.byte_len().max(plain.len()) + 1;
        self.items.push(TranscriptItem {
            plain: plain.to_owned(),
            entry,
        });
        if self.bytes > max_bytes {
            let mut drained = 0;
            while drained < self.items.len() && self.bytes > keep_bytes {
                let item = &self.items[drained];
                self.bytes = self
                    .bytes
                    .saturating_sub(item.entry.byte_len().max(item.plain.len()) + 1);
                drained += 1;
            }
            if drained > 0 {
                self.items.drain(..drained);
                // Trimming is rare (only past `max_bytes`); re-wrap lazily.
                let width = self.cache.get_mut().width;
                self.cache.get_mut().reset(width);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
        self.revision = self.revision.wrapping_add(1);
        let width = self.cache.get_mut().width;
        self.cache.get_mut().reset(width);
    }

    /// Wrapped lines for `width`, reusing previously wrapped items.
    pub(super) fn lines(&self, stream: &str, width: usize) -> TranscriptLines {
        let w = width.saturating_sub(1);
        let mut cache = self.cache.borrow_mut();
        if cache.width != w {
            cache.reset(w);
        }
        if cache.rendered_items < self.items.len() {
            let start = cache.rendered_items;
            // Copies only if a caller still holds the previous snapshot.
            let items = Rc::make_mut(&mut cache.items);
            for item in &self.items[start..] {
                items.extend(item.entry.render(w));
            }
            cache.rendered_items = self.items.len();
        }
        if cache.stream_text != stream {
            cache.stream = Rc::new(if stream.is_empty() {
                Vec::new()
            } else {
                markdown::lines(&crate::tool_codec::visible_prose(stream), w)
            });
            cache.stream_text.clear();
            cache.stream_text.push_str(stream);
        }
        TranscriptLines {
            items: Rc::clone(&cache.items),
            stream: Rc::clone(&cache.stream),
        }
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

    /// Reference: wrap every item from scratch, as before the cache existed.
    fn uncached(t: &Transcript, stream: &str, width: usize) -> Vec<markdown::Line> {
        let w = width.saturating_sub(1);
        let mut lines = Vec::new();
        for item in &t.items {
            lines.extend(item.entry.render(w));
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
        let max = 1 << 20;
        push_md(
            &mut t,
            "first **bold** paragraph that wraps at narrow widths",
            max,
        );
        assert_eq!(t.render_lines("", 40), uncached(&t, "", 40));

        // Incremental append while the previous snapshot is still held.
        let held = t.lines("", 40);
        t.push(
            "tool",
            Entry::Tool {
                tool: "shell".into(),
                detail: "ls -la".into(),
            },
            max,
        );
        push_md(&mut t, "second", max);
        assert_eq!(t.render_lines("", 40), uncached(&t, "", 40));
        assert_eq!(held.len(), uncached(&t, "", 40).len() - 2);

        // Streaming tail changes without re-wrapping items.
        assert_eq!(
            t.render_lines("stream *one*", 40),
            uncached(&t, "stream *one*", 40)
        );
        assert_eq!(
            t.render_lines("stream *one* two", 40),
            uncached(&t, "stream *one* two", 40)
        );

        // Width change invalidates.
        assert_eq!(t.render_lines("tail", 17), uncached(&t, "tail", 17));

        t.clear();
        assert!(t.lines("", 17).is_empty());
        push_md(&mut t, "after clear", max);
        assert_eq!(t.render_lines("", 17), uncached(&t, "", 17));
    }

    #[test]
    fn cached_lines_follow_overflow_trimming() {
        let mut t = Transcript::default();
        let max = 200;
        for i in 0..40 {
            push_md(&mut t, &format!("entry number {i:02} with some text"), max);
            assert_eq!(
                t.render_lines("", 30),
                uncached(&t, "", 30),
                "after push {i}"
            );
        }
        assert!(t.bytes <= max);
    }

    #[test]
    fn slice_spans_items_and_stream() {
        let mut t = Transcript::default();
        for i in 0..5 {
            push_md(&mut t, &format!("line {i}"), 1 << 20);
        }
        let stream = "s1\n\ns2\n\ns3";
        let lines = t.lines(stream, 80);
        let full = uncached(&t, stream, 80);
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
}
