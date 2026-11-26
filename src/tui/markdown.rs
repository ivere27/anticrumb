//! Small, streaming-friendly Markdown presentation for the terminal transcript.
//! Styling is generated locally; terminal control sequences in content are discarded.
mod table;

use super::theme::Theme;
use crossterm::{queue, style};
use std::borrow::Cow;
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Face {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub heading: bool,
    pub muted: bool,
    pub subtle: bool,
    pub accent: bool,
    pub highlight: bool,
    pub danger: bool,
}

impl Face {
    pub const TEXT: Self = Self {
        bold: false,
        italic: false,
        strike: false,
        code: false,
        heading: false,
        muted: false,
        subtle: false,
        accent: false,
        highlight: false,
        danger: false,
    };
    pub const BOLD: Self = Self {
        bold: true,
        ..Self::TEXT
    };
    pub const CODE: Self = Self {
        code: true,
        ..Self::TEXT
    };
    pub const CODE_BOLD: Self = Self {
        bold: true,
        code: true,
        ..Self::TEXT
    };
    pub const HEADING: Self = Self {
        bold: true,
        heading: true,
        ..Self::TEXT
    };
    pub const MUTED: Self = Self {
        muted: true,
        ..Self::TEXT
    };
    pub const SUBTLE: Self = Self {
        subtle: true,
        ..Self::TEXT
    };
    pub const ACCENT: Self = Self {
        accent: true,
        ..Self::TEXT
    };
    pub const ACCENT_BOLD: Self = Self {
        bold: true,
        accent: true,
        ..Self::TEXT
    };
    pub const HIGHLIGHT: Self = Self {
        highlight: true,
        ..Self::TEXT
    };
    pub const HIGHLIGHT_BOLD: Self = Self {
        bold: true,
        highlight: true,
        ..Self::TEXT
    };
    pub const DANGER: Self = Self {
        danger: true,
        ..Self::TEXT
    };
    pub const DANGER_BOLD: Self = Self {
        bold: true,
        danger: true,
        ..Self::TEXT
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub text: String,
    pub(super) face: Face,
}
pub(super) type Line = Vec<Span>;

fn push(spans: &mut Line, text: &str, face: Face) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut().filter(|span| span.face == face) {
        last.text.push_str(text);
    } else {
        spans.push(Span {
            text: text.into(),
            face,
        });
    }
}

fn push_char(spans: &mut Line, ch: char, face: Face) {
    if let Some(last) = spans.last_mut().filter(|span| span.face == face) {
        last.text.push(ch);
    } else {
        let mut text = String::with_capacity(ch.len_utf8());
        text.push(ch);
        spans.push(Span { text, face });
    }
}

pub(super) fn strip_controls<'a>(
    text: &'a str,
    allow_newlines_and_tabs: bool,
) -> impl Iterator<Item = char> + 'a {
    text.chars().filter(move |ch| {
        !ch.is_control() || (allow_newlines_and_tabs && matches!(ch, '\n' | '\t'))
    })
}

/// Zero-allocation fast path for `clean` when `text` contains no ANSI escapes,
/// ASCII controls (other than `\n`/`\t`), DEL (`0x7f`), or UTF-8 C1 controls (`0xC2 0x80..0x9F`).
pub(super) fn clean_cow(text: &str) -> Cow<'_, str> {
    if text
        .bytes()
        .all(|b| b != 0x7f && b != 0xc2 && (b >= 0x20 || b == b'\n' || b == b'\t'))
    {
        return Cow::Borrowed(text);
    }
    Cow::Owned(clean_alloc(text))
}

pub(super) fn clean(text: &str) -> String {
    clean_cow(text).into_owned()
}

fn clean_alloc(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut output = String::with_capacity(text.len());
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for ch in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '^' | '_') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\u{7}' || ch == '\u{1b}' && chars.next() == Some('\\') {
                            break;
                        }
                    }
                }
                _ => (),
            }
        } else if !ch.is_control() || matches!(ch, '\n' | '\t') {
            output.push(ch);
        }
    }
    output
}

pub(super) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn word(ch: Option<char>) -> bool {
    ch.is_some_and(is_word_char)
}

