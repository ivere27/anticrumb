//! Small, streaming-friendly Markdown presentation for the terminal transcript.
//! Styling is generated locally; terminal control sequences in content are discarded.
mod table;

use super::theme::Theme;
use crossterm::{queue, style};
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

pub(super) fn strip_controls<'a>(
    text: &'a str,
    allow_newlines_and_tabs: bool,
) -> impl Iterator<Item = char> + 'a {
    text.chars().filter(move |ch| {
        !ch.is_control() || (allow_newlines_and_tabs && matches!(ch, '\n' | '\t'))
    })
}

fn clean(text: &str) -> String {
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
                push(spans, &next.to_string(), face);
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
            push(spans, &ch.to_string(), face);
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
            let repeat = if ch == '\t' { 4 - used % 4 } else { 1 };
            let ch = if ch == '\t' { ' ' } else { ch };
            let cells = ch.width().unwrap_or(0);
            for _ in 0..repeat {
                if used + cells > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                push(&mut line, &ch.to_string(), span.face);
                used += cells;
            }
        }
    }
    lines.push(line);
}

pub(super) fn lines(text: &str, width: usize) -> Vec<Line> {
    let text = clean(text);
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
    let text = clean(text);
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
    let prefix = clean(prefix);
    let body = clean(body);
    let mut result = Vec::new();
    for (index, line) in body.split('\n').enumerate() {
        let mut spans = Vec::new();
        if index == 0 {
            push(&mut spans, &prefix, prefix_face);
        } else {
            let indent = " ".repeat(prefix.chars().count());
            push(&mut spans, &indent, prefix_face);
        }
        push(&mut spans, line, body_face);
        wrap(spans, width, &mut result);
    }
    result
}

pub(super) fn spans(parts: &[(&str, Face)], width: usize) -> Vec<Line> {
    let mut line = Vec::new();
    for (text, face) in parts {
        let cleaned = clean(text);
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
}
