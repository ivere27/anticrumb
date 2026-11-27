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

pub(super) const INSPECTOR_MIN_WIDTH_PCT: u8 = 35;
pub(super) const INSPECTOR_MAX_WIDTH_PCT: u8 = 70;
pub(super) const INSPECTOR_MIN_LEFT_COLS: u16 = 28;
pub(super) const INSPECTOR_MIN_RIGHT_COLS: u16 = 44;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectorMode {
    #[default]
    Inline,
    Side,
    Off,
}

impl InspectorMode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Inline => "Inline",
            Self::Side => "Side",
            Self::Off => "Off",
        }
    }

    pub(super) fn cycle(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Inline, true) | (Self::Off, false) => Self::Side,
            (Self::Side, true) | (Self::Inline, false) => Self::Off,
            (Self::Off, true) | (Self::Side, false) => Self::Inline,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterTurnPolicy {
    #[default]
    Fit,
    Collapse,
    Keep,
}

impl AfterTurnPolicy {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Collapse => "Collapse",
            Self::Keep => "Keep",
        }
    }

    pub(super) fn cycle(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Fit, true) | (Self::Keep, false) => Self::Collapse,
            (Self::Collapse, true) | (Self::Fit, false) => Self::Keep,
            (Self::Keep, true) | (Self::Collapse, false) => Self::Fit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct InspectorConfig {
    pub mode: InspectorMode,
    pub width_pct: u8,
    pub after_turn: AfterTurnPolicy,
    pub follow: bool,
    pub min_split_width: u16,
}

impl Default for InspectorConfig {
    fn default() -> Self {
        Self {
            mode: InspectorMode::Inline,
            width_pct: 55,
            after_turn: AfterTurnPolicy::Fit,
            follow: true,
            min_split_width: 100,
        }
    }
}

impl InspectorConfig {
    pub fn normalized(mut self) -> Self {
        self.width_pct = self
            .width_pct
            .clamp(INSPECTOR_MIN_WIDTH_PCT, INSPECTOR_MAX_WIDTH_PCT);
        let floor = INSPECTOR_MIN_LEFT_COLS + 1 + INSPECTOR_MIN_RIGHT_COLS;
        self.min_split_width = self.min_split_width.clamp(floor, 400);
        self
    }

    pub(super) fn summary(&self) -> String {
        format!(
            "{} · {}% · after-turn {} · {}",
            self.mode.label().to_ascii_lowercase(),
            self.width_pct,
            self.after_turn.label().to_ascii_lowercase(),
            if self.follow { "follow" } else { "manual" }
        )
    }
}

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
    pub inspector: InspectorConfig,
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
            inspector: InspectorConfig::default(),
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ColSpan {
    pub start: u16,
    pub len: u16,
}

impl ColSpan {
    pub(super) fn end(self) -> u16 {
        self.start.saturating_add(self.len)
    }