fn closing(text: &str, start: usize, mark: char, count: usize) -> Option<usize> {
    // A malformed or huge streamed line must not make every repaint quadratic.
    let mut end = text.len().min(start.saturating_add(8192));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text = &text[..end];
    let token = mark.to_string().repeat(count);
    let mut offset = start;
    while let Some(relative) = text[offset..].find(&token) {
        let at = offset + relative;
        let before = text[..at].chars().next_back();
        let after = text[at + count..].chars().next();
        if before != Some('\\')
            && before != Some(mark)
            && after != Some(mark)
            && (mark == '`' || before.is_some_and(|ch| !ch.is_whitespace()))
            && (mark != '_' || !word(after))
        {
            return Some(at);
        }
        offset = at + count;
    }
    None
}

fn inline(text: &str, face: Face, depth: usize, budget: &mut usize, spans: &mut Line) {
    if depth >= 8 || *budget == 0 {
        push(spans, text, face);
        return;
    }
    let mut at = 0;
    while at < text.len() {
        let ch = text[at..].chars().next().unwrap();
        if ch == '\\' {
            if let Some(next) = text[at + 1..]
                .chars()
                .next()
                .filter(char::is_ascii_punctuation)
            {
                push_char(spans, next, face);
                at += 1 + next.len_utf8();
                continue;
            }
        }
        if matches!(ch, '*' | '_' | '`' | '~') {
            if *budget == 0 {
                push(spans, &text[at..], face);
                return;
            }
            *budget -= 1;
            let count = text[at..].chars().take_while(|c| *c == ch).count();
            let next = text[at + count..].chars().next();
            let before = text[..at].chars().next_back();
            let allowed = match ch {
                '`' => true,
                '~' => count == 2,
                '_' => count <= 3 && !word(before),
                _ => count <= 3,
            };
            if allowed && next.is_some_and(|c| ch == '`' || !c.is_whitespace()) {
                if let Some(end) = closing(text, at + count, ch, count) {
                    let inner = &text[at + count..end];
                    if !inner.is_empty() {
                        let mut emphasized = face;
                        match ch {
                            '`' => emphasized.code = true,
                            '~' => emphasized.strike = true,
                            _ => {
                                emphasized.bold |= count >= 2;
                                emphasized.italic |= count == 1 || count == 3;
                            }
                        }
                        if ch == '`' {
                            push(spans, inner, emphasized);
                        } else {
                            inline(inner, emphasized, depth + 1, budget, spans);
                        }
                        at = end + count;
                        continue;
                    }
                }
            }
            // An unfinished streaming delimiter remains literal until its close arrives.
            push(spans, &text[at..at + count], face);
            at += count;
        } else {
            push_char(spans, ch, face);
            at += ch.len_utf8();
        }
    }
}

fn wrap(spans: Line, width: usize, lines: &mut Vec<Line>) {
    let width = width.max(1);
    let mut line = Vec::new();
    let mut used = 0;
    for span in spans {
        for ch in span.text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut line));
                used = 0;
                continue;
            }
            let repeat = if ch == '\t' { 4 - used % 4 } else { 1 };
            let ch = if ch == '\t' { ' ' } else { ch };
            let cells = ch.width().unwrap_or(0);
            for _ in 0..repeat {
                if used + cells > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                push_char(&mut line, ch, span.face);
                used += cells;
            }
        }
    }
    lines.push(line);
}

