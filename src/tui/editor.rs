//! Composer text is stored in full. Folded pastes affect presentation only.
use serde::{Deserialize, Deserializer, Serialize};
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) const LARGE_PASTE_CHARS: usize = 1000;
pub(super) const MAX_INPUT_BYTES: usize = 48_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Paste {
    start: usize,
    end: usize,
    folded: bool,
    block: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Draft {
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pastes: Vec<Paste>,
}
impl<'de> Deserialize<'de> for Draft {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Legacy(String),
            Draft {
                text: String,
                #[serde(default)]
                pastes: Vec<Paste>,
            },
        }
        let mut draft = match Stored::deserialize(deserializer)? {
            Stored::Legacy(text) => Self::plain(text),
            Stored::Draft { text, pastes } => Self { text, pastes },
        };
        draft.pastes.retain(|p| {
            p.start < p.end
                && p.end <= draft.text.len()
                && draft.text.is_char_boundary(p.start)
                && draft.text.is_char_boundary(p.end)
        });
        draft.pastes.sort_by_key(|p| (p.start, p.end));
        let mut end = 0;
        for paste in &mut draft.pastes {
            if paste.folded && paste.start < end {
                paste.folded = false;
            }
            if paste.folded {
                end = paste.end;
            }
        }
        Ok(draft)
    }
}
impl Draft {
    pub fn plain(text: String) -> Self {
        Self {
            text,
            pastes: Vec::new(),
        }
    }
    pub fn literal_ranges(&self) -> Vec<Range<usize>> {
        self.pastes.iter().map(|p| p.start..p.end).collect()
    }
    pub fn is_block_at(&self, position: usize) -> bool {
        self.pastes
            .iter()
            .any(|p| p.block && p.start <= position && position < p.end)
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Default)]
pub(super) struct Editor {
    pub text: String,
    pub cursor: usize,
    pastes: Vec<Paste>,
    preferred_column: Option<usize>,
}
struct Layout {
    rows: Vec<String>,
    points: Vec<(usize, usize, usize)>, // source byte offset, visual row, cell column
}
impl Layout {
    fn position(&self, cursor: usize) -> (usize, usize) {
        self.points
            .iter()
            .rev()
            .find(|(byte, _, _)| *byte <= cursor)
            .map(|(_, row, col)| (*row, *col))
            .unwrap_or((0, 0))
    }
    fn point(&mut self, byte: usize, col: usize) {
        let position = (byte, self.rows.len() - 1, col);
        if self.points.last().is_some_and(|p| p.0 == byte) {
            *self.points.last_mut().unwrap() = position;
        } else {
            self.points.push(position);
        }
    }
    fn character(&mut self, ch: char, col: &mut usize, width: usize) {
        let cells = if ch == '\t' {
            2
        } else {
            ch.width().unwrap_or(0)
        };
        if ch != '\n' && *col + cells > width && *col > 0 {
            self.rows.push(String::new());
            *col = 0;
        }
        if ch == '\n' {
            self.rows.push(String::new());
            *col = 0;
        } else {
            let row = self.rows.last_mut().unwrap();
            if ch == '\t' {
                row.push_str("  ");
            } else {
                row.push(ch);
            }
            *col += cells;
        }
    }
}
impl Editor {
    pub fn set(&mut self, text: String) {
        self.restore(Draft::plain(text));
    }
    pub fn restore(&mut self, draft: Draft) {
        let clean = Self::clean(&draft.text);
        self.pastes = if clean == draft.text {
            draft.pastes
        } else {
            Vec::new()
        };
        self.cursor = clean.len();
        self.text = clean;
        self.preferred_column = None;
    }
    pub fn draft(&self) -> Draft {
        Draft {
            text: self.text.clone(),
            pastes: self.pastes.clone(),
        }
    }
    fn clean(text: &str) -> String {
        let normalized = text.replace("\r\n", "\n");
        super::markdown::strip_controls(&normalized, true).collect()
    }
    pub fn insert(&mut self, text: &str) -> bool {
        let clean = Self::clean(text);
        self.replace(self.cursor..self.cursor, &clean)
    }
    pub fn paste(&mut self, text: &str) -> bool {
        let text = Self::clean(text);
        let start = self.cursor;
        if !self.replace(start..start, &text) {
            return false;
        }
        if !text.is_empty() {
            let block = text.chars().count() > LARGE_PASTE_CHARS;
            self.pastes.push(Paste {
                start,
                end: start + text.len(),
                folded: block,
                block,
            });
            self.pastes.sort_by_key(|p| (p.start, p.end));
        }
        true
    }
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> bool {
        if self.text.len() - (range.end - range.start) + text.len() > MAX_INPUT_BYTES {
            return false;
        }
        let removed = range.end - range.start;
        let inserted = text.len();
        self.pastes.retain_mut(|paste| {
            if paste.end <= range.start {
                return true;
            }
            if paste.start >= range.end {
                paste.start = paste.start - removed + inserted;
                paste.end = paste.end - removed + inserted;
                return true;
            }
            // Edits in an expanded paste remain literal text, including @.
            let start = paste.start.min(range.start);
            let end = if paste.end > range.end {
                paste.end - removed + inserted
            } else {
                range.start + inserted
            };
            paste.start = start;
            paste.end = end;
            paste.folded = false;
            start < end
        });
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + inserted;
        self.preferred_column = None;
        true
    }
    fn folded_before(&self) -> Option<Range<usize>> {
        self.pastes
            .iter()
            .find(|p| p.folded && p.end == self.cursor)
            .map(|p| p.start..p.end)
    }
    fn folded_after(&self) -> Option<Range<usize>> {
        self.pastes
            .iter()
            .find(|p| p.folded && p.start == self.cursor)
            .map(|p| p.start..p.end)
    }
    pub fn left(&mut self) {
        self.preferred_column = None;
        if let Some(range) = self.folded_before() {
            self.cursor = range.start;
        } else if let Some((i, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = i;
        }
    }
    pub fn right(&mut self) {
        self.preferred_column = None;
        if let Some(range) = self.folded_after() {
            self.cursor = range.end;
        } else if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
    pub fn word_left(&mut self) {
        self.cursor = self.previous_block_boundary();
        self.preferred_column = None;
    }
    pub fn word_right(&mut self) {
        self.cursor = self.next_block_boundary();
        self.preferred_column = None;
    }
    pub fn home(&mut self) {
        self.cursor = 0;
        self.preferred_column = None;
    }
    pub fn end(&mut self) {
        self.cursor = self.text.len();
        self.preferred_column = None;
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.replace(self.cursor..end, "");
    }
    pub fn delete(&mut self) {
        let end = self.folded_after().map(|r| r.end).or_else(|| {
            self.text[self.cursor..]
                .chars()
                .next()
                .map(|c| self.cursor + c.len_utf8())
        });
        if let Some(end) = end {
            self.replace(self.cursor..end, "");
        }
    }
    pub fn kill_before(&mut self) {
        self.replace(0..self.cursor, "");
    }
    pub fn kill_after(&mut self) {
        self.replace(self.cursor..self.text.len(), "");
    }
    fn is_word_char(ch: char) -> bool {
        super::markdown::is_word_char(ch)
    }
    pub fn delete_word(&mut self) {
        if let Some(range) = self.folded_before() {
            self.replace(range.start..self.cursor, "");
            return;
        }
        while self.cursor > 0
            && self.folded_before().is_none()
            && !self.text[..self.cursor].ends_with('\n')
            && self.text[..self.cursor].ends_with(char::is_whitespace)
        {
            self.backspace();
        }
        while self.cursor > 0
            && self.folded_before().is_none()
            && !self.text[..self.cursor].ends_with(char::is_whitespace)
        {
            self.backspace();
        }
    }
    pub fn delete_block_backward(&mut self) {
        let start = self.previous_block_boundary();
        self.replace(start..self.cursor, "");
    }
    fn previous_block_boundary(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }
        if let Some(range) = self.folded_before() {
            return range.start;
        }
        if self.text[..self.cursor].ends_with('\n') {
            return self.cursor - 1;
        }
        let end = self.cursor;
        let mut pos = self.cursor;
        let folded_end_at =
            |pastes: &[Paste], byte: usize| pastes.iter().any(|p| p.folded && p.end == byte);
        while pos > 0 && !folded_end_at(&self.pastes, pos) {
            let Some((idx, ch)) = self.text[..pos].char_indices().next_back() else {
                break;
            };
            if ch == '\n' || !ch.is_whitespace() {
                break;
            }
            pos = idx;
        }
        if pos == 0
            || (pos < end && (self.text[..pos].ends_with('\n') || folded_end_at(&self.pastes, pos)))
        {
            return pos;
        }
        while pos > 0 && !folded_end_at(&self.pastes, pos) {
            let Some((idx, ch)) = self.text[..pos].char_indices().next_back() else {
                break;
            };
            if ch.is_whitespace() || Self::is_word_char(ch) {
                break;
            }
            pos = idx;
        }
        while pos > 0 && !folded_end_at(&self.pastes, pos) {
            let Some((idx, ch)) = self.text[..pos].char_indices().next_back() else {
                break;
            };
            if !Self::is_word_char(ch) {
                break;
            }
            pos = idx;
        }
        pos
    }
    fn next_block_boundary(&self) -> usize {
        if let Some(range) = self.folded_after() {
            return range.end;
        }
        if self.text[self.cursor..].starts_with('\n') {
            return self.cursor + 1;
        }
        let start = self.cursor;
        let mut pos = start;
        let folded_start_at = |byte: usize| self.pastes.iter().any(|p| p.folded && p.start == byte);
        // Stop at line and folded-paste edges before consuming another block.
        while !folded_start_at(pos) {
            let Some(ch) = self.text[pos..].chars().next() else {
                return pos;
            };
            if ch == '\n' || !ch.is_whitespace() {
                break;
            }
            pos += ch.len_utf8();
        }
        if pos > start && (self.text[pos..].starts_with('\n') || folded_start_at(pos)) {
            return pos;
        }
        while !folded_start_at(pos) {
            let Some(ch) = self.text[pos..].chars().next() else {
                return pos;
            };
            if ch.is_whitespace() || Self::is_word_char(ch) {
                break;
            }
            pos += ch.len_utf8();
        }
        while !folded_start_at(pos) {
            let Some(ch) = self.text[pos..].chars().next() else {
                return pos;
            };
            if !Self::is_word_char(ch) {
                break;
            }
            pos += ch.len_utf8();
        }
        pos
    }
    pub fn expand_paste(&mut self) -> bool {
        if let Some(paste) = self
            .pastes
            .iter_mut()
            .find(|p| p.folded && p.start <= self.cursor && self.cursor <= p.end)
        {
            paste.folded = false;
            self.preferred_column = None;
            return true;
        }
        false
    }
    pub fn is_literal_before_cursor(&self) -> bool {
        self.cursor > 0
            && self
                .pastes
                .iter()
                .any(|p| p.start < self.cursor && self.cursor <= p.end)
    }
    fn label(&self, paste: &Paste) -> String {
        let chars = self.text[paste.start..paste.end].chars().count();
        format!("[Pasted Content {chars} chars]")
    }
    pub fn display_text(&self) -> String {
        let mut result = String::new();
        let mut at = 0;
        for paste in self.pastes.iter().filter(|p| p.folded) {
            if paste.start < at {
                continue;
            }
            result.push_str(&self.text[at..paste.start]);
            result.push_str(&self.label(paste));
            at = paste.end;
        }
        result.push_str(&self.text[at..]);
        result
    }
    fn mapped_layout(&self, width: usize) -> Layout {
        let width = width.max(1);
        let mut layout = Layout {
            rows: vec![String::new()],
            points: Vec::new(),
        };
        let mut at = 0;
        let mut col = 0;
        while at < self.text.len() {
            if let Some(paste) = self.pastes.iter().find(|p| p.folded && p.start == at) {
                let label = self.label(paste);
                if col > 0 && col + label.width() > width {
                    layout.rows.push(String::new());
                    col = 0;
                }
                layout.point(at, col);
                for ch in label.chars() {
                    layout.character(ch, &mut col, width);
                }
                at = paste.end;
                layout.point(at, col);
            } else {
                let ch = self.text[at..].chars().next().unwrap();
                let cells = if ch == '\t' {
                    2
                } else {
                    ch.width().unwrap_or(0)
                };
                if ch != '\n' && col + cells > width && col > 0 {
                    layout.rows.push(String::new());
                    col = 0;
                }
                layout.point(at, col);
                layout.character(ch, &mut col, width);
                at += ch.len_utf8();
                layout.point(at, col);
            }
        }
        if col >= width {
            layout.rows.push(String::new());
            col = 0;
        }
        layout.point(self.text.len(), col);
        layout
    }
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let layout = self.mapped_layout(width);
        let position = layout.position(self.cursor);
        (layout.rows, position)
    }
    pub fn vertical(&mut self, down: bool, width: usize) -> bool {
        let layout = self.mapped_layout(width);
        let (row, column) = layout.position(self.cursor);
        let goal = *self.preferred_column.get_or_insert(column);
        let mut target = row;
        loop {
            target = if down {
                if target + 1 >= layout.rows.len() {
                    return false;
                }
                target + 1
            } else {
                let Some(previous) = target.checked_sub(1) else {
                    return false;
                };
                previous
            };
            let candidates: Vec<_> = layout
                .points
                .iter()
                .filter(|(_, r, _)| *r == target)
                .collect();
            if let Some(point) = candidates
                .iter()
                .filter(|(_, _, col)| *col <= goal)
                .max_by_key(|(byte, _, col)| (*col, *byte))
                .or_else(|| candidates.iter().min_by_key(|(byte, _, col)| (*col, *byte)))
            {
                self.cursor = point.0;
                return true;
            }
            // A narrow terminal can wrap a block's label over several rows.
            // Its interior has no editable offsets, so move to the next edge.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn word_navigation_handles_paths_and_keeps_the_draft_unchanged() {
        let mut editor = Editor::default();
        editor.set("hello, src/tui/editor.rs   ".into());
        let draft = editor.draft();
        for prefix in [
            "hello, src/tui/editor.",
            "hello, src/tui/",
            "hello, src/",
            "hello, ",
            "",
            "",
        ] {
            editor.word_left();
            assert_eq!(&editor.text[..editor.cursor], prefix);
        }
        for prefix in [
            "hello",
            "hello,",
            "hello, src",
            "hello, src/tui",
            "hello, src/tui/editor",
            "hello, src/tui/editor.rs",
            "hello, src/tui/editor.rs   ",
            "hello, src/tui/editor.rs   ",
        ] {
            editor.word_right();
            assert_eq!(&editor.text[..editor.cursor], prefix);
        }
        assert_eq!(editor.draft(), draft);
        editor.set(String::new());
        editor.word_left();
        editor.word_right();
        assert_eq!(editor.cursor, 0);
    }

    #[test]
    fn word_navigation_preserves_unicode_and_stops_at_line_boundaries() {
        let mut editor = Editor::default();
        editor.set("\tcafé  naïve\n \trésumé ".into());
        editor.home();
        for prefix in [
            "\tcafé",
            "\tcafé  naïve",
            "\tcafé  naïve\n",
            "\tcafé  naïve\n \trésumé",
            "\tcafé  naïve\n \trésumé ",
        ] {
            editor.word_right();
            assert_eq!(&editor.text[..editor.cursor], prefix);
        }
        for prefix in [
            "\tcafé  naïve\n \t",
            "\tcafé  naïve\n",
            "\tcafé  naïve",
            "\tcafé  ",
            "\t",
            "",
        ] {
            editor.word_left();
            assert_eq!(&editor.text[..editor.cursor], prefix);
        }
    }

    #[test]
    fn word_navigation_treats_adjacent_folded_pastes_as_individual_blocks() {
        let mut editor = Editor::default();
        editor.insert("prefix");
        let first_start = editor.cursor;
        editor.paste(&"\u{754c}\u{8a9e} ".repeat(340));
        let second_start = editor.cursor;
        editor.paste(&"x\ny ".repeat(260));
        let tail_start = editor.cursor;
        editor.insert("tail");
        let end = editor.cursor;
        let draft = editor.draft();
        for position in [tail_start, second_start, first_start, 0] {
            editor.word_left();
            assert_eq!(editor.cursor, position);
        }
        for position in [first_start, second_start, tail_start, end] {
            editor.word_right();
            assert_eq!(editor.cursor, position);
        }
        assert_eq!(editor.draft(), draft);
        editor.cursor = first_start;
        assert!(editor.expand_paste());
        editor.word_right();
        assert_eq!(editor.cursor, first_start + "\u{754c}\u{8a9e}".len());
        editor.word_left();
        // Expanded paste joins the adjacent ordinary word.
        assert_eq!(editor.cursor, 0);
    }

    #[test]
    fn word_navigation_resets_the_vertical_preferred_column() {
        let mut editor = Editor::default();
        editor.set("abcd\nx\nwxyz".into());
        editor.cursor = 4;
        assert!(editor.vertical(true, 20));
        editor.word_left();
        assert_eq!(&editor.text[..editor.cursor], "abcd\n");
        assert!(editor.vertical(true, 20));
        assert_eq!(&editor.text[..editor.cursor], "abcd\nx\n");
    }

    #[test]
    fn vertical_preserves_visual_column_across_short_and_unicode_lines() {
        let mut editor = Editor::default();
        editor.set("abcd\n\u{754c}\n12345".into());
        editor.cursor = 4;
        assert!(editor.vertical(true, 20));
        assert_eq!(editor.cursor, "abcd\n\u{754c}".len());
        assert!(editor.vertical(true, 20));
        assert_eq!(&editor.text[..editor.cursor], "abcd\n\u{754c}\n1234");
        assert!(editor.vertical(false, 20));
        assert!(editor.vertical(false, 20));
        assert_eq!(editor.cursor, 4);
        assert!(!editor.vertical(false, 20));
    }
    #[test]
    fn soft_wrapped_rows_are_editable_before_history() {
        let mut editor = Editor::default();
        editor.set("ab\u{754c}cd界ef".into());
        assert_eq!(editor.layout(4).0, ["ab\u{754c}", "cd界", "ef"]);
        assert!(editor.vertical(false, 4));
        editor.insert("!");
        assert_eq!(editor.text, "ab\u{754c}cd!界ef");
        assert!(editor.vertical(true, 4));
        assert!(editor.text.is_char_boundary(editor.cursor));
    }
    #[test]
    fn paste_threshold_is_unicode_chars_and_blocks_keep_exact_payload() {
        let mut editor = Editor::default();
        assert!(editor.paste(&"\u{754c}".repeat(1000)));
        assert!(!editor.display_text().contains("Pasted Content"));
        editor.set(String::new());
        let payload = format!("\n{}\r\n@not-a-file\n", "\u{754c}".repeat(1001));
        assert!(editor.paste(&payload));
        let expected = payload.replace("\r\n", "\n");
        assert_eq!(editor.text, expected);
        assert!(editor.display_text().starts_with("[Pasted Content "));
        editor.left();
        assert_eq!(editor.cursor, 0);
        editor.right();
        assert_eq!(editor.cursor, expected.len());
        let restored: Draft =
            serde_json::from_str(&serde_json::to_string(&editor.draft()).unwrap()).unwrap();
        editor.set(String::new());
        editor.restore(restored);
        assert_eq!(editor.text, expected);
        assert!(editor.expand_paste());
        assert_eq!(editor.display_text(), expected);
    }
    #[test]
    fn block_deletion_does_not_rewrite_user_typed_marker_text() {
        let mut editor = Editor::default();
        editor.insert("[Pasted Content 1001 chars] ");
        editor.paste(&"x".repeat(1001));
        editor.insert(" tail");
        for _ in 0..5 {
            editor.left();
        }
        editor.backspace();
        assert_eq!(editor.text, "[Pasted Content 1001 chars]  tail");
        assert!(editor.draft().literal_ranges().is_empty());
    }
    #[test]
    fn oversized_paste_keeps_existing_draft_and_metadata() {
        let mut editor = Editor::default();
        editor.paste(&"x".repeat(1001));
        let before = editor.draft();
        assert!(!editor.paste(&"y".repeat(MAX_INPUT_BYTES)));
        assert_eq!(editor.draft(), before);
        assert_eq!(editor.cursor, 1001);
    }
    #[test]
    fn delete_block_backward_deletes_words_paths_newlines_and_paste_blocks() {
        let mut editor = Editor::default();
        editor.set("hello, src/tui/editor.rs   ".into());
        editor.delete_block_backward();
        assert_eq!(editor.text, "hello, src/tui/editor.");
        editor.delete_block_backward();
        assert_eq!(editor.text, "hello, src/tui/");
        editor.delete_block_backward();
        assert_eq!(editor.text, "hello, src/");
        editor.delete_block_backward();
        assert_eq!(editor.text, "hello, ");
        editor.delete_block_backward();
        assert_eq!(editor.text, "");

        editor.set("café   naïve\n   résumé".into());
        editor.delete_block_backward();
        assert_eq!(editor.text, "café   naïve\n   ");
        editor.delete_block_backward();
        assert_eq!(editor.text, "café   naïve\n");
        editor.delete_block_backward();
        assert_eq!(editor.text, "café   naïve");
        editor.delete_block_backward();
        assert_eq!(editor.text, "café   ");
        editor.delete_block_backward();
        assert_eq!(editor.text, "");

        editor.set("prefix ".into());
        assert!(editor.paste(&"x".repeat(1001)));
        editor.insert("   tail");
        editor.delete_block_backward();
        assert_eq!(&editor.text[editor.text.len() - 3..], "   ");
        editor.delete_block_backward();
        assert!(editor
            .display_text()
            .ends_with("[Pasted Content 1001 chars]"));
        editor.delete_block_backward();
        assert_eq!(editor.text, "prefix ");
        editor.delete_block_backward();
        assert_eq!(editor.text, "");
    }
}
