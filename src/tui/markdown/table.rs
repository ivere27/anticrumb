//! Tables render with aligned columns and inner ASCII dividers (` | ` and `---+---`).
use super::{closing, inline, push, wrap, Face, Line};
use unicode_width::UnicodeWidthChar;

const GAP: usize = 3;
const MAX_COLUMNS: usize = 32;

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// Split only real cell boundaries. Escaped pipes and pipes in complete code
/// spans are cell contents; unmatched backticks remain ordinary streamed text.
fn cells(line: &str) -> Option<Vec<&str>> {
    if line.len() - line.trim_start_matches(' ').len() > 3 {
        return None;
    }
    let text = line.trim();
    let hashes = text.chars().take_while(|ch| *ch == '#').count();
    if text.starts_with("```")
        || text.starts_with("~~~")
        || ((1..=6).contains(&hashes) && text[hashes..].starts_with(' '))
        || text.starts_with('>')
    {
        return None;
    }
    let mut parts = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < text.len() {
        let ch = text[at..].chars().next()?;
        match ch {
            '\\' => {
                at += 1;
                if let Some(next) = text[at..].chars().next() {
                    at += next.len_utf8();
                }
                continue;
            }
            '`' => {
                let count = text[at..].chars().take_while(|ch| *ch == '`').count();
                if let Some(end) = closing(text, at + count, '`', count) {
                    at = end + count;
                    continue;
                }
                at += count;
                continue;
            }
            '|' => {
                parts.push(text[start..at].trim());
                start = at + 1;
                // Bound both work and layout allocation for malformed input.
                if parts.len() > MAX_COLUMNS + 1 {
                    return None;
                }
            }
            _ => (),
        }
        at += ch.len_utf8();
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(text[start..].trim());
    if text.starts_with('|') {
        parts.remove(0);
    }
    if start == text.len() {
        parts.pop();
    }
    (!parts.is_empty() && parts.len() <= MAX_COLUMNS).then_some(parts)
}

fn is_pipe_wrapped(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() >= 3
}

fn alignment(cell: &str, pipe_wrapped: bool) -> Option<Align> {
    let dash = cell.trim_start_matches(':').trim_end_matches(':');
    let min_dashes = if pipe_wrapped { 1 } else { 3 };
    if dash.len() < min_dashes || !dash.bytes().all(|ch| ch == b'-') {
        return None;
    }
    // Multiple colons are not a valid delimiter cell.
    if cell.starts_with("::") || cell.ends_with("::") {
        return None;
    }
    Some(match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => Align::Center,
        (_, true) => Align::Right,
        _ => Align::Left,
    })
}

fn cell_line(text: &str, header: bool) -> Line {
    let mut line = Vec::new();
    inline(
        text,
        Face {
            bold: header,
            ..Face::default()
        },
        0,
        &mut 64,
        &mut line,
    );
    // A tab inside a cell has no stable column until layout; normalize it here.
    for span in &mut line {
        span.text = span.text.replace('\t', "    ");
    }
    line
}