pub(super) fn lines(text: &str, width: usize) -> Vec<Line> {
    let text = clean_cow(text);
    let mut result = Vec::new();
    let mut fence = None::<(char, usize)>;
    let source: Vec<_> = text.split('\n').collect();
    let mut index = 0;
    while index < source.len() {
        let line = source[index];
        if fence.is_none() {
            if let Some((consumed, rendered)) = table::render(&source[index..], width) {
                result.extend(rendered);
                index += consumed;
                continue;
            }
        }
        index += 1;
        let trimmed = line.trim_start_matches(' ');
        let leading = line.len() - trimmed.len();
        let mark = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|ch| *ch == mark).count();
        let mut spans = Vec::new();
        let mut budget = 64;
        if let Some((opened, length)) = fence {
            if leading <= 3
                && mark == opened
                && count >= length
                && trimmed[count..].trim().is_empty()
            {
                fence = None;
                continue;
            }
            push(&mut spans, line, Face::CODE);
        } else if leading <= 3 && matches!(mark, '`' | '~') && count >= 3 {
            fence = Some((mark, count));
            let language = trimmed[count..].trim();
            push(
                &mut spans,
                if language.is_empty() {
                    "code"
                } else {
                    language
                },
                Face::MUTED,
            );
        } else if leading <= 3 && mark == '#' && count <= 6 && trimmed[count..].starts_with(' ') {
            inline(
                trimmed[count..].trim_start(),
                Face::HEADING,
                0,
                &mut budget,
                &mut spans,
            );
        } else {
            inline(line, Face::default(), 0, &mut budget, &mut spans);
        }
        wrap(spans, width, &mut result);
    }
    result
}

pub(super) fn literal(text: &str, face: Face, width: usize) -> Vec<Line> {
    let text = clean_cow(text);
    let mut result = Vec::new();
    for line in text.split('\n') {
        let mut spans = Vec::new();
        push(&mut spans, line, face);
        wrap(spans, width, &mut result);
    }
    result
}

pub(super) fn prefixed(
    prefix: &str,
    prefix_face: Face,
    body: &str,
    body_face: Face,
    width: usize,
) -> Vec<Line> {
    let prefix = clean_cow(prefix);
    let body = clean_cow(body);
    let mut result = Vec::new();
    let mut indent: Option<String> = None;
    for (index, line) in body.split('\n').enumerate() {
        let mut spans = Vec::new();
        if index == 0 {
            push(&mut spans, &prefix, prefix_face);
        } else {
            let spaces = indent.get_or_insert_with(|| " ".repeat(prefix.chars().count()));
            push(&mut spans, spaces, prefix_face);
        }
        push(&mut spans, line, body_face);
        wrap(spans, width, &mut result);
    }
    result
}

pub(super) fn spans(parts: &[(&str, Face)], width: usize) -> Vec<Line> {
    let mut line = Vec::new();
    for (text, face) in parts {
        let cleaned = clean_cow(text);
        push(&mut line, &cleaned, *face);
    }
    let mut result = Vec::new();
    wrap(line, width, &mut result);
    result
}

fn is_dark_color(color: style::Color) -> bool {
    match color {
        style::Color::Rgb { r, g, b } => {
            (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) < 107_000
        }
        style::Color::Red
        | style::Color::Blue
        | style::Color::Magenta
        | style::Color::DarkRed
        | style::Color::DarkBlue
        | style::Color::DarkMagenta
        | style::Color::Black => true,
        _ => false,
    }
}

fn span_colors(face: Face, theme: Theme, selected: bool) -> (style::Color, style::Color) {
    if !selected {
        let fg = if face.danger {
            theme.danger
        } else if face.highlight {
            theme.highlight
        } else if face.subtle {
            theme.subtle
        } else if face.muted {
            theme.muted
        } else if face.heading {
            theme.heading
        } else if face.code {
            theme.code
        } else if face.accent {
            theme.accent
        } else {
            theme.text
        };
        return (fg, theme.background);
    }
    let semantic = if face.danger {
        Some(theme.danger)
    } else if face.highlight {
        Some(theme.highlight)
    } else if face.subtle || face.muted {
        None
    } else if face.heading {
        Some(theme.heading)
    } else if face.code {
        Some(theme.code)
    } else if face.accent {
        Some(theme.accent)
    } else {
        None
    };
    match semantic {
        Some(bg) => {
            let fg = if is_dark_color(bg) {
                style::Color::White
            } else {
                style::Color::Black
            };
            (fg, bg)
        }
        None => (style::Color::Black, style::Color::White),
    }
}