    pub(super) fn contains(self, col: u16) -> bool {
        col >= self.start && col < self.end()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Rect {
    pub col: ColSpan,
    pub row: RowSpan,
}

impl Rect {
    pub(super) fn contains(self, col: u16, row: u16) -> bool {
        self.col.contains(col) && self.row.contains(row)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HorizontalSplit {
    pub left: ColSpan,
    pub divider_x: u16,
    pub right: ColSpan,
}

impl HorizontalSplit {
    pub(super) fn compute(total_width: u16, config: &InspectorConfig) -> Option<Self> {
        if config.mode == InspectorMode::Off {
            return None;
        }
        let min_total = config
            .min_split_width
            .max(INSPECTOR_MIN_LEFT_COLS + 1 + INSPECTOR_MIN_RIGHT_COLS);
        if total_width < min_total {
            return None;
        }
        let usable = total_width.saturating_sub(1);
        let pct = u32::from(
            config
                .width_pct
                .clamp(INSPECTOR_MIN_WIDTH_PCT, INSPECTOR_MAX_WIDTH_PCT),
        );
        let desired_right = ((u32::from(total_width) * pct + 50) / 100) as u16;
        let max_right = usable.saturating_sub(INSPECTOR_MIN_LEFT_COLS);
        let right_len = desired_right.clamp(INSPECTOR_MIN_RIGHT_COLS, max_right);
        let left_len = usable.saturating_sub(right_len);
        Some(Self {
            left: ColSpan {
                start: 0,
                len: left_len,
            },
            divider_x: left_len,
            right: ColSpan {
                start: left_len.saturating_add(1),
                len: right_len,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameLayout {
    pub width: u16,
    pub height: u16,
    pub header: RowSpan,
    pub transcript: RowSpan,
    pub transcript_left: ColSpan,
    pub inspector_rect: Option<Rect>,
    pub divider_x: Option<u16>,
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

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn compute(
        config: &LayoutConfig,
        width: u16,
        height: u16,
        editor_rows: usize,
        editor_cursor: (usize, usize),
        completion_rows: usize,
        subagent_count: usize,
    ) -> Option<Self> {
        Self::compute_with_inspector(
            config,
            width,
            height,
            editor_rows,
            editor_cursor,
            completion_rows,
            subagent_count,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn compute_with_inspector(
        config: &LayoutConfig,
        width: u16,
        height: u16,
        editor_rows: usize,
        editor_cursor: (usize, usize),
        completion_rows: usize,
        subagent_count: usize,
        has_tool_blocks: bool,
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

        let side_split = if config.inspector.mode == InspectorMode::Side && has_tool_blocks {
            HorizontalSplit::compute(width, &config.inspector)
        } else {
            None
        };
        let (transcript_left, divider_x, inspector_rect) = if let Some(split) = side_split {
            (
                split.left,
                Some(split.divider_x),
                Some(Rect {
                    col: split.right,
                    row: transcript,
                }),
            )
        } else {
            (
                ColSpan {
                    start: 0,
                    len: width,
                },
                None,
                None,
            )
        };

        Some(Self {
            width,
            height,
            header,
            transcript,
            transcript_left,
            inspector_rect,
            divider_x,
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

    #[test]
    fn horizontal_split_and_inspector_layout_invariants() {
        let mut cfg = InspectorConfig::default();
        assert_eq!(cfg.mode, InspectorMode::Inline);
        assert_eq!(cfg.width_pct, 55);
        assert_eq!(cfg.min_split_width, 100);

        // Below min_split_width -> None.
        assert!(HorizontalSplit::compute(80, &cfg).is_none());
        assert!(HorizontalSplit::compute(99, &cfg).is_none());

        // Off mode -> always None regardless of width.
        cfg.mode = InspectorMode::Off;
        assert!(HorizontalSplit::compute(200, &cfg).is_none());

        // Across widths 100..=240 and width_pct 0..=100, strict invariants hold.
        for mode in [InspectorMode::Inline, InspectorMode::Side] {
            for pct in [0u8, 30, 35, 50, 55, 70, 95] {
                let c = InspectorConfig {
                    mode,
                    width_pct: pct,
                    ..InspectorConfig::default()
                };
                for width in [100u16, 101, 120, 160, 240] {
                    let split = HorizontalSplit::compute(width, &c).expect("split active");
                    assert_eq!(split.left.start, 0);
                    assert_eq!(split.divider_x, split.left.end());
                    assert_eq!(split.right.start, split.divider_x + 1);
                    assert_eq!(split.right.end(), width);
                    assert!(split.left.len >= INSPECTOR_MIN_LEFT_COLS);
                    assert!(split.right.len >= INSPECTOR_MIN_RIGHT_COLS);
                    assert_eq!(split.left.len + 1 + split.right.len, width);
                }
            }
        }

        // FrameLayout::compute_with_inspector:
        // 1. Inline mode never allocates top-level inspector_rect (splits per block).
        let inline_cfg = LayoutConfig::default();
        let inline =
            FrameLayout::compute_with_inspector(&inline_cfg, 120, 26, 1, (0, 0), 0, 0, true)
                .unwrap();
        assert!(inline.inspector_rect.is_none());
        assert!(inline.divider_x.is_none());
        assert_eq!(inline.transcript_left, ColSpan { start: 0, len: 120 });

        // 2. Side mode with 0 tool blocks keeps 100% full width (no wasted 55% blank pane).
        let mut side_cfg = LayoutConfig::default();
        side_cfg.inspector.mode = InspectorMode::Side;
        let side_empty =
            FrameLayout::compute_with_inspector(&side_cfg, 120, 26, 1, (0, 0), 0, 0, false)
                .unwrap();
        assert!(side_empty.inspector_rect.is_none());
        assert!(side_empty.divider_x.is_none());
        assert_eq!(side_empty.transcript_left, ColSpan { start: 0, len: 120 });

        // 3. Side mode with tool blocks splits Band::Transcript cleanly.
        let side_active =
            FrameLayout::compute_with_inspector(&side_cfg, 120, 26, 1, (0, 0), 0, 0, true).unwrap();
        let insp = side_active.inspector_rect.expect("side inspector rect");
        let div = side_active.divider_x.expect("side divider");
        assert_eq!(side_active.transcript_left.start, 0);
        assert_eq!(side_active.transcript_left.end(), div);
        assert_eq!(insp.col.start, div + 1);
        assert_eq!(insp.col.end(), 120);
        assert_eq!(insp.row.start, side_active.transcript.start);
        assert_eq!(insp.row.len, side_active.transcript.len);
        assert_eq!(side_active.transcript_left.len + 1 + insp.col.len, 120);

        // 4. Side mode below min_split_width falls back to single column.
        let side_narrow =
            FrameLayout::compute_with_inspector(&side_cfg, 80, 26, 1, (0, 0), 0, 0, true).unwrap();
        assert!(side_narrow.inspector_rect.is_none());
        assert_eq!(side_narrow.transcript_left, ColSpan { start: 0, len: 80 });
    }
}
