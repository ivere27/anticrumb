//! Declarative vertical dock layout and row-span coordinates for the TUI.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Band {
    Header,
    Transcript,
    Hint,
    Composer,
    Completion,
    Subagents,
    Footer,
}

/// Default top-to-bottom dock order of UI regions on screen.
pub(super) const DOCK_ORDER: [Band; 7] = [
    Band::Header,
    Band::Transcript,
    Band::Hint,
    Band::Composer,
    Band::Completion,
    Band::Subagents,
    Band::Footer,
];

/// Runtime-mutable layout specification driving `FrameLayout::compute`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LayoutConfig {
    pub dock_order: [Band; 7],
    pub show_header: bool,
    pub show_hint: bool,
    pub show_footer: bool,
    pub min_transcript_rows: usize,
    pub max_composer_rows: usize,
    pub max_completion_rows: usize,
    pub max_subagent_rows: usize,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            dock_order: DOCK_ORDER,
            show_header: false,
            show_hint: true,
            show_footer: true,
            min_transcript_rows: 2,
            max_composer_rows: 5,
            max_completion_rows: 8,
            max_subagent_rows: 8,
        }
    }
}

impl LayoutConfig {
    pub(super) fn subagent_rows(&self, height: usize, count: usize) -> u16 {
        let non_transcript_base = usize::from(self.show_header)
            + usize::from(self.show_hint)
            + usize::from(self.show_footer)
            + 1
            + self.min_transcript_rows;
        if count == 0 || self.max_subagent_rows == 0 || height <= non_transcript_base + 1 {
            0
        } else {
            count
                .min(height.saturating_sub(non_transcript_base + 1))
                .min(self.max_subagent_rows) as u16
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RowSpan {
    pub start: u16,
    pub len: u16,
}

impl RowSpan {
    pub(super) fn end(self) -> u16 {
        self.start.saturating_add(self.len)
    }

    pub(super) fn contains(self, row: u16) -> bool {
        row >= self.start && row < self.end()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameLayout {
    pub width: u16,
    pub height: u16,
    pub header: RowSpan,
    pub transcript: RowSpan,
    pub hint: RowSpan,
    pub composer: RowSpan,
    pub completion: RowSpan,
    pub subagents: RowSpan,
    pub footer: RowSpan,
    pub editor_from: usize,
    pub cursor: (u16, u16),
}

impl FrameLayout {
    pub(super) fn editor_width(terminal_width: usize) -> usize {
        terminal_width.saturating_sub(4)
    }

    pub(super) fn compute(
        config: &LayoutConfig,
        width: u16,
        height: u16,
        editor_rows: usize,
        editor_cursor: (usize, usize),
        completion_rows: usize,
        subagent_count: usize,
    ) -> Option<Self> {
        if width < 12 || height < 6 {
            return None;
        }
        let h = usize::from(height);
        let w = usize::from(width);
        let subagents_len = config.subagent_rows(h, subagent_count);

        let fixed_base: usize = config
            .dock_order
            .iter()
            .filter_map(|band| match band {
                Band::Header => Some(usize::from(config.show_header)),
                Band::Hint => Some(usize::from(config.show_hint)),
                Band::Footer => Some(usize::from(config.show_footer)),
                Band::Subagents => Some(usize::from(subagents_len)),
                Band::Transcript | Band::Composer | Band::Completion => None,
            })
            .sum();

        // Keep at least `min_transcript_rows` for the transcript area at minimum supported height (6).
        let min_transcript = config.min_transcript_rows.max(1);
        let available_for_completion = h.saturating_sub(fixed_base + min_transcript + 1);
        let completion_len_usize = completion_rows
            .min(config.max_completion_rows)
            .min(available_for_completion);
        let completion_len = completion_len_usize as u16;
        let fixed_excluding_transcript_and_composer = fixed_base + completion_len_usize;

        let max_composer_rows =
            h.saturating_sub(fixed_excluding_transcript_and_composer + min_transcript);
        let composer_len_usize = editor_rows
            .max(1)
            .min(config.max_composer_rows.max(1))
            .min(max_composer_rows)
            .max(1);
        let composer_len = composer_len_usize as u16;
        let transcript_len =
            h.saturating_sub(fixed_excluding_transcript_and_composer + composer_len_usize) as u16;

        let mut header = RowSpan::default();
        let mut transcript = RowSpan::default();
        let mut hint = RowSpan::default();
        let mut composer = RowSpan::default();
        let mut completion = RowSpan::default();
        let mut subagents = RowSpan::default();
        let mut footer = RowSpan::default();

        let mut y = 0u16;
        for band in config.dock_order {
            let len = match band {
                Band::Header => u16::from(config.show_header),
                Band::Hint => u16::from(config.show_hint),
                Band::Footer => u16::from(config.show_footer),
                Band::Transcript => transcript_len,
                Band::Composer => composer_len,
                Band::Completion => completion_len,
                Band::Subagents => subagents_len,
            };
            let span = RowSpan { start: y, len };
            match band {
                Band::Header => header = span,
                Band::Transcript => transcript = span,
                Band::Hint => hint = span,
                Band::Composer => composer = span,
                Band::Completion => completion = span,
                Band::Subagents => subagents = span,
                Band::Footer => footer = span,
            }
            y = span.end();
        }

        let (cursor_row, cursor_col) = editor_cursor;
        let editor_rows_clamped = editor_rows.max(composer_len_usize);
        let editor_from = cursor_row
            .saturating_sub(composer_len_usize - 1)
            .min(editor_rows_clamped - composer_len_usize);
        let cursor = (
            (cursor_col + 2).min(w - 1) as u16,
            composer.start + cursor_row.saturating_sub(editor_from) as u16,
        );

        Some(Self {
            width,
            height,
            header,
            transcript,
            hint,
            composer,
            completion,
            subagents,
            footer,
            editor_from,
            cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_terminals_skip_layout_instead_of_underflowing() {
        let config = LayoutConfig::default();
        assert!(FrameLayout::compute(&config, 11, 26, 1, (0, 0), 0, 0).is_none());
        assert!(FrameLayout::compute(&config, 80, 5, 1, (0, 0), 0, 0).is_none());
    }

    #[test]
    fn dock_bands_tile_entire_height_without_gaps_or_overlaps() {
        for show_header in [false, true] {
            for show_footer in [true, false] {
                let config = LayoutConfig {
                    show_header,
                    show_footer,
                    ..LayoutConfig::default()
                };
                for height in [6u16, 7, 10, 24, 26, 50] {
                    for subagents in [0usize, 1, 2, 10] {
                        for completion_rows in [0usize, 3, 8] {
                            for editor_rows in [1usize, 3, 7] {
                                let layout = FrameLayout::compute(
                                    &config,
                                    100,
                                    height,
                                    editor_rows,
                                    (editor_rows - 1, 4),
                                    completion_rows,
                                    subagents,
                                )
                                .expect("valid layout");
                                let spans = [
                                    layout.header,
                                    layout.transcript,
                                    layout.hint,
                                    layout.composer,
                                    layout.completion,
                                    layout.subagents,
                                    layout.footer,
                                ];
                                assert_eq!(spans[0].start, 0);
                                for pair in spans.windows(2) {
                                    assert_eq!(pair[0].end(), pair[1].start);
                                }
                                assert_eq!(layout.footer.end(), height);
                                assert!(layout.composer.contains(layout.cursor.1));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn standard_26_row_geometry_keeps_expected_row_indices() {
        let config = LayoutConfig::default();
        let layout = FrameLayout::compute(&config, 80, 26, 1, (0, 0), 0, 0).unwrap();
        assert_eq!(layout.header, RowSpan { start: 0, len: 0 });
        assert_eq!(layout.transcript, RowSpan { start: 0, len: 23 });
        assert_eq!(layout.hint, RowSpan { start: 23, len: 1 });
        assert_eq!(layout.composer, RowSpan { start: 24, len: 1 });
        assert_eq!(layout.completion, RowSpan { start: 25, len: 0 });
        assert_eq!(layout.subagents, RowSpan { start: 25, len: 0 });
        assert_eq!(layout.footer, RowSpan { start: 25, len: 1 });
        assert_eq!(layout.cursor, (2, 24));
    }

    #[test]
    fn completion_reserves_space_between_composer_and_footer() {
        let config = LayoutConfig::default();
        let layout = FrameLayout::compute(&config, 100, 26, 1, (0, 0), 8, 0).unwrap();
        assert_eq!(layout.transcript, RowSpan { start: 0, len: 15 });
        assert_eq!(layout.hint, RowSpan { start: 15, len: 1 });
        assert_eq!(layout.composer, RowSpan { start: 16, len: 1 });
        assert_eq!(layout.completion, RowSpan { start: 17, len: 8 });
        assert_eq!(layout.subagents, RowSpan { start: 25, len: 0 });
        assert_eq!(layout.footer, RowSpan { start: 25, len: 1 });
        assert_eq!(layout.cursor, (2, 16));
    }

    #[test]
    fn subagents_reserve_space_between_composer_and_footer() {
        let config = LayoutConfig::default();
        let layout = FrameLayout::compute(&config, 100, 26, 1, (0, 0), 0, 2).unwrap();
        assert_eq!(layout.transcript, RowSpan { start: 0, len: 21 });
        assert_eq!(layout.hint, RowSpan { start: 21, len: 1 });
        assert_eq!(layout.composer, RowSpan { start: 22, len: 1 });
        assert_eq!(layout.completion, RowSpan { start: 23, len: 0 });
        assert_eq!(layout.subagents, RowSpan { start: 23, len: 2 });
        assert_eq!(layout.footer, RowSpan { start: 25, len: 1 });
    }

    #[test]
    fn runtime_layout_config_toggles_bands_and_reorders_dock() {
        let mut config = LayoutConfig {
            show_header: true,
            show_footer: false,
            dock_order: [
                Band::Footer,
                Band::Header,
                Band::Transcript,
                Band::Subagents,
                Band::Hint,
                Band::Composer,
                Band::Completion,
            ],
            ..LayoutConfig::default()
        };
        let layout = FrameLayout::compute(&config, 100, 26, 1, (0, 0), 0, 2).unwrap();
        assert_eq!(layout.footer, RowSpan { start: 0, len: 0 });
        assert_eq!(layout.header, RowSpan { start: 0, len: 1 });
        assert_eq!(layout.transcript, RowSpan { start: 1, len: 21 });
        assert_eq!(layout.subagents, RowSpan { start: 22, len: 2 });
        assert_eq!(layout.hint, RowSpan { start: 24, len: 1 });
        assert_eq!(layout.composer, RowSpan { start: 25, len: 1 });
        assert_eq!(layout.completion, RowSpan { start: 26, len: 0 });

        config.show_header = false;
        let hidden = FrameLayout::compute(&config, 100, 26, 1, (0, 0), 0, 2).unwrap();
        assert_eq!(hidden.header, RowSpan { start: 0, len: 0 });
        assert_eq!(hidden.transcript, RowSpan { start: 0, len: 22 });
    }
}
