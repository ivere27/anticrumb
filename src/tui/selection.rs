//! Selection uses a snapshot of the painted transcript, not raw Markdown offsets.
use super::markdown::{Line, Span};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    row: usize,
    column: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Space,
    Punct,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Space
    } else if super::markdown::is_word_char(ch) {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Char {
        anchor: Point,
        head: Point,
    },
    Block {
        anchor_start: Point,
        anchor_end: Point,
        first: Point,
        last: Point,
    },
}

pub(super) struct Selection {
    pub lines: Vec<Line>,
    mode: Mode,
}

impl Selection {
    pub fn new(lines: Vec<Line>, column: usize, row: usize) -> Option<Self> {
        if row >= lines.len() {
            return None;
        }
        let anchor = Point { row, column };
        Some(Self {
            lines,
            mode: Mode::Char {
                anchor,
                head: anchor,
            },
        })
    }

    pub fn new_block(lines: Vec<Line>, column: usize, row: usize) -> Option<Self> {
        if row >= lines.len() {
            return None;
        }
        let (start_col, end_col) = block_bounds(&lines, row, column);
        let anchor_start = Point {
            row,
            column: start_col,
        };
        let anchor_end = Point {
            row,
            column: end_col,
        };
        Some(Self {
            lines,
            mode: Mode::Block {
                anchor_start,
                anchor_end,
                first: anchor_start,
                last: anchor_end,
            },
        })
    }

    pub fn extend(&mut self, column: usize, row: usize) {
        let clamped_row = row.min(self.lines.len().saturating_sub(1));
        match &mut self.mode {
            Mode::Char { head, .. } => {
                *head = Point {
                    row: clamped_row,
                    column,
                };
            }
            Mode::Block {
                anchor_start,
                anchor_end,
                first,
                last,
            } => {
                let cursor = Point {
                    row: clamped_row,
                    column,
                };
                let (cur_start_col, cur_end_col) = block_bounds(&self.lines, clamped_row, column);
                let cur_start = Point {
                    row: clamped_row,
                    column: cur_start_col,
                };
                let cur_end = Point {
                    row: clamped_row,
                    column: cur_end_col,
                };
                if cursor < *anchor_start {
                    *first = cur_start;
                    *last = *anchor_end;
                } else if cursor > *anchor_end {
                    *first = *anchor_start;
                    *last = cur_end;
                } else {
                    *first = *anchor_start;
                    *last = *anchor_end;
                }
            }
        }
    }

    pub fn fragment_spans(&self, row: usize) -> Option<(usize, Line)> {
        let (first, last) = match self.mode {
            Mode::Char { anchor, head } => {
                if anchor == head {
                    return None;
                }
                if anchor < head {
                    (anchor, head)
                } else {
                    (head, anchor)
                }
            }
            Mode::Block { first, last, .. } => (first, last),
        };
        if row < first.row || row > last.row {
            return None;
        }
        let from = if row == first.row { first.column } else { 0 };
        let to = if row == last.row {
            last.column.saturating_add(1)
        } else {
            usize::MAX
        };
        let line = self.lines.get(row)?;
        Some(slice_line_cells(line, from, to))
    }

    pub fn fragment(&self, row: usize) -> Option<(usize, String)> {
        let (start, spans) = self.fragment_spans(row)?;
        let text = spans.into_iter().map(|span| span.text).collect::<String>();
        Some((start, text))
    }

    pub fn text(&self) -> Option<String> {
        let result = (0..self.lines.len())
            .filter_map(|row| self.fragment(row).map(|(_, text)| text))
            .collect::<Vec<_>>()
            .join("\n");
        (!result.trim().is_empty()).then_some(result)
    }
}

fn block_bounds(lines: &[Line], row: usize, column: usize) -> (usize, usize) {
    let Some(line) = lines.get(row) else {
        return (column, column);
    };
    let mut current_start = 0usize;
    let mut current_end = 0usize;
    let mut current_class: Option<CharClass> = None;
    let mut col = 0usize;

    for span in line {
        for ch in span.text.chars() {
            let cells = ch.width().unwrap_or(0);
            if cells == 0 {
                continue;
            }
            let start = col;
            let end = col.saturating_add(cells - 1);
            col = col.saturating_add(cells);
            let class = char_class(ch);
            match current_class {
                Some(prev) if prev == class && current_end + 1 == start => {
                    current_end = end;
                }
                Some(_) => {
                    if column >= current_start && column <= current_end {
                        return (current_start, current_end);
                    }
                    current_start = start;
                    current_end = end;
                    current_class = Some(class);
                }
                None => {
                    current_start = start;
                    current_end = end;
                    current_class = Some(class);
                }
            }
        }
    }

    if current_class.is_some() && column >= current_start && column <= current_end {
        (current_start, current_end)
    } else {
        (column, column)
    }
}