fn write_styled_line(
    out: &mut impl Write,
    line: &Line,
    theme: Theme,
    selected: bool,
) -> io::Result<()> {
    for span in line {
        let face = span.face;
        let (fg, bg) = span_colors(face, theme, selected);
        queue!(
            out,
            style::SetAttribute(style::Attribute::Reset),
            style::SetBackgroundColor(bg),
            style::SetForegroundColor(fg)
        )?;
        if face.bold {
            queue!(out, style::SetAttribute(style::Attribute::Bold))?;
        }
        if face.italic {
            queue!(out, style::SetAttribute(style::Attribute::Italic))?;
        }
        if face.strike {
            queue!(out, style::SetAttribute(style::Attribute::CrossedOut))?;
        }
        queue!(out, style::Print(&span.text))?;
    }
    // No emphasis or code color may leak into the composer, menus, or status bar.
    queue!(
        out,
        style::SetAttribute(style::Attribute::Reset),
        style::SetBackgroundColor(theme.background),
        style::SetForegroundColor(theme.text)
    )
}

pub(super) fn write_spaces(out: &mut impl Write, mut count: usize) -> io::Result<()> {
    const SPACES: &str = "                                                                ";
    while count > 0 {
        let chunk = count.min(SPACES.len());
        out.write_all(&SPACES.as_bytes()[..chunk])?;
        count -= chunk;
    }
    Ok(())
}

fn write_face_style(
    out: &mut impl Write,
    face: Face,
    theme: Theme,
    selected: bool,
) -> io::Result<()> {
    let (fg, bg) = span_colors(face, theme, selected);
    queue!(
        out,
        style::SetAttribute(style::Attribute::Reset),
        style::SetBackgroundColor(bg),
        style::SetForegroundColor(fg)
    )?;
    if face.bold {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if face.italic {
        queue!(out, style::SetAttribute(style::Attribute::Italic))?;
    }
    if face.strike {
        queue!(out, style::SetAttribute(style::Attribute::CrossedOut))?;
    }
    Ok(())
}

/// Stream a styled line clipped to `max_width` display columns directly to `out`
/// with zero intermediate `Vec`/`String` allocations, optionally preceded by a single
/// indent space and right-padded with spaces up to `pad_to.min(max_width)`.
/// Returns the number of display columns written (excluding right padding).
pub(super) fn write_clipped_padded_line(
    out: &mut impl Write,
    line: &[Span],
    leading_space: bool,
    max_width: usize,
    pad_to: usize,
    theme: Theme,
) -> io::Result<usize> {
    let mut used = 0usize;
    let mut last_face: Option<Face> = None;
    if leading_space && max_width > 0 {
        write_face_style(out, Face::TEXT, theme, false)?;
        out.write_all(b" ")?;
        used = 1;
        last_face = Some(Face::TEXT);
    }
    'spans: for span in line {
        if used >= max_width {
            break;
        }
        let cleaned = clean_cow(&span.text);
        // Fast path: find the longest prefix with no newline/tab/control chars that fits `max_width`.
        let mut fast_end = 0usize;
        let mut fast_w = 0usize;
        let mut needs_slow = false;
        for (byte_idx, ch) in cleaned.char_indices() {
            if matches!(ch, '\n' | '\r' | '\t') || ch.is_control() {
                needs_slow = true;
                break;
            }
            let cw = ch.width().unwrap_or(0);
            if used + fast_w + cw > max_width {
                break;
            }
            fast_w += cw;
            fast_end = byte_idx + ch.len_utf8();
        }
        if !needs_slow {
            if fast_end > 0 {
                if last_face != Some(span.face) {
                    write_face_style(out, span.face, theme, false)?;
                    last_face = Some(span.face);
                }
                out.write_all(&cleaned.as_bytes()[..fast_end])?;
                used += fast_w;
            }
            if fast_end < cleaned.len() {
                break 'spans;
            }
            continue;
        }
        let mut buf = [0u8; 4];
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
                break 'spans;
            }
            if last_face != Some(span.face) {
                write_face_style(out, span.face, theme, false)?;
                last_face = Some(span.face);
            }
            out.write_all(ch.encode_utf8(&mut buf).as_bytes())?;
            used += cw;
        }
    }
    queue!(
        out,
        style::SetAttribute(style::Attribute::Reset),
        style::SetBackgroundColor(theme.background),
        style::SetForegroundColor(theme.text)
    )?;
    let fill = pad_to.min(max_width).saturating_sub(used);
    if fill > 0 {
        write_spaces(out, fill)?;
    }
    Ok(used)
}