fn width(line: &Line) -> usize {
    line.iter()
        .flat_map(|span| span.text.chars())
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

fn append(to: &mut Line, from: &Line) {
    for span in from {
        push(to, &span.text, span.face);
    }
}

/// Allocate short columns their natural size, sharing the rest among long ones.
/// If even short readable cells do not fit, a stacked layout preserves the text.
fn columns(rows: &[Vec<Line>], available: usize) -> Option<Vec<usize>> {
    let count = rows.first()?.len();
    let available = available.checked_sub(GAP * count.saturating_sub(1))?;
    let natural: Vec<_> = (0..count)
        .map(|i| {
            rows.iter()
                .map(|row| width(&row[i]))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let minimum: Vec<_> = natural.iter().map(|size| (*size).min(4)).collect();
    if minimum.iter().sum::<usize>() > available {
        return None;
    }
    let allocated = |cap: usize| -> Vec<usize> {
        natural
            .iter()
            .zip(&minimum)
            .map(|(size, min)| (*size).min(cap).max(*min))
            .collect()
    };
    let (mut low, mut high) = (0, natural.iter().copied().max().unwrap_or(0).min(available));
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if allocated(middle).iter().sum::<usize>() <= available {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let mut widths = allocated(low);
    let mut spare = available.saturating_sub(widths.iter().sum());
    for (size, natural) in widths.iter_mut().zip(natural) {
        if spare > 0 && *size < natural {
            *size += 1;
            spare -= 1;
        }
    }
    Some(widths)
}

fn aligned(rows: Vec<Vec<Line>>, widths: &[usize], aligns: &[Align]) -> Vec<Line> {
    let mut result = Vec::new();
    let has_body = rows.len() > 1;
    let border_face = Face::MUTED;
    for (index, row) in rows.into_iter().enumerate() {
        let cells: Vec<Vec<Line>> = row
            .into_iter()
            .zip(widths)
            .map(|(line, width)| {
                let mut wrapped = Vec::new();
                wrap(line, *width, &mut wrapped);
                wrapped
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for y in 0..height {
            let mut line = Vec::new();
            for (x, cell) in cells.iter().enumerate() {
                let content = cell.get(y);
                let padding = widths[x].saturating_sub(content.map(width).unwrap_or(0));
                let left = match aligns[x] {
                    Align::Left => 0,
                    Align::Center => padding / 2,
                    Align::Right => padding,
                };
                push(&mut line, &" ".repeat(left), Face::default());
                if let Some(content) = content {
                    append(&mut line, content);
                }
                if x + 1 < cells.len() {
                    push(&mut line, &" ".repeat(padding - left), Face::default());
                    push(&mut line, " | ", border_face);
                }
            }
            result.push(line);
        }
        if index == 0 && has_body {
            let mut sep = Vec::new();
            for (x, w) in widths.iter().enumerate() {
                push(&mut sep, &"-".repeat(*w), border_face);
                if x + 1 < widths.len() {
                    push(&mut sep, "-+-", border_face);
                }
            }
            result.push(sep);
        }
    }
    result
}

fn stacked(rows: Vec<Vec<Line>>, available: usize) -> Vec<Line> {
    let mut result = Vec::new();
    let mut rows = rows.into_iter();
    let Some(headers) = rows.next() else {
        return result;
    };
    let mut any = false;
    for row in rows {
        if any {
            result.push(Vec::new());
        }
        any = true;
        for (header, cell) in headers.iter().zip(row) {
            let mut line = header.clone();
            push(&mut line, ": ", Face::default());
            append(&mut line, &cell);
            wrap(line, available, &mut result);
        }
    }
    if !any {
        for header in headers {
            wrap(header, available, &mut result);
        }
    }
    result
}

pub(super) fn render(source: &[&str], available: usize) -> Option<(usize, Vec<Line>)> {
    let first_line = *source.first()?;
    let second_line = *source.get(1)?;
    let headers = cells(first_line)?;
    let second_cells = cells(second_line)?;
    if headers.len() != second_cells.len() {
        return None;
    }
    let count = headers.len();
    let second_wrapped = is_pipe_wrapped(second_line);
    let (aligns, mut consumed) = if let Some(parsed) = second_cells
        .iter()
        .map(|cell| alignment(cell, second_wrapped))
        .collect::<Option<Vec<_>>>()
    {
        (parsed, 2)
    } else if is_pipe_wrapped(first_line) && second_wrapped && count >= 2 {
        (vec![Align::Left; count], 1)
    } else {
        return None;
    };
    let mut rows = vec![headers
        .into_iter()
        .map(|cell| cell_line(cell, true))
        .collect::<Vec<_>>()];
    for line in &source[consumed..] {
        let Some(mut next) = cells(line) else {
            break;
        };
        // Never discard extra cells (including a not-yet-complete next table).
        if next.len() > count {
            break;
        }
        if consumed == 1
            && next
                .iter()
                .all(|cell| alignment(cell, is_pipe_wrapped(line)).is_some())
        {
            consumed += 1;
            continue;
        }
        next.resize(count, "");
        rows.push(
            next.into_iter()
                .map(|cell| cell_line(cell, false))
                .collect(),
        );
        consumed += 1;
    }
    let rendered = if let Some(widths) = columns(&rows, available) {
        aligned(rows, &widths, &aligns)
    } else {
        stacked(rows, available)
    };
    Some((consumed, rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn table_columns_align_with_inner_ascii_borders_and_keep_inline_faces() {
        let rendered = super::super::lines(
            "| Name | Count | Rank |\n| :--- | ---: | :---: |\n| **\u{754c}\u{8a9e}** | 2 | *A* |\n| alpha | 12 | `BBB` |", 80,
        );
        assert_eq!(
            plain(&rendered),
            [
                "Name  | Count | Rank",
                "------+-------+-----",
                "\u{754c}\u{8a9e}  |     2 |  A",
                "alpha |    12 | BBB"
            ]
        );
        assert!(rendered[0]
            .iter()
            .filter(|span| !span.text.trim().is_empty() && span.text != " | ")
            .all(|span| span.face.bold));
        assert!(rendered[1].iter().all(|span| span.face.muted));
        assert!(rendered[2]
            .iter()
            .any(|span| span.text == "\u{754c}\u{8a9e}" && span.face.bold));
        assert!(rendered[2]
            .iter()
            .any(|span| span.text == "A" && span.face.italic));
        assert!(rendered[3]
            .iter()
            .any(|span| span.text == "BBB" && span.face.code));

        // Short delimiter dashes (`| - | - |`) and delimiterless pipe tables also render with inner lines.
        let short_delim = super::super::lines("| A | B |\n| - | - |\n| 1 | 2 |", 80);
        assert_eq!(plain(&short_delim), ["A | B", "--+--", "1 | 2"]);
        let pipe_only = super::super::lines("| Col1 | Col2 |\n| val1 | val2 |", 80);
        assert_eq!(
            plain(&pipe_only),
            ["Col1 | Col2", "-----+-----", "val1 | val2"]
        );
    }

    #[test]
    fn escaped_and_code_pipes_are_content_and_fences_remain_literal() {
        let rendered = super::super::lines(
            "Key | Value\n--- | ---\na\\|b | `x|y`\n\n```text\nA | B\n--- | ---\na | b\n```\nend",
            80,
        );
        assert_eq!(
            plain(&rendered),
            [
                "Key | Value",
                "----+------",
                "a|b | x|y",
                "",
                "text",
                "A | B",
                "--- | ---",
                "a | b",
                "end"
            ]
        );
        let fence = super::super::lines("```a|b\n---|---\nx|y\n```", 80);
        assert_eq!(plain(&fence), ["a|b", "---|---", "x|y"]);
        let heading = super::super::lines("# A | B\n--- | ---", 80);
        assert_eq!(plain(&heading), ["A | B", "--- | ---"]);
    }

    #[test]
    fn narrow_tables_wrap_cells_or_stack_without_losing_unicode_or_overflowing() {
        let source = "Name | Details | Count\n--- | --- | ---:\n\u{754c}\u{8a9e} | abcdefghijklmnop | 12345\nsecond | \u{4e00}\u{4e8c}\u{4e09}\u{56db}\u{4e94} | 9";
        for available in [2, 8, 12, 20, 30, 60] {
            let rendered = super::super::lines(source, available);
            assert!(
                rendered.iter().all(|line| width(line) <= available),
                "{available}: {:?}",
                plain(&rendered)
            );
            let text = plain(&rendered).join("");
            let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
            for value in [
                "\u{754c}\u{8a9e}",
                "abcdefghijklmnop",
                "12345",
                "second",
                "\u{4e00}\u{4e8c}\u{4e09}\u{56db}\u{4e94}",
            ] {
                let mut chars = compact.chars();
                assert!(
                    value.chars().all(|wanted| chars.any(|ch| ch == wanted)),
                    "{available}: {compact}"
                );
            }
            assert!(!text.starts_with('|') && !text.ends_with('|'));
        }
    }

    #[test]
    fn incomplete_or_malformed_tables_keep_streamed_text_and_extra_cells() {
        for text in [
            "A | B\n--- | -",
            "A | B\n--- | nope",
            "A | B\n--- | --- | ---",
        ] {
            assert_eq!(plain(&super::super::lines(text, 80)).join("\n"), text);
        }
        let rendered = super::super::lines("A | B\n--- | ---\nfirst | second | extra\nafter", 80);
        assert_eq!(
            plain(&rendered),
            ["A | B", "first | second | extra", "after"]
        );
        let partial = super::super::lines("A | B\n--- | ---\n| still typing", 80);
        assert!(plain(&partial).join("\n").contains("still typing"));
    }
}