fn slice_line_cells(line: &[Span], from: usize, to: usize) -> (usize, Line) {
    let mut column = 0;
    let mut start = None;
    let mut output: Line = Vec::new();
    let mut selected = false;
    for span in line {
        for ch in span.text.chars() {
            let cells = ch.width().unwrap_or(0);
            if cells > 0 {
                selected = column < to && column.saturating_add(cells) > from;
            }
            if selected {
                start.get_or_insert(column);
                if let Some(last) = output.last_mut().filter(|last| last.face == span.face) {
                    last.text.push(ch);
                } else {
                    output.push(Span {
                        text: ch.into(),
                        face: span.face,
                    });
                }
            }
            column += cells;
        }
    }
    (start.unwrap_or(from), output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::markdown;

    #[test]
    fn wide_characters_and_combining_marks_are_not_split_at_cell_boundaries() {
        let mut selection =
            Selection::new(markdown::lines("A\u{754c}\u{8a9e}e\u{301}Z", 80), 2, 0).unwrap();
        assert!(
            selection.text().is_none(),
            "a click does not replace the clipboard"
        );
        selection.extend(5, 0);
        assert_eq!(
            selection.fragment(0),
            Some((1, "\u{754c}\u{8a9e}e\u{301}".into()))
        );
        assert_eq!(
            selection.text().as_deref(),
            Some("\u{754c}\u{8a9e}e\u{301}")
        );
    }

    #[test]
    fn reverse_selection_uses_visible_wrapped_rows_and_styled_table_text() {
        let mut wrapped = Selection::new(markdown::lines("abcdefgh", 4), 2, 1).unwrap();
        wrapped.extend(1, 0);
        assert_eq!(wrapped.text().as_deref(), Some("bcd\nefg"));
        let rows = markdown::lines(
            "| Name | Value |\n| --- | ---: |\n| **\u{754c}\u{8a9e}** | `42` |",
            80,
        );
        let mut table = Selection::new(rows, 0, 0).unwrap();
        table.extend(50, 2);
        let text = table.text().unwrap();
        assert!(text.contains("Name") && text.contains("\u{754c}\u{8a9e}") && text.contains("42"));
        assert!(text.contains(" | ") && text.contains("-+-"));
        assert!(!text.contains(['*', '`']));
    }

    #[test]
    fn block_selection_selects_word_and_extends_by_blocks_including_wide_chars() {
        let lines = markdown::lines("alpha foo_\u{754c}\u{8a9e} bar\nsecond line", 80);
        // Double click the identifier with two wide characters (columns 6..14).
        let mut sel = Selection::new_block(lines.clone(), 11, 0).unwrap();
        assert_eq!(sel.text().as_deref(), Some("foo_\u{754c}\u{8a9e}"));
        // Drag forward into `bar` (col 16) -> expands to full `bar` block
        sel.extend(16, 0);
        assert_eq!(sel.text().as_deref(), Some("foo_\u{754c}\u{8a9e} bar"));
        // Drag backward into `alpha` (column 2), retaining the anchor word.
        sel.extend(2, 0);
        assert_eq!(sel.text().as_deref(), Some("alpha foo_\u{754c}\u{8a9e}"));
        // Drag across lines into `second` (row 1, col 3) -> expands through end of `second`
        sel.extend(3, 1);
        assert_eq!(
            sel.text().as_deref(),
            Some("foo_\u{754c}\u{8a9e} bar\nsecond")
        );
        // Drag back inside the anchor block to select only that word.
        sel.extend(7, 0);
        assert_eq!(sel.text().as_deref(), Some("foo_\u{754c}\u{8a9e}"));

        // Single-character word block ("a") must also be selectable on double-click.
        let mut single = Selection::new_block(markdown::lines("x a y", 80), 2, 0).unwrap();
        assert_eq!(single.text().as_deref(), Some("a"));
        single.extend(4, 0);
        assert_eq!(single.text().as_deref(), Some("a y"));
    }

    #[test]
    fn fragment_spans_preserves_faces_across_partial_selection() {
        let lines = markdown::spans(
            &[
                ("plain ", markdown::Face::default()),
                (
                    "danger",
                    markdown::Face {
                        danger: true,
                        ..markdown::Face::default()
                    },
                ),
                (
                    " highlight",
                    markdown::Face {
                        highlight: true,
                        ..markdown::Face::default()
                    },
                ),
            ],
            80,
        );
        let mut sel = Selection::new(lines, 3, 0).unwrap();
        sel.extend(15, 0);
        let (col, spans) = sel.fragment_spans(0).unwrap();
        assert_eq!(col, 3);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "in ");
        assert_eq!(spans[0].face, markdown::Face::default());
        assert_eq!(spans[1].text, "danger");
        assert!(spans[1].face.danger);
        assert_eq!(spans[2].text, " hig");
        assert!(spans[2].face.highlight);
        assert_eq!(sel.fragment(0), Some((3, "in danger hig".into())));
    }
}