pub(super) fn write_line(out: &mut impl Write, line: &Line, theme: Theme) -> io::Result<()> {
    write_styled_line(out, line, theme, false)
}

pub(super) fn write_selection(out: &mut impl Write, line: &Line, theme: Theme) -> io::Result<()> {
    write_styled_line(out, line, theme, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plain(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn emphasis_code_and_headings_have_distinct_faces() {
        let result = lines(
            "# Result\nplain **important** *note* `file_name.rs` ~~old~~\n***both***",
            100,
        );
        assert_eq!(
            plain(&result),
            "Result\nplain important note file_name.rs old\nboth"
        );
        assert!(result[0][0].face.bold);
        for (text, matches) in [
            (
                "important",
                Face {
                    bold: true,
                    ..Face::default()
                },
            ),
            (
                "note",
                Face {
                    italic: true,
                    ..Face::default()
                },
            ),
            (
                "file_name.rs",
                Face {
                    code: true,
                    ..Face::default()
                },
            ),
            (
                "old",
                Face {
                    strike: true,
                    ..Face::default()
                },
            ),
        ] {
            assert!(
                result[1]
                    .iter()
                    .any(|s| s.text == text && s.face == matches),
                "{text}"
            );
        }
        assert!(result[2][0].face.bold && result[2][0].face.italic);
    }

    #[test]
    fn code_is_literal_and_unfinished_markdown_does_not_hide_text() {
        let result = lines("a_file_name **unfinished\n```rust\nlet x = \"**literal**\";\n```\nafter\n\\*literal* and ``a ` b``", 100);
        assert_eq!(
            plain(&result),
            "a_file_name **unfinished\nrust\nlet x = \"**literal**\";\nafter\n*literal* and a ` b"
        );
        assert!(result[2].iter().all(|s| s.face.code));
        assert!(result[3].iter().all(|s| s.face == Face::default()));
    }

    #[test]
    fn wrapping_preserves_unicode_and_emphasis_across_rows() {
        let result = lines(
            "**\u{4e00}\u{4e8c}\u{4e09} e\u{301}**\n````\n\t42\n```\n````",
            4,
        );
        assert_eq!(
            plain(&result),
            "\u{4e00}\u{4e8c}\n\u{4e09} e\u{301}\ncode\n    \n42\n```"
        );
        assert!(result[..2].iter().flatten().all(|s| s.face.bold));
        assert!(result[3..].iter().flatten().all(|s| s.face.code));
    }

    #[test]
    fn untrusted_controls_are_removed_and_terminal_styles_are_reset() {
        let result = lines("\u{1b}[2J**safe**\u{1b}]52;c;private\u{7} `code`\u{1b}]8;;https://example.invalid\u{1b}\\link\u{1b}]8;;\u{1b}\\", 100);
        assert_eq!(plain(&result), "safe codelink");
        let mut output = Vec::new();
        write_line(&mut output, &result[0], super::super::theme::THEMES[0]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[1m"));
        assert!(!output.contains("[2J") && !output.contains("]52;") && !output.contains("]8;"));
        let reset = format!(
            "{}{}{}",
            style::SetAttribute(style::Attribute::Reset),
            style::SetBackgroundColor(style::Color::Reset),
            style::SetForegroundColor(style::Color::Reset)
        );
        assert!(output.ends_with(&reset));
    }

    #[test]
    fn selection_uses_white_background_black_normal_text_and_keeps_special_colors() {
        let nord = super::super::theme::find("nord").unwrap();
        let line_spans = spans(
            &[
                ("normal ", Face::default()),
                (
                    "muted ",
                    Face {
                        muted: true,
                        ..Face::default()
                    },
                ),
                (
                    "err ",
                    Face {
                        danger: true,
                        ..Face::default()
                    },
                ),
                (
                    "ok ",
                    Face {
                        highlight: true,
                        ..Face::default()
                    },
                ),
                (
                    "cmd",
                    Face {
                        accent: true,
                        bold: true,
                        ..Face::default()
                    },
                ),
            ],
            100,
        );
        let mut output = Vec::new();
        write_selection(&mut output, &line_spans[0], nord).unwrap();
        let output = String::from_utf8(output).unwrap();

        let white_bg = format!("{}", style::SetBackgroundColor(style::Color::White));
        let black_fg = format!("{}", style::SetForegroundColor(style::Color::Black));
        let danger_bg = format!("{}", style::SetBackgroundColor(nord.danger));
        let highlight_bg = format!("{}", style::SetBackgroundColor(nord.highlight));
        let accent_bg = format!("{}", style::SetBackgroundColor(nord.accent));

        assert!(output.contains(&format!("{white_bg}{black_fg}normal ")));
        assert!(output.contains(&format!("{white_bg}{black_fg}muted ")));
        assert!(output.contains(&format!("{danger_bg}{black_fg}err ")));
        assert!(output.contains(&format!("{highlight_bg}{black_fg}ok ")));
        assert!(output.contains(&format!("{accent_bg}{black_fg}\u{1b}[1mcmd")));

        let reset = format!(
            "{}{}{}",
            style::SetAttribute(style::Attribute::Reset),
            style::SetBackgroundColor(nord.background),
            style::SetForegroundColor(nord.text)
        );
        assert!(output.ends_with(&reset));

        let latte = super::super::theme::find("catppuccin-latte").unwrap();
        let latte_err = spans(
            &[(
                "err",
                Face {
                    danger: true,
                    ..Face::default()
                },
            )],
            80,
        );
        let mut latte_out = Vec::new();
        write_selection(&mut latte_out, &latte_err[0], latte).unwrap();
        let latte_str = String::from_utf8(latte_out).unwrap();
        let latte_danger_bg = format!("{}", style::SetBackgroundColor(latte.danger));
        let white_fg = format!("{}", style::SetForegroundColor(style::Color::White));
        assert!(latte_str.contains(&format!("{latte_danger_bg}{white_fg}err")));
    }

    #[test]
    fn default_terminal_theme_separates_heading_code_bold_and_readable_muted() {
        let term = super::super::theme::THEMES[0];
        assert_eq!(term.id, "terminal");
        let parsed = lines("# Title\n**strong** `sym`", 80);
        let extra = spans(&[("note", Face::MUTED), ("───", Face::SUBTLE)], 80);

        let mut bytes = Vec::new();
        for line in parsed.iter().chain(extra.iter()) {
            write_line(&mut bytes, line, term).unwrap();
            bytes.push(b'\n');
        }
        let out = String::from_utf8(bytes).unwrap();
        // Heading uses heading blue + bold; code uses cyan; strong bold uses default fg + bold.
        assert!(out.contains(&format!(
            "{}\x1b[1mTitle",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 96,
                g: 165,
                b: 250
            })
        )));
        assert!(out.contains(&format!(
            "{}\x1b[1mstrong",
            style::SetForegroundColor(style::Color::Reset)
        )));
        assert!(out.contains(&format!(
            "{}sym",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 103,
                g: 232,
                b: 249
            })
        )));
        // Muted uses readable #9ca3af silver-gray and subtle uses #52525b.
        assert!(out.contains(&format!(
            "{}note",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 156,
                g: 163,
                b: 175
            })
        )));
        assert!(out.contains(&format!(
            "{}───",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 82,
                g: 82,
                b: 91
            })
        )));
    }

    #[test]
    fn spans_splits_embedded_newlines_into_distinct_rows_without_raw_lf() {
        let result = spans(
            &[("first\nsecond", Face::BOLD), ("\n\nthird", Face::ACCENT)],
            80,
        );
        assert_eq!(result.len(), 4);
        assert_eq!(plain(&result), "first\nsecond\n\nthird");
        assert!(result
            .iter()
            .flatten()
            .all(|span| !span.text.contains('\n')));
        assert!(result[0][0].face.bold);
        assert!(result[1][0].face.bold);
        assert!(result[2].is_empty());
        assert!(result[3][0].face.accent);
    }
}
