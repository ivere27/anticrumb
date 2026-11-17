//! Bounded transcript buffer, entry styling, and plain-text projection.
use super::markdown::{self, Face};
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

#[derive(Default)]
pub(super) struct Transcript {
    items: Vec<TranscriptItem>,
    bytes: usize,
}

impl Transcript {
    pub(super) fn push(&mut self, plain: &str, entry: Entry, max_bytes: usize) {
        let keep_bytes = (max_bytes * 3) / 4;
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
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
    }

    pub(super) fn render_lines(&self, stream: &str, width: usize) -> Vec<markdown::Line> {
        let w = width.saturating_sub(1);
        let mut lines = Vec::new();
        for item in &self.items {
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
