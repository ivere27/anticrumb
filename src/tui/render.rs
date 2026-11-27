//! Terminal frame, status bar, and subagent banner rendering.
use super::{
    commands::{Selection, ShortcutSection, SHORTCUT_QUICK_COMMANDS, SHORTCUT_SECTIONS},
    debug_log::DebugLogger,
    inspector::{self, BlockId, BlockRowKind, PaintedInspectorPane, PaintedToolBlock},
    layout::{ColSpan, FrameLayout, Rect, RowSpan},
    markdown::{self, Face, Span},
    reducer::{PendingToolApproval, SelectionTarget, State, SubagentView},
    transcript::{clip, BlockSpan},
    usage,
};
use crate::domain::*;
use crossterm::{
    cursor, queue,
    style::{self, Stylize},
    terminal::{self, ClearType},
};
use std::io::Write;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn spans_width(parts: &[(String, Face)]) -> usize {
    parts
        .iter()
        .flat_map(|(text, _)| markdown::strip_controls(text, false))
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

fn clip_spans(parts: Vec<(String, Face)>, max_width: usize) -> Vec<(String, Face)> {
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (text, face) in parts {
        let mut kept = String::new();
        for ch in markdown::strip_controls(&text, false) {
            let w = ch.width().unwrap_or(0);
            if used + w > max_width {
                truncated = true;
                break;
            }
            used += w;
            kept.push(ch);
        }
        if !kept.is_empty() {
            out.push((kept, face));
        }
        if truncated || used >= max_width {
            break;
        }
    }
    if truncated {
        while let Some((last, face)) = out.last() {
            if *face == Face::MUTED && last.chars().all(|c| c == ' ' || c == '·') {
                out.pop();
            } else {
                break;
            }
        }
    }
    out
}

pub(super) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const SHORTCUTS_TWO_COL_MIN_WIDTH: usize = 96;

fn shortcut_section_lines(section: &ShortcutSection, col_w: usize) -> Vec<markdown::Line> {
    if col_w == 0 {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(section.items.len() + 1);
    let mut heading = Vec::new();
    let title = clip(&format!(" {}", section.title), col_w);
    let title_w = title.width();
    if !title.is_empty() {
        heading.push(Span {
            text: title,
            face: Face::HEADING,
        });
    }
    if let Some(sub) = section.subtitle {
        let sub_text = format!("  ({sub})");
        let rem = col_w.saturating_sub(title_w);
        if rem >= sub_text.width() {
            heading.push(Span {
                text: sub_text,
                face: Face::MUTED,
            });
        }
    }
    rows.push(heading);

    let key_face = if section.highlight_key {
        Face::HIGHLIGHT_BOLD
    } else {
        Face::ACCENT_BOLD
    };
    let key_col = if col_w >= 42 {
        15
    } else {
        12.min(col_w.saturating_sub(4))
    };
    for &(key, desc) in section.items {
        let mut line = Vec::new();
        let mut used = 0usize;
        if col_w >= 2 {
            line.push(Span {
                text: "  ".into(),
                face: Face::TEXT,
            });
            used = 2;
        }
        let key_budget = col_w.saturating_sub(used);
        let clipped_key = clip(key, key_budget);
        let kw = clipped_key.width();
        if !clipped_key.is_empty() {
            line.push(Span {
                text: clipped_key,
                face: key_face,
            });
            used += kw;
        }
        let pad_len = key_col
            .saturating_sub(kw)
            .saturating_add(1)
            .min(col_w.saturating_sub(used));
        if pad_len > 0 {
            line.push(Span {
                text: " ".repeat(pad_len),
                face: Face::TEXT,
            });
            used += pad_len;
        }
        let desc_budget = col_w.saturating_sub(used);
        if desc_budget > 0 {
            let clipped_desc = clip(desc, desc_budget);
            if !clipped_desc.is_empty() {
                line.push(Span {
                    text: clipped_desc,
                    face: Face::TEXT,
                });
            }
        }
        rows.push(line);
    }
    rows
}

fn shortcuts_panel_rows(width: usize) -> Vec<markdown::Line> {
    if width == 0 {
        return Vec::new();
    }
    if width >= SHORTCUTS_TWO_COL_MIN_WIDTH {
        let left_w = width.saturating_sub(1) / 2;
        let right_w = width.saturating_sub(left_w + 1);
        let mut left_rows = Vec::new();
        for (i, section) in SHORTCUT_SECTIONS[..2].iter().enumerate() {
            if i > 0 {
                left_rows.push(Vec::new());
            }
            left_rows.extend(shortcut_section_lines(section, left_w));
        }
        let mut right_rows = Vec::new();
        for (i, section) in SHORTCUT_SECTIONS[2..].iter().enumerate() {
            if i > 0 {
                right_rows.push(Vec::new());
            }
            right_rows.extend(shortcut_section_lines(section, right_w));
        }
        right_rows.push(Vec::new());
        right_rows.extend(shortcut_section_lines(&SHORTCUT_QUICK_COMMANDS, right_w));

        let total = left_rows.len().max(right_rows.len());
        let mut combined = Vec::with_capacity(total);
        for i in 0..total {
            let mut row = Vec::new();
            let mut lw = 0usize;
            if let Some(l) = left_rows.get(i) {
                for span in l {
                    lw += span.text.width();
                    row.push(span.clone());
                }
            }
            if lw < left_w {
                row.push(Span {
                    text: " ".repeat(left_w - lw),
                    face: Face::TEXT,
                });
            }
            row.push(Span {
                text: "│".into(),
                face: Face::SUBTLE,
            });
            if let Some(r) = right_rows.get(i) {
                row.extend(r.iter().cloned());
            }
            combined.push(row);
        }
        combined
    } else {
        let mut rows = Vec::new();
        for (i, section) in SHORTCUT_SECTIONS.iter().enumerate() {
            if i > 0 {
                rows.push(Vec::new());
            }
            rows.extend(shortcut_section_lines(section, width));
        }
        rows.push(Vec::new());
        rows.extend(shortcut_section_lines(&SHORTCUT_QUICK_COMMANDS, width));
        rows
    }
}

pub(super) fn shortcuts_panel_metrics(width: usize, avail_rows: usize) -> (usize, usize) {
    let total = shortcuts_panel_rows(width).len();
    let visible = total.min(avail_rows.saturating_sub(1));
    (total, visible)
}

impl State {
    fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[(self.started.elapsed().as_millis() / 120 % 10) as usize]
    }

    #[cfg(test)]
    pub(super) fn status_text(&self, width: usize) -> String {
        self.status_spans(width)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    pub(super) fn status_spans(&self, width: usize) -> Vec<(String, Face)> {
        let path = if width >= 130 {
            self.options.workspace.display().to_string()
        } else {
            self.options
                .workspace
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let quota = self
            .web_usage
            .as_ref()
            .filter(|usage| {
                let account_matches = usage["account_id"] == self.options.account;
                let browser = self.status["browser_connection"]["server_id"]
                    .as_str()
                    .filter(|id| !id.is_empty());
                let same_server = browser.is_none_or(|id| usage["server_id"] == id);
                account_matches && same_server
            })
            .map(|value| {
                let mut text = usage::footer(value);
                if self.web_usage_stale {
                    text.push_str(" (stale)");
                }
                let qface = if self.web_usage_stale || text.contains("≈0%") || text.contains(":0%")
                {
                    Face::DANGER_BOLD
                } else {
                    Face::HIGHLIGHT
                };
                (text, qface)
            });

        let mut parts: Vec<(String, Face)> = Vec::new();
        let push_sep = |parts: &mut Vec<(String, Face)>| {
            if !parts.is_empty() {
                parts.push((" · ".into(), Face::MUTED));
            }
        };
        if let Some((q, qface)) = quota {
            parts.push((q, qface));
        }

        let cleanup = &self.status["chat_cleanup"];
        let active =
            cleanup["pending"].as_u64().unwrap_or(0) + cleanup["running"].as_u64().unwrap_or(0);
        let failed = cleanup["failed"].as_u64().unwrap_or(0);
        if active > 0 {
            push_sep(&mut parts);
            parts.push((format!("Cleanup:{active}"), Face::ACCENT_BOLD));
        }
        if failed > 0 {
            push_sep(&mut parts);
            parts.push((format!("Cleanup:!{failed}"), Face::DANGER_BOLD));
        }
        match self.web_history_status["state"].as_str() {
            Some("pending" | "running") => {
                push_sep(&mut parts);
                parts.push((
                    format!(
                        "History:{} deleted",
                        self.web_history_status["deleted"].as_u64().unwrap_or(0)
                    ),
                    Face::ACCENT_BOLD,
                ));
            }
            Some("failed" | "interrupted") => {
                push_sep(&mut parts);
                parts.push(("History:!".into(), Face::DANGER_BOLD));
            }
            _ => {}
        }

        let daemon = self.daemon_label();
        let daemon_face = if self.status_disconnected {
            Face::DANGER_BOLD
        } else {
            match daemon {
                "remote" | "local" => Face::MUTED,
                "checking" => Face::ACCENT,
                _ => Face::DANGER_BOLD,
            }
        };
        let daemon_text = match (daemon, self.daemon_remote_addr()) {
            ("remote" | "remote/offline", Some(addr)) => format!("D:{daemon}({addr})"),
            _ => format!("D:{daemon}"),
        };
        let priority_w = spans_width(&parts);

        push_sep(&mut parts);
        parts.push((
            format!(
                "M:{}",
                clip(self.model_label(), if width < 100 { 12 } else { 24 })
            ),
            Face::ACCENT,
        ));
        parts.push((" · ".into(), Face::MUTED));
        parts.push((format!("E:{}", clip(self.thinking_label(), 8)), Face::MUTED));
        parts.push((" · ".into(), Face::MUTED));
        if !self.options.agent {
            parts.push(("CHAT".into(), Face::ACCENT));
        } else if self.options.yolo {
            parts.push(("YOLO".into(), Face::HIGHLIGHT_BOLD));
        } else {
            parts.push(("AGENT".into(), Face::HIGHLIGHT_BOLD));
        }
        if self.options.temporary {
            parts.push((" · ".into(), Face::MUTED));
            parts.push(("TEMP".into(), Face::CODE_BOLD));
        }
        if self.debug.level > 0 {
            parts.push((" · ".into(), Face::MUTED));
            parts.push((
                format!(
                    "DBG:{}({})",
                    self.debug.level,
                    DebugLogger::label(self.debug.level)
                ),
                Face::ACCENT_BOLD,
            ));
        }
        parts.push((" · ".into(), Face::MUTED));
        parts.push((
            format!("W:{}", clip(&path, if width < 100 { 10 } else { 32 })),
            Face::TEXT,
        ));
        parts.push((" · ".into(), Face::MUTED));
        parts.push((
            format!("U:{}/{}c T:?", self.in_chars, self.out_chars),
            Face::MUTED,
        ));
        let queued = self.status["queued"].as_u64().unwrap_or(0);
        parts.push((" · ".into(), Face::MUTED));
        parts.push((
            format!("Q:{queued}"),
            if queued > 0 {
                Face::ACCENT_BOLD
            } else {
                Face::MUTED
            },
        ));
        let secs = if self.busy() {
            self.started.elapsed().as_secs()
        } else {
            0
        };
        parts.push((" · ".into(), Face::MUTED));
        parts.push((
            format!("{secs}s"),
            if self.busy() {
                Face::ACCENT
            } else {
                Face::MUTED
            },
        ));
        let acct_state = self
            .account()
            .and_then(|a| a["state"].as_str())
            .unwrap_or("connecting");
        let acct_face = match acct_state {
            "ready" => Face::HIGHLIGHT,
            "awaiting_human" | "auth_required" => Face::DANGER_BOLD,
            _ => Face::MUTED,
        };
        let acct_label = if self.provider_id() == crate::provider::DEFAULT_PROVIDER_ID {
            format!("{}:{acct_state}", self.options.account)
        } else {
            format!(
                "{}/{}:{acct_state}",
                self.provider_id(),
                self.options.account
            )
        };
        parts.push((" · ".into(), Face::MUTED));
        parts.push((acct_label, acct_face));

        if width == 0 {
            return Vec::new();
        }
        let right_text = clip(&daemon_text, width);
        let right_w = right_text.width();
        let reserved_right = right_w + 2;
        if priority_w > 0 && priority_w + reserved_right > width {
            let left_max = priority_w.min(width);
            let mut left = clip_spans(parts.clone(), left_max);
            let left_w = spans_width(&left);
            if left_w + 2 + right_w <= width {
                let pad = width - left_w - right_w;
                left.push((" ".repeat(pad), Face::MUTED));
                left.push((right_text, daemon_face));
                return left;
            }
            return clip_spans(parts, width);
        }
        let left_max = width.saturating_sub(reserved_right);
        if left_max == 0 {
            return vec![(right_text, daemon_face)];
        }
        let full_left_w = spans_width(&parts);
        let chosen_hint: Option<&'static str> = if self.show_footer_shortcut_hint() {
            const CANDIDATES: [&str; 3] = ["? for shortcuts", "? shortcuts", "? help"];
            if full_left_w <= left_max {
                let spare = width.saturating_sub(full_left_w + right_w);
                CANDIDATES
                    .into_iter()
                    .find(|label| spare >= label.len() + 4)
            } else {
                let min_left = priority_w.max(44);
                CANDIDATES.into_iter().find(|label| {
                    let reserve = label.len() + 4 + right_w;
                    width >= reserve && width - reserve >= min_left
                })
            }
        } else {
            None
        };
        let effective_left_max = match chosen_hint {
            Some(label) => width.saturating_sub(right_w + label.len() + 4),
            None => left_max,
        };
        let mut left = clip_spans(parts, effective_left_max);
        let left_w = spans_width(&left);
        if let Some(label) = chosen_hint {
            let pad_before_hint = width.saturating_sub(left_w + label.len() + 2 + right_w);
            if pad_before_hint > 0 {
                left.push((" ".repeat(pad_before_hint), Face::MUTED));
            }
            left.push((label.into(), Face::MUTED));
            left.push(("  ".into(), Face::MUTED));
            left.push((right_text, daemon_face));
        } else {
            let pad = width.saturating_sub(left_w + right_w);
            if pad > 0 {
                left.push((" ".repeat(pad), Face::MUTED));
            }
            left.push((right_text, daemon_face));
        }
        left
    }

    #[cfg(test)]
    pub(super) fn subagent_rows(&self, height: usize) -> usize {
        usize::from(self.layout.subagent_rows(height, self.subagents.len()))
    }

    pub(super) fn compute_layout(
        &self,
        width: u16,
        height: u16,
    ) -> Option<(FrameLayout, Vec<String>)> {
        let (rows, cursor) = self
            .editor
            .layout(FrameLayout::editor_width(usize::from(width)));
        let layout = FrameLayout::compute_with_inspector(
            &self.layout,
            width,
            height,
            rows.len(),
            cursor,
            self.mention_panel_rows(),
            self.subagents.len(),
            !self.tool_blocks.is_empty(),
        )?;
        Some((layout, rows))
    }

    pub(super) fn subagent_line_spans(
        &self,
        index: usize,
        sub: &SubagentView,
        width: usize,
    ) -> Vec<(String, Face)> {
        let spinner = SPINNER_FRAMES[(sub.started.elapsed().as_millis() / 120 % 10) as usize];
        let label_limit = if width < 80 { 16 } else { 28 };
        let detail_limit = width.saturating_sub(42).max(12);
        let elapsed = format!("{:.1}s", sub.started.elapsed().as_secs_f64());
        let mut parts = vec![
            ("  ↳ ".into(), Face::MUTED),
            (
                format!("{spinner} worker #{} ", index + 1),
                Face::ACCENT_BOLD,
            ),
            (
                format!("[{}]", clip(&sub.label, label_limit)),
                Face::HIGHLIGHT_BOLD,
            ),
            (" · ".into(), Face::MUTED),
            (clip(&sub.phase, 14), Face::ACCENT),
        ];
        if !sub.detail.is_empty() {
            parts.push((" · ".into(), Face::MUTED));
            parts.push((clip(&sub.detail, detail_limit), Face::TEXT));
        }
        parts.push((" · ".into(), Face::MUTED));
        parts.push((elapsed, Face::MUTED));
        parts
    }

    #[cfg(test)]
    pub(super) fn subagent_status_lines(&self, width: usize) -> Vec<String> {
        self.subagents
            .iter()
            .enumerate()
            .map(|(i, sub)| {
                self.subagent_line_spans(i, sub, width)
                    .into_iter()
                    .map(|(text, _)| text)
                    .collect()
            })
            .collect()
    }

    fn render_header(&self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        if layout.header.len == 0 {
            return Ok(());
        }
        let w = usize::from(layout.width);
        let ws_name = self
            .options
            .workspace
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let tools_label = if self.options.no_shell {
            "file tools"
        } else {
            "file + shell tools"
        };
        let title_parts = [
            (" anticrumb", Face::ACCENT_BOLD),
            (" · ", Face::MUTED),
            (ws_name.as_str(), Face::MUTED),
            (" · ", Face::MUTED),
            (
                tools_label,
                if self.options.no_shell {
                    Face::MUTED
                } else {
                    Face::TEXT
                },
            ),
        ];
        queue!(out, cursor::MoveTo(0, layout.header.start)).map_err(ServiceError::storage)?;
        let title_lines = markdown::spans(&title_parts, w);
        let title_width = if let Some(first) = title_lines.first() {
            markdown::write_line(out, first, self.theme).map_err(ServiceError::storage)?;
            first.iter().map(|s| s.text.width()).sum()
        } else {
            0
        };
        if title_width < w {
            queue!(
                out,
                style::SetBackgroundColor(self.theme.background),
                terminal::Clear(ClearType::UntilNewLine)
            )
            .map_err(ServiceError::storage)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_clipped_padded_cell(
        out: &mut impl Write,
        col: u16,
        y: u16,
        line: &[Span],
        leading_space: bool,
        max_width: usize,
        pad_to: usize,
        theme: crate::tui::theme::Theme,
    ) -> Result<()> {
        if max_width == 0 {
            return Ok(());
        }
        queue!(out, cursor::MoveTo(col, y)).map_err(ServiceError::storage)?;
        markdown::write_clipped_padded_line(out, line, leading_space, max_width, pad_to, theme)
            .map_err(ServiceError::storage)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_inspector_pane_rect(
        &self,
        out: &mut impl Write,
        layout: FrameLayout,
        rect: Rect,
        block_id: BlockId,
        scroll_override: Option<usize>,
        zoom_mode: bool,
        spinner: &str,
        selection_matches: bool,
    ) -> Result<Option<PaintedInspectorPane>> {
        let vis_len = usize::from(rect.row.len);
        let col_len = usize::from(rect.col.len);
        if vis_len == 0 || col_len == 0 {
            return Ok(None);
        }
        let Some(block) = self.tool_blocks.get(block_id) else {
            return Ok(None);
        };
        let inner_w = col_len.saturating_sub(1).max(1);
        let detail = self
            .tool_blocks
            .detail_lines(block_id, inner_w, &self.highlights);
        let header_rows: u16 = 1;
        let (body_rows, has_footer) = if vis_len >= 3 {
            (vis_len - 2, true)
        } else if vis_len == 2 {
            (1, false)
        } else {
            (0, false)
        };
        let max_scroll = detail.len().saturating_sub(body_rows.max(1));
        let eff_scroll = scroll_override
            .unwrap_or(block.scroll_from_bottom)
            .min(max_scroll);
        let body_end = detail.len().saturating_sub(eff_scroll);
        let body_start = body_end.saturating_sub(body_rows);

        // Row 0: Sticky header
        let y0 = rect.row.start;
        let row0_w = Self::safe_row_width(y0, layout.width, layout.height)
            .saturating_sub(usize::from(rect.col.start))
            .min(col_len);
        let header_line = if zoom_mode {
            let mut h = inspector::inspector_header_line(block, row0_w, spinner);
            if let Some(first) = h.first_mut() {
                if let Some(rest) = first.text.strip_prefix(" Inspector") {
                    first.text = format!(" Inspector [Zoom]{rest}");
                }
            }
            inspector::clip_rendered_line(&h, row0_w)
        } else {
            inspector::inspector_header_line(block, row0_w, spinner)
        };
        Self::write_clipped_padded_cell(
            out,
            rect.col.start,
            y0,
            &header_line,
            false,
            row0_w,
            row0_w,
            self.theme,
        )?;

        // Body rows (1..1+body_rows) — streamed zero-alloc directly from cached detail lines.
        for i in 0..body_rows {
            let y = rect.row.start + header_rows + i as u16;
            let row_w = Self::safe_row_width(y, layout.width, layout.height)
                .saturating_sub(usize::from(rect.col.start))
                .min(col_len);
            let abs_idx = body_start + i;
            let (src_slice, leading_space): (&[Span], bool) = match detail.get(abs_idx) {
                Some(src_line) => (src_line.as_slice(), true),
                None => (&[], false),
            };
            Self::write_clipped_padded_cell(
                out,
                rect.col.start,
                y,
                src_slice,
                leading_space,
                row_w,
                row_w,
                self.theme,
            )?;
            if selection_matches {
                if let Some((col_off, selected)) = self
                    .selection
                    .as_ref()
                    .filter(|_| self.selection_target == SelectionTarget::Inspector)
                    .and_then(|sel| sel.fragment_spans(abs_idx))
                {
                    let draw_col = col_off.saturating_add(1);
                    if !selected.is_empty() && draw_col < row_w {
                        let clipped_sel = inspector::clip_rendered_line(
                            &selected,
                            row_w.saturating_sub(draw_col),
                        );
                        queue!(out, cursor::MoveTo(rect.col.start + draw_col as u16, y))
                            .map_err(ServiceError::storage)?;
                        markdown::write_selection(out, &clipped_sel, self.theme)
                            .map_err(ServiceError::storage)?;
                    }
                }
            }
        }

        // Bottom status/scroll footer row
        let mut live_badge = None;
        let has_active_block = self.tool_blocks.active_block().is_some();
        if has_footer {
            let yf = rect.row.end().saturating_sub(1);
            let row_w = Self::safe_row_width(yf, layout.width, layout.height)
                .saturating_sub(usize::from(rect.col.start))
                .min(col_len);
            let (footer_line, badge_cols) = if zoom_mode {
                let pct = if max_scroll == 0 {
                    100
                } else {
                    ((body_end * 100) / detail.len().max(1)).clamp(1, 100)
                };
                let (right, bface, action) = if block.active {
                    if !block.follow_tool {
                        let label = if eff_scroll > 0 {
                            format!("[▶ Follow {pct}%]")
                        } else {
                            "[▶ Follow]".to_owned()
                        };
                        (
                            label,
                            Face::ACCENT_BOLD,
                            Some(inspector::InspectorBadgeAction::ToggleFollow),
                        )
                    } else if eff_scroll > 0 {
                        (
                            format!("[↓ Live {pct}%]"),
                            Face::ACCENT_BOLD,
                            Some(inspector::InspectorBadgeAction::JumpToBottom),
                        )
                    } else {
                        (
                            "[● Follow]".to_owned(),
                            Face::HIGHLIGHT,
                            Some(inspector::InspectorBadgeAction::ToggleFollow),
                        )
                    }
                } else if has_active_block {
                    (
                        "[▶ Follow]".to_owned(),
                        Face::ACCENT_BOLD,
                        Some(inspector::InspectorBadgeAction::ToggleFollow),
                    )
                } else if eff_scroll > 0 {
                    (
                        format!("[↓ Bottom {pct}%]"),
                        Face::ACCENT_BOLD,
                        Some(inspector::InspectorBadgeAction::JumpToBottom),
                    )
                } else {
                    (format!("[{pct}%]"), Face::MUTED, None)
                };
                let right_clean = clip(&right, row_w);
                let right_w = right_clean.width();
                let left_hint = clip(
                    " ←/→ tool · ↑↓/PgUp/PgDn scroll · f follow · Ctrl+O/Esc close",
                    row_w.saturating_sub(right_w + 1),
                );
                let left_w = left_hint.width();
                let pad = row_w.saturating_sub(left_w + right_w);
                let mut line = Vec::new();
                if !left_hint.is_empty() {
                    line.push(Span {
                        text: left_hint,
                        face: Face::MUTED,
                    });
                }
                if pad > 0 {
                    line.push(Span {
                        text: " ".repeat(pad),
                        face: Face::MUTED,
                    });
                }
                let bstart = left_w + pad;
                if !right_clean.is_empty() {
                    line.push(Span {
                        text: right_clean,
                        face: bface,
                    });
                }
                let hit = action.map(|act| (bstart, bstart + right_w, act));
                (line, hit)
            } else {
                inspector::inspector_footer_line(
                    block,
                    has_active_block,
                    detail.len(),
                    body_rows,
                    row_w,
                )
            };
            Self::write_clipped_padded_cell(
                out,
                rect.col.start,
                yf,
                &footer_line,
                false,
                row_w,
                row_w,
                self.theme,
            )?;
            live_badge = badge_cols.map(|(s, e, act)| {
                (
                    yf,
                    rect.col.start.saturating_add(s as u16),
                    rect.col.start.saturating_add(e as u16),
                    act,
                )
            });
        }

        Ok(Some(PaintedInspectorPane {
            block_id,
            rect,
            header_rows,
            body_rows,
            body_start,
            detail_lines: detail,
            live_badge,
        }))
    }

    pub(super) fn render_transcript(
        &mut self,
        out: &mut impl Write,
        layout: FrameLayout,
        full_clear: bool,
    ) -> Result<()> {
        self.painted_blocks.clear();
        self.painted_inspector = None;
        if layout.transcript.len == 0 {
            self.painted_transcript.clear();
            self.painted_all_transcript.clear();
            self.scroll_bottom_button = None;
            self.painted_size = (layout.width, layout.height);
            self.painted_layout = Some(layout);
            return Ok(());
        }
        let w = usize::from(layout.width);
        let left_w = usize::from(layout.transcript_left.len);
        let side_rect = layout.inspector_rect;
        let side_divider = layout.divider_x;
        let viewport = usize::from(layout.transcript.len);
        let spinner = self.spinner();
        let transcript_selection = self
            .selection
            .as_ref()
            .filter(|_| self.selection_target == SelectionTarget::Transcript);
        let (start, effective_scroll, painted, all_snapshot, inline_split, block_spans) =
            if let Some(selection) = transcript_selection {
                let (start, end) = self.selection_view_range(selection.lines.len(), viewport);
                let eff = selection.lines.len().saturating_sub(end);
                let inline_split = if self.layout.inspector.mode
                    == super::layout::InspectorMode::Inline
                {
                    super::layout::HorizontalSplit::compute(layout.width, &self.layout.inspector)
                } else {
                    None
                };
                let spans: Vec<BlockSpan> =
                    self.painted_all_transcript.block_spans().cloned().collect();
                (
                    start,
                    eff,
                    selection.lines[start..end].to_vec(),
                    None,
                    inline_split,
                    spans,
                )
            } else {
                // Release the previous snapshot first so the cache appends in place.
                self.painted_all_transcript.clear();
                let (all, inline_split, _) = self.transcript_lines_for(layout, spinner);
                let anchor_w = if side_rect.is_some() { left_w } else { w };
                self.sync_scroll_anchor(
                    all.len(),
                    all.evicted_lines(),
                    anchor_w,
                    all.visible_stream_len(),
                );
                let (start, end) = self.selection_view_range(all.len(), viewport);
                let eff = all.len().saturating_sub(end);
                let painted = all.slice(start, end);
                let spans: Vec<BlockSpan> = all.block_spans().cloned().collect();
                (start, eff, painted, Some(all), inline_split, spans)
            };

        let end = start + painted.len();

        // Build per-row lookup for inline split blocks (virtual row -> (block_id, BlockRowKind)).
        // Also collect visible block hit-test records.
        let mut inline_row_owner: Vec<Option<(BlockId, BlockRowKind)>> = vec![None; painted.len()];
        for span in &block_spans {
            if span.end_line <= start || span.start_line >= end {
                continue;
            }
            let content_end = if span.has_bottom_border {
                span.end_line.saturating_sub(1)
            } else {
                span.end_line
            };
            let vis_vstart = span.start_line.max(start);
            let vis_vend = content_end.min(end);
            if vis_vstart >= vis_vend {
                continue;
            }
            let Some(block) = self.tool_blocks.get(span.block_id) else {
                continue;
            };
            let is_inline_split = inline_split.is_some() && !block.is_collapsed();
            let mut row_map = Vec::with_capacity(vis_vend - vis_vstart);
            for vrow in vis_vstart..vis_vend {
                let rel_in_span = vrow - span.start_line;
                let rel_in_view = vrow - start;
                let y = layout.transcript.start + rel_in_view as u16;
                let kind = span
                    .row_kinds
                    .get(rel_in_span)
                    .copied()
                    .unwrap_or(BlockRowKind::Padding);
                row_map.push((y, kind));
                if is_inline_split {
                    inline_row_owner[rel_in_view] = Some((span.block_id, kind));
                }
            }
            let vis_row_start = layout.transcript.start + (vis_vstart - start) as u16;
            let vis_row_end = layout.transcript.start + (vis_vend - start) as u16;
            let (left_col_end, inspector_rect, divider_x) =
                if let (true, Some(split)) = (is_inline_split, inline_split) {
                    (
                        split.left.end(),
                        Some(Rect {
                            col: split.right,
                            row: RowSpan {
                                start: vis_row_start,
                                len: vis_row_end.saturating_sub(vis_row_start),
                            },
                        }),
                        Some(split.divider_x),
                    )
                } else {
                    (layout.transcript_left.end(), None, side_divider)
                };
            self.painted_blocks.push(PaintedToolBlock {
                block_id: span.block_id,
                vis_row_start,
                vis_row_end,
                left_col_start: 0,
                left_col_end,
                inspector_rect,
                divider_x,
                row_map,
                live_badge: None,
                inspector_header_rows: 1,
                inspector_body_rows: 0,
                inspector_body_start: 0,
                inspector_detail_lines: None,
            });
        }

        // Paint transcript rows (left column when side_rect or inline split owns the row).
        for (row, cached_line) in painted.iter().enumerate() {
            let y = layout.transcript.start + row as u16;
            let row_safe_w = Self::safe_row_width(y, layout.width, layout.height);
            let inline_owner = inline_row_owner[row];
            let (col_limit, preserve_right) = if side_rect.is_some() {
                (left_w.min(row_safe_w), true)
            } else if inline_owner.is_some() {
                (
                    inline_split
                        .map_or(row_safe_w, |s| usize::from(s.left.len))
                        .min(row_safe_w),
                    true,
                )
            } else {
                (row_safe_w, false)
            };

            // For visible ToolItem rows, paint the live selection marker `›` directly
            // during viewport compositing so moving selection incurs 0 LineCache invalidations.
            let live_item_line: Option<markdown::Line> =
                if self.selection_target != SelectionTarget::Transcript || self.selection.is_none()
                {
                    let owner = inline_owner.or_else(|| {
                        self.painted_blocks.iter().find_map(|pb| {
                            (y >= pb.vis_row_start && y < pb.vis_row_end).then(|| {
                                pb.row_map
                                    .iter()
                                    .find(|(r, _)| *r == y)
                                    .map(|(_, k)| (pb.block_id, *k))
                            })?
                        })
                    });
                    if let Some((bid, BlockRowKind::ToolItem(idx))) = owner {
                        self.tool_blocks.get(bid).and_then(|block| {
                            let item = block.items.get(idx)?;
                            let selected =
                                idx == block.selected.min(block.items.len().saturating_sub(1));
                            Some(inspector::render_tool_item_row(
                                item, selected, col_limit, spinner,
                            ))
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };
            let line = live_item_line.as_ref().unwrap_or(cached_line);

            if preserve_right {
                Self::write_clipped_padded_cell(
                    out, 0, y, line, false, col_limit, col_limit, self.theme,
                )?;
            } else {
                queue!(out, cursor::MoveTo(0, y)).map_err(ServiceError::storage)?;
                let line_width =
                    markdown::write_clipped_padded_line(out, line, false, col_limit, 0, self.theme)
                        .map_err(ServiceError::storage)?;
                if line_width < w {
                    queue!(
                        out,
                        style::SetBackgroundColor(self.theme.background),
                        terminal::Clear(ClearType::UntilNewLine)
                    )
                    .map_err(ServiceError::storage)?;
                }
            }

            if let Some((column, selected)) = self
                .selection
                .as_ref()
                .filter(|_| self.selection_target == SelectionTarget::Transcript)
                .and_then(|selection| selection.fragment_spans(start + row))
            {
                if !selected.is_empty() && column < col_limit {
                    let clipped_sel =
                        inspector::clip_rendered_line(&selected, col_limit.saturating_sub(column));
                    if !clipped_sel.is_empty() {
                        queue!(out, cursor::MoveTo(column as u16, y))
                            .map_err(ServiceError::storage)?;
                        markdown::write_selection(out, &clipped_sel, self.theme)
                            .map_err(ServiceError::storage)?;
                    }
                }
            }
        }

        // Clear trailing unused transcript rows on the left (or full width if no side pane).
        if !full_clear || side_rect.is_some() {
            let clear_from = usize::from(layout.transcript.start) + painted.len();
            for row in clear_from..usize::from(layout.transcript.end()) {
                let y = row as u16;
                if side_rect.is_some() {
                    Self::write_clipped_padded_cell(
                        out,
                        0,
                        y,
                        &[],
                        false,
                        left_w,
                        left_w,
                        self.theme,
                    )?;
                } else if !full_clear {
                    queue!(
                        out,
                        cursor::MoveTo(0, y),
                        style::SetBackgroundColor(self.theme.background),
                        terminal::Clear(ClearType::UntilNewLine)
                    )
                    .map_err(ServiceError::storage)?;
                }
            }
        }

        // Paint inline sticky split dividers & right Inspectors.
        if let Some(split) = inline_split {
            let active_inline_sel = self.painted_inline_inspector_target;
            let block_count = self.painted_blocks.len();
            for idx in 0..block_count {
                let (block_id, rect_opt) = {
                    let pb = &self.painted_blocks[idx];
                    (pb.block_id, pb.inspector_rect)
                };
                let Some(rect) = rect_opt else {
                    continue;
                };
                for y in rect.row.start..rect.row.end() {
                    queue!(
                        out,
                        cursor::MoveTo(split.divider_x, y),
                        style::Print("│".with(self.theme.subtle).on(self.theme.background))
                    )
                    .map_err(ServiceError::storage)?;
                }
                let sel_matches = active_inline_sel == Some(block_id);
                if let Some(pane) = self.render_inspector_pane_rect(
                    out,
                    layout,
                    rect,
                    block_id,
                    None,
                    false,
                    spinner,
                    sel_matches,
                )? {
                    let pb = &mut self.painted_blocks[idx];
                    pb.live_badge = pane.live_badge;
                    pb.inspector_header_rows = pane.header_rows;
                    pb.inspector_body_rows = pane.body_rows;
                    pb.inspector_body_start = pane.body_start;
                    pb.inspector_detail_lines = Some(pane.detail_lines);
                }
            }
        }

        // Paint Side docked mode vertical divider & focused right Inspector.
        if let (Some(rect), Some(div_x)) = (side_rect, side_divider) {
            for y in layout.transcript.start..layout.transcript.end() {
                queue!(
                    out,
                    cursor::MoveTo(div_x, y),
                    style::Print("│".with(self.theme.subtle).on(self.theme.background))
                )
                .map_err(ServiceError::storage)?;
            }
            if let Some(focused_id) = self.tool_blocks.focused_id() {
                let sel_matches = self.painted_inline_inspector_target.is_none();
                self.painted_inspector = self.render_inspector_pane_rect(
                    out,
                    layout,
                    rect,
                    focused_id,
                    None,
                    false,
                    spinner,
                    sel_matches,
                )?;
            }
        }

        if let Some(all) = all_snapshot {
            self.painted_all_transcript = all;
        }
        self.scroll = effective_scroll;
        self.painted_effective_scroll = effective_scroll;
        if effective_scroll == 0 && self.scroll_anchor_deficit.is_none() {
            self.has_unread_below = false;
        }
        self.scroll_relative = false;
        self.painted_transcript_start = start;
        self.painted_transcript = painted;
        self.painted_size = (layout.width, layout.height);
        self.painted_layout = Some(layout);
        Ok(())
    }

    fn render_inspector_zoom_overlay(
        &mut self,
        out: &mut impl Write,
        layout: FrameLayout,
    ) -> Result<()> {
        if !self.inspector_zoom || layout.transcript.len == 0 {
            return Ok(());
        }
        let Some(focused_id) = self.tool_blocks.focused_id() else {
            self.inspector_zoom = false;
            return Ok(());
        };
        let rect = Rect {
            col: ColSpan {
                start: 0,
                len: layout.width,
            },
            row: layout.transcript,
        };
        let spinner = self.spinner();
        self.painted_inspector = self.render_inspector_pane_rect(
            out,
            layout,
            rect,
            focused_id,
            Some(self.inspector_zoom_scroll),
            true,
            spinner,
            true,
        )?;
        Ok(())
    }

    fn approval_hint_parts(
        &self,
        approval: &PendingToolApproval,
        include_esc: bool,
    ) -> Vec<(String, Face)> {
        let mut parts = vec![
            (" ".into(), Face::MUTED),
            ("[Y] Approve".into(), Face::HIGHLIGHT_BOLD),
            ("  ".into(), Face::MUTED),
            ("[N] Reject".into(), Face::DANGER_BOLD),
        ];
        let count_text = if approval.call_count <= 1 {
            " · 1 tool".to_owned()
        } else {
            format!(" · {} tools", approval.call_count)
        };
        let origin = approval
            .source
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        let queued = if self.tool_approvals.len() > 1 {
            format!(" (+{} queued)", self.tool_approvals.len() - 1)
        } else {
            String::new()
        };
        parts.push((
            format!("{count_text}{origin} · step {}{queued}", approval.step),
            Face::ACCENT,
        ));
        if include_esc {
            parts.push((" · Esc: interrupt".into(), Face::MUTED));
        }
        parts
    }

    fn hint_line(&self, layout: FrameLayout) -> (markdown::Line, Option<(u16, u16)>) {
        let w = usize::from(layout.width);
        let resume_notice = self.resume_notice.as_ref().filter(|_| {
            self.menu.as_ref().is_some_and(|menu| {
                matches!(
                    menu.choices.first().map(|choice| &choice.value),
                    Some(Selection::Resume(_))
                )
            })
        });
        let right_badge = if let Some(selected) = self.selection.as_ref().and_then(|s| s.text()) {
            Some((
                format!("{} chars selected ", selected.chars().count()),
                Face::ACCENT,
            ))
        } else {
            self.clipboard_notice.as_ref().map(|notice| {
                let face = if notice.starts_with("Could not") {
                    Face::DANGER
                } else {
                    Face::HIGHLIGHT
                };
                (format!("✓ {notice} "), face)
            })
        };
        let safe_w = Self::safe_row_width(layout.hint.start, layout.width, layout.height);

        if layout.transcript.len > 0
            && self.painted_effective_scroll > 0
            && self.history_clear_confirmation.is_none()
            && self.selection.is_none()
            && resume_notice.is_none()
            && safe_w > 0
        {
            let badge = self.scroll_bottom_badge();
            let spinner = self.spinner();
            let full_btn = badge.label(spinner);
            let compact_btn = badge.compact_label(spinner);
            if let Some(approval) = self.tool_approvals.front() {
                let raw_btn = if safe_w >= full_btn.width() + 26 {
                    full_btn.as_str()
                } else {
                    compact_btn.as_str()
                };
                let btn_text = clip(raw_btn, safe_w);
                let btn_w = btn_text.width();
                let left_budget = safe_w.saturating_sub(btn_w + 2);
                if !btn_text.is_empty() && left_budget >= 23 {
                    let left_parts =
                        clip_spans(self.approval_hint_parts(approval, false), left_budget);
                    let left_w = spans_width(&left_parts);
                    let gap = safe_w.saturating_sub(left_w + btn_w);
                    let mut line: markdown::Line = left_parts
                        .into_iter()
                        .map(|(text, face)| Span { text, face })
                        .collect();
                    if gap > 0 {
                        line.push(Span {
                            text: " ".repeat(gap),
                            face: Face::MUTED,
                        });
                    }
                    let btn_start = left_w + gap;
                    line.push(Span {
                        text: btn_text,
                        face: Face::ACCENT_BOLD,
                    });
                    return (line, Some((btn_start as u16, (btn_start + btn_w) as u16)));
                }
                let line: markdown::Line =
                    clip_spans(self.approval_hint_parts(approval, false), safe_w)
                        .into_iter()
                        .map(|(text, face)| Span { text, face })
                        .collect();
                return (line, None);
            }
            let raw_btn = if safe_w >= 22 {
                full_btn.as_str()
            } else {
                compact_btn.as_str()
            };
            let btn_text = clip(raw_btn, safe_w);
            let btn_w = btn_text.width();
            if !btn_text.is_empty() {
                if let Some((right_text, badge_face)) = right_badge.as_ref() {
                    let badge_clipped = clip(right_text, safe_w);
                    let badge_w = badge_clipped.width();
                    if btn_w + 2 + badge_w <= safe_w {
                        let center_start = safe_w.saturating_sub(btn_w) / 2;
                        let start = if center_start + btn_w + 2 + badge_w <= safe_w {
                            center_start
                        } else {
                            safe_w.saturating_sub(badge_w + 2 + btn_w)
                        };
                        let gap = safe_w.saturating_sub(start + btn_w + badge_w);
                        let mut line = Vec::new();
                        if start > 0 {
                            line.push(Span {
                                text: " ".repeat(start),
                                face: Face::MUTED,
                            });
                        }
                        line.push(Span {
                            text: btn_text,
                            face: Face::ACCENT_BOLD,
                        });
                        if gap > 0 {
                            line.push(Span {
                                text: " ".repeat(gap),
                                face: Face::MUTED,
                            });
                        }
                        if !badge_clipped.is_empty() {
                            line.push(Span {
                                text: badge_clipped,
                                face: *badge_face,
                            });
                        }
                        return (line, Some((start as u16, (start + btn_w) as u16)));
                    }
                } else {
                    let start = safe_w.saturating_sub(btn_w) / 2;
                    let mut line = Vec::new();
                    if start > 0 {
                        line.push(Span {
                            text: " ".repeat(start),
                            face: Face::MUTED,
                        });
                    }
                    line.push(Span {
                        text: btn_text,
                        face: Face::ACCENT_BOLD,
                    });
                    return (line, Some((start as u16, (start + btn_w) as u16)));
                }
            }
        }

        if self.history_clear_confirmation.is_none()
            && self.selection.is_none()
            && resume_notice.is_none()
        {
            if let Some(approval) = self.tool_approvals.front() {
                let parts = self.approval_hint_parts(approval, true);
                let mut line = Vec::new();
                if let Some((badge, badge_face)) = right_badge {
                    let badge_clipped = clip(&badge, safe_w);
                    let badge_w = badge_clipped.width();
                    let left_max = safe_w.saturating_sub(badge_w + 2);
                    let left_parts = if left_max >= 8 {
                        clip_spans(parts, left_max)
                    } else {
                        Vec::new()
                    };
                    let left_w = spans_width(&left_parts);
                    let pad = safe_w.saturating_sub(left_w + badge_w);
                    for (text, face) in left_parts {
                        line.push(Span { text, face });
                    }
                    if pad > 0 {
                        line.push(Span {
                            text: " ".repeat(pad),
                            face: Face::MUTED,
                        });
                    }
                    if !badge_clipped.is_empty() {
                        line.push(Span {
                            text: badge_clipped,
                            face: badge_face,
                        });
                    }
                } else {
                    for (text, face) in clip_spans(parts, safe_w) {
                        line.push(Span { text, face });
                    }
                }
                return (line, None);
            }
        }

        let mut hint_is_live = false;
        let hint: Option<String> = if self.history_clear_confirmation.is_some() {
            Some(" Type DELETE + Enter to confirm · Esc cancels".into())
        } else if self.selection.is_some() {
            Some(" Drag to select · Release to copy · Esc cancels".into())
        } else if let Some(notice) = resume_notice {
            Some(format!(" {notice}"))
        } else if self.history_index < self.history.len() {
            Some(format!(
                " History {}/{} · ↑↓ edit/history · Enter send",
                self.history_index + 1,
                self.history.len()
            ))
        } else if !self.files.is_empty() {
            Some(format!(
                " {} file(s): {} · Enter to send",
                self.files.len(),
                self.files
                    .iter()
                    .map(|f| f.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else if let Some(c) = &self.completion {
            Some(format!(
                " Tab → {}",
                c.values
                    .iter()
                    .take(3)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("  ")
            ))
        } else if self.busy() {
            hint_is_live = true;
            let spinner = self.spinner();
            let live_text = if self.active.is_some()
                && self.stream.is_empty()
                && !self.thinking_preview.is_empty()
            {
                self.thinking_preview
                    .split_once(" thinking ")
                    .map_or(self.thinking_preview.as_str(), |(_, rest)| {
                        &self.thinking_preview
                            [self.thinking_preview.len() - rest.len() - "thinking ".len()..]
                    })
                    .trim()
            } else {
                match self.phase.as_str() {
                    "running" | "working" => "working...",
                    "answering" => "generating...",
                    "loading" => "loading...",
                    "deleting" => "deleting...",
                    other => other,
                }
            };
            let suffix = if self.active.is_some() {
                " · Esc: interrupt"
            } else {
                ""
            };
            if !suffix.is_empty() && w > live_text.width() + suffix.width() + 5 {
                Some(format!(" {spinner} {live_text}{suffix}"))
            } else {
                Some(format!(" {spinner} {live_text}"))
            }
        } else {
            None
        };
        let Some(hint) = hint else {
            let mut line = Vec::new();
            if let Some((badge, badge_face)) = right_badge {
                let badge_clipped = clip(&badge, safe_w);
                let badge_w = badge_clipped.width();
                let rule_w = safe_w.saturating_sub(badge_w + 1);
                if rule_w > 0 {
                    line.push(Span {
                        text: "─".repeat(rule_w),
                        face: Face::SUBTLE,
                    });
                }
                let pad = safe_w.saturating_sub(rule_w + badge_w);
                if pad > 0 {
                    line.push(Span {
                        text: " ".repeat(pad),
                        face: Face::MUTED,
                    });
                }
                if !badge_clipped.is_empty() {
                    line.push(Span {
                        text: badge_clipped,
                        face: badge_face,
                    });
                }
            } else if safe_w > 0 {
                line.push(Span {
                    text: "─".repeat(safe_w),
                    face: Face::SUBTLE,
                });
            }
            return (line, None);
        };
        let hint_face = if hint_is_live {
            Face::ACCENT
        } else {
            Face::MUTED
        };
        let mut line = Vec::new();
        if let Some((badge, badge_face)) = right_badge {
            let badge_clipped = clip(&badge, safe_w);
            let badge_w = badge_clipped.width();
            let left_max = safe_w.saturating_sub(badge_w + 2);
            let left_clipped = if left_max >= 8 {
                clip(&hint, left_max)
            } else {
                String::new()
            };
            let left_w = left_clipped.width();
            let pad = safe_w.saturating_sub(left_w + badge_w);
            if !left_clipped.is_empty() {
                line.push(Span {
                    text: left_clipped,
                    face: hint_face,
                });
            }
            if pad > 0 {
                line.push(Span {
                    text: " ".repeat(pad),
                    face: Face::MUTED,
                });
            }
            if !badge_clipped.is_empty() {
                line.push(Span {
                    text: badge_clipped,
                    face: badge_face,
                });
            }
        } else {
            let hint_clipped = clip(&hint, safe_w);
            if !hint_clipped.is_empty() {
                line.push(Span {
                    text: hint_clipped,
                    face: hint_face,
                });
            }
        }
        (line, None)
    }

    pub(super) fn render_hint(&mut self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        if layout.hint.len == 0 {
            self.scroll_bottom_button = None;
            if self.selection.is_none() {
                self.painted_hint.clear();
            }
            return Ok(());
        }
        let safe_w = Self::safe_row_width(layout.hint.start, layout.width, layout.height);
        let hint_selected =
            self.selection.is_some() && self.selection_target == SelectionTarget::Hint;
        let (line, button) = if hint_selected {
            (
                self.selection
                    .as_ref()
                    .and_then(|s| s.lines.first())
                    .cloned()
                    .unwrap_or_default(),
                None,
            )
        } else {
            let (line, button) = self.hint_line(layout);
            if self.selection.is_none() {
                self.painted_hint = vec![line.clone()];
            }
            (line, button)
        };
        self.scroll_bottom_button = button.map(|(start, end)| (layout.hint.start, start, end));
        let rendered_width: usize = line.iter().map(|s| s.text.width()).sum();
        queue!(out, cursor::MoveTo(0, layout.hint.start)).map_err(ServiceError::storage)?;
        markdown::write_line(out, &line, self.theme).map_err(ServiceError::storage)?;
        Self::pad_and_clear_row(out, layout.hint.start, rendered_width, layout, self.theme)?;
        if hint_selected {
            if let Some((column, selected)) = self
                .selection
                .as_ref()
                .and_then(|selection| selection.fragment_spans(0))
            {
                if !selected.is_empty() && column < safe_w {
                    queue!(out, cursor::MoveTo(column as u16, layout.hint.start))
                        .map_err(ServiceError::storage)?;
                    markdown::write_selection(out, &selected, self.theme)
                        .map_err(ServiceError::storage)?;
                }
            }
        }
        Ok(())
    }

    fn render_composer(
        &mut self,
        out: &mut impl Write,
        layout: FrameLayout,
        rows: &[String],
    ) -> Result<()> {
        let w = usize::from(layout.width);
        let visible = usize::from(layout.composer.len);
        let composer_selected =
            self.selection.is_some() && self.selection_target == SelectionTarget::Composer;
        let lines: Vec<markdown::Line> = if composer_selected {
            self.selection
                .as_ref()
                .map(|s| s.lines.iter().take(visible).cloned().collect())
                .unwrap_or_default()
        } else {
            let lines: Vec<markdown::Line> = rows[layout.editor_from..layout.editor_from + visible]
                .iter()
                .map(|row| {
                    if row.is_empty() {
                        Vec::new()
                    } else {
                        vec![Span {
                            text: row.clone(),
                            face: Face::TEXT,
                        }]
                    }
                })
                .collect();
            if self.selection.is_none() {
                self.painted_composer = lines.clone();
            }
            lines
        };
        for (i, line) in lines.iter().enumerate() {
            let y = layout.composer.start + i as u16;
            let row_width: usize = line.iter().map(|s| s.text.width()).sum();
            queue!(
                out,
                cursor::MoveTo(0, y),
                style::Print(if i == 0 { "> " } else { "  " }.with(self.theme.accent)),
            )
            .map_err(ServiceError::storage)?;
            markdown::write_line(out, line, self.theme).map_err(ServiceError::storage)?;
            if 2 + row_width < w {
                queue!(
                    out,
                    style::SetBackgroundColor(self.theme.background),
                    terminal::Clear(ClearType::UntilNewLine)
                )
                .map_err(ServiceError::storage)?;
            }
            if composer_selected {
                if let Some((column, selected)) = self
                    .selection
                    .as_ref()
                    .and_then(|selection| selection.fragment_spans(i))
                {
                    if !selected.is_empty() && 2 + column < w {
                        queue!(out, cursor::MoveTo((2 + column) as u16, y))
                            .map_err(ServiceError::storage)?;
                        markdown::write_selection(out, &selected, self.theme)
                            .map_err(ServiceError::storage)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn render_completion(&self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        let Some(mention) = &self.mention else {
            return Ok(());
        };
        if layout.completion.len == 0 {
            return Ok(());
        }
        let start_row = layout.completion.start;
        let total_rows = usize::from(layout.completion.len);

        // Top horizontal divider separating composer from the mention panel.
        let divider_w = Self::safe_row_width(start_row, layout.width, layout.height);
        queue!(
            out,
            cursor::MoveTo(0, start_row),
            style::Print(
                "─"
                    .repeat(divider_w)
                    .with(self.theme.subtle)
                    .on(self.theme.background)
            ),
            style::SetBackgroundColor(self.theme.background),
            terminal::Clear(ClearType::UntilNewLine)
        )
        .map_err(ServiceError::storage)?;

        if total_rows == 1 {
            return Ok(());
        }

        let has_hint = total_rows >= 3;
        let body_end = total_rows - usize::from(has_hint);

        if mention.matches.is_empty() {
            let status_row = start_row + 1;
            let safe_w = Self::safe_row_width(status_row, layout.width, layout.height);
            queue!(out, cursor::MoveTo(0, status_row)).map_err(ServiceError::storage)?;
            if mention.loading {
                let spinner =
                    SPINNER_FRAMES[(mention.started_at.elapsed().as_millis() / 120 % 10) as usize];
                let msg = if mention.query.is_empty() {
                    " Fetching matches...".to_owned()
                } else {
                    format!(" Fetching matches for \"{}\"...", mention.query)
                };
                let line = markdown::spans(
                    &[
                        (" ", Face::MUTED),
                        (spinner, Face::ACCENT),
                        (&msg, Face::MUTED),
                    ],
                    safe_w.max(1),
                )
                .into_iter()
                .next()
                .unwrap_or_default();
                let rendered_w: usize = line.iter().map(|s| s.text.width()).sum();
                markdown::write_line(out, &line, self.theme).map_err(ServiceError::storage)?;
                Self::pad_and_clear_row(out, status_row, rendered_w, layout, self.theme)?;
            } else {
                let msg = if mention.query.is_empty() {
                    "   No workspace files found".to_owned()
                } else {
                    format!("   No matches for \"{}\"", mention.query)
                };
                let text = clip(&msg, safe_w);
                let rendered_w = text.width();
                queue!(
                    out,
                    style::Print(text.with(self.theme.muted).on(self.theme.background))
                )
                .map_err(ServiceError::storage)?;
                Self::pad_and_clear_row(out, status_row, rendered_w, layout, self.theme)?;
            }
            for empty_idx in 2..body_end {
                let row = start_row + empty_idx as u16;
                queue!(out, cursor::MoveTo(0, row)).map_err(ServiceError::storage)?;
                Self::pad_and_clear_row(out, row, 0, layout, self.theme)?;
            }
            if has_hint {
                let hint_row = start_row + total_rows as u16 - 1;
                let hint_w = Self::safe_row_width(hint_row, layout.width, layout.height);
                let hint = clip(" esc Cancel", hint_w);
                let rendered_w = hint.width();
                queue!(
                    out,
                    cursor::MoveTo(0, hint_row),
                    style::Print(hint.with(self.theme.muted).on(self.theme.background))
                )
                .map_err(ServiceError::storage)?;
                Self::pad_and_clear_row(out, hint_row, rendered_w, layout, self.theme)?;
            }
            return Ok(());
        }

        let avail = body_end - 1;
        let has_overflow = mention.has_overflow_row() && avail >= 2;
        let (visible, local_offset) = mention.visible_window(total_rows);

        let first_row_w = Self::safe_row_width(start_row + 1, layout.width, layout.height);
        let max_display_cap = (first_row_w * 42 / 100).max(16);
        let display_col_w = visible
            .iter()
            .map(|m| m.display.width())
            .max()
            .unwrap_or(16)
            .clamp(12, max_display_cap)
            .min(first_row_w.saturating_sub(10).max(8));

        for (i, matched) in visible.iter().enumerate() {
            let row = start_row + 1 + i as u16;
            let safe_w = Self::safe_row_width(row, layout.width, layout.height);
            let selected = mention.scroll + local_offset + i == mention.selected;
            let indicator = if selected { "›" } else { " " };
            let display_text = clip(&matched.display, display_col_w);
            let display_pad = " ".repeat(display_col_w.saturating_sub(display_text.width()));
            let kind_label = format!("{:<4}", matched.kind.label());

            let prefix_w = 3 + display_text.width() + display_pad.len() + 2 + kind_label.width();
            let detail_budget = safe_w.saturating_sub(prefix_w + 2);
            let base_detail =
                if matched.detail.is_empty() && matched.kind == super::mentions::MentionKind::Git {
                    "Git commit"
                } else {
                    matched.detail.as_str()
                };
            let detail_owned;
            let detail_src = if matched.recent {
                detail_owned = if base_detail.is_empty() {
                    "Recent".to_owned()
                } else {
                    format!("Recent · {base_detail}")
                };
                detail_owned.as_str()
            } else {
                base_detail
            };
            let detail_text = if detail_budget >= 8 {
                clip(detail_src, detail_budget)
            } else {
                String::new()
            };

            queue!(out, cursor::MoveTo(0, row)).map_err(ServiceError::storage)?;
            if selected {
                let mut full = format!(" {indicator} {display_text}{display_pad}  {kind_label}");
                if !detail_text.is_empty() {
                    full.push_str("  ");
                    full.push_str(&detail_text);
                }
                let full = clip(&full, safe_w);
                let fill = " ".repeat(safe_w.saturating_sub(full.width()));
                queue!(
                    out,
                    style::Print(
                        format!("{full}{fill}")
                            .with(self.theme.selected_text)
                            .on(self.theme.selected_background)
                    ),
                    style::SetBackgroundColor(self.theme.background),
                    terminal::Clear(ClearType::UntilNewLine)
                )
                .map_err(ServiceError::storage)?;
            } else {
                let mut parts: Vec<(&str, Face)> = vec![
                    ("   ", Face::TEXT),
                    (&display_text, Face::TEXT),
                    (&display_pad, Face::TEXT),
                    ("  ", Face::MUTED),
                    (
                        &kind_label,
                        match matched.kind {
                            super::mentions::MentionKind::Dir => Face::ACCENT,
                            super::mentions::MentionKind::Git => Face::HIGHLIGHT,
                            super::mentions::MentionKind::File => Face::MUTED,
                        },
                    ),
                ];
                if !detail_text.is_empty() {
                    parts.push(("  ", Face::MUTED));
                    parts.push((&detail_text, Face::MUTED));
                }
                let line = markdown::spans(&parts, safe_w.max(1))
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let rendered_w: usize = line.iter().map(|s| s.text.width()).sum();
                markdown::write_line(out, &line, self.theme).map_err(ServiceError::storage)?;
                Self::pad_and_clear_row(out, row, rendered_w, layout, self.theme)?;
            }
        }

        if has_overflow {
            let overflow_row = start_row + 1 + visible.len() as u16;
            let safe_w = Self::safe_row_width(overflow_row, layout.width, layout.height);
            let above = mention.hidden_above() + local_offset;
            let shown_end = (mention.scroll + local_offset + visible.len()).min(mention.total);
            let below = mention.total.saturating_sub(shown_end);
            let overflow_text = match (above > 0, below > 0) {
                (true, true) => format!("   ↑ {above} more · ↓ {below} more"),
                (false, true) => format!("   ↓ {below} more"),
                (true, false) => format!("   ↑ {above} more"),
                (false, false) => String::new(),
            };
            let overflow_text = clip(&overflow_text, safe_w);
            let rendered_w = overflow_text.width();
            queue!(
                out,
                cursor::MoveTo(0, overflow_row),
                style::Print(
                    overflow_text
                        .with(self.theme.muted)
                        .on(self.theme.background)
                )
            )
            .map_err(ServiceError::storage)?;
            Self::pad_and_clear_row(out, overflow_row, rendered_w, layout, self.theme)?;
        }

        let used_body_end = 1 + visible.len() + usize::from(has_overflow);
        for empty_idx in used_body_end..body_end {
            let row = start_row + empty_idx as u16;
            queue!(out, cursor::MoveTo(0, row)).map_err(ServiceError::storage)?;
            Self::pad_and_clear_row(out, row, 0, layout, self.theme)?;
        }

        if has_hint {
            let hint_row = start_row + total_rows as u16 - 1;
            let hint_w = Self::safe_row_width(hint_row, layout.width, layout.height);
            let hint = clip(
                " ↑/↓ Navigate · enter Select · tab Complete · esc Cancel",
                hint_w,
            );
            let rendered_w = hint.width();
            queue!(
                out,
                cursor::MoveTo(0, hint_row),
                style::Print(hint.with(self.theme.muted).on(self.theme.background))
            )
            .map_err(ServiceError::storage)?;
            Self::pad_and_clear_row(out, hint_row, rendered_w, layout, self.theme)?;
        }

        Ok(())
    }

    fn render_theme_preview_row(
        &self,
        out: &mut impl Write,
        row: usize,
        width: usize,
        is_header: bool,
    ) -> Result<()> {
        if width == 0 {
            return Ok(());
        }
        let id_suffix = format!(" ({})", self.theme.id);
        let parts: Vec<(&str, Face)> = if is_header {
            vec![
                (" Preview · ", Face::MUTED),
                (self.theme.name, Face::HEADING),
                (&id_suffix, Face::MUTED),
            ]
        } else {
            match row {
                0 => vec![
                    (" 31   ", Face::MUTED),
                    ("fn", Face::syntax(markdown::SyntaxRole::Keyword)),
                    (" ", Face::TEXT),
                    ("summarize", Face::syntax(markdown::SyntaxRole::Function)),
                    ("(users: &[", Face::TEXT),
                    ("User", Face::syntax(markdown::SyntaxRole::Type)),
                    ("]) -> ", Face::TEXT),
                    ("String", Face::syntax(markdown::SyntaxRole::Type)),
                    (" {", Face::TEXT),
                ],
                1 => {
                    let bg = markdown::DiffBg::Removed;
                    vec![
                        (" 32 ", Face::MUTED),
                        ("- ", Face::DANGER_BOLD.with_diff_bg(bg)),
                        (
                            "  ",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "let",
                            Face::syntax(markdown::SyntaxRole::Keyword).with_diff_bg(bg),
                        ),
                        (
                            " active = users.",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "iter",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "().",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "filter",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "(|u| u.",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "ok",
                            Face::syntax(markdown::SyntaxRole::Property).with_diff_bg(bg),
                        ),
                        (
                            ").",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "count",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "();",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                    ]
                }
                2 => {
                    let bg = markdown::DiffBg::Added;
                    vec![
                        (" 32 ", Face::MUTED),
                        ("+ ", Face::HIGHLIGHT_BOLD.with_diff_bg(bg)),
                        (
                            "  ",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "let",
                            Face::syntax(markdown::SyntaxRole::Keyword).with_diff_bg(bg),
                        ),
                        (
                            " n = users.",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "iter",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "().",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "filter",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "(|u| u.",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "is_active()",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            ").",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "count",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "();",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                    ]
                }
                3 => vec![
                    (" 33     ", Face::MUTED),
                    ("let", Face::syntax(markdown::SyntaxRole::Keyword)),
                    (" label = ", Face::TEXT),
                    ("\"active\"", Face::syntax(markdown::SyntaxRole::String)),
                    ("; ", Face::TEXT),
                    ("// badge", Face::syntax(markdown::SyntaxRole::Comment)),
                ],
                4 => {
                    let bg = markdown::DiffBg::Added;
                    vec![
                        (" 34 ", Face::MUTED),
                        ("+ ", Face::HIGHLIGHT_BOLD.with_diff_bg(bg)),
                        (
                            "  ",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "format!",
                            Face::syntax(markdown::SyntaxRole::Macro).with_diff_bg(bg),
                        ),
                        (
                            "(",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "\"{n} {label}: {}\"",
                            Face::syntax(markdown::SyntaxRole::String).with_diff_bg(bg),
                        ),
                        (
                            ", names.",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "join",
                            Face::syntax(markdown::SyntaxRole::Function).with_diff_bg(bg),
                        ),
                        (
                            "(",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                        (
                            "\", \"",
                            Face::syntax(markdown::SyntaxRole::String).with_diff_bg(bg),
                        ),
                        (
                            "))",
                            Face::syntax(markdown::SyntaxRole::Plain).with_diff_bg(bg),
                        ),
                    ]
                }
                5 => vec![(" 35   }", Face::MUTED)],
                _ => Vec::new(),
            }
        };
        let line = markdown::spans(&parts, width)
            .into_iter()
            .next()
            .unwrap_or_default();
        markdown::write_clipped_padded_line(out, &line, false, width, width, self.theme)
            .map_err(ServiceError::storage)?;
        Ok(())
    }

    fn render_menu_overlay(&self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        let Some(menu) = &self.menu else {
            return Ok(());
        };
        let w = usize::from(layout.width);
        let anchor = layout.transcript.end();
        let top_bound = layout.transcript.start + u16::from(layout.header.len > 0);
        let max = usize::from(anchor.saturating_sub(top_bound)).min(8);
        if max == 0 {
            return Ok(());
        }
        let is_theme_menu = matches!(
            menu.choices.first().map(|c| &c.value),
            Some(Selection::Theme(_))
        );
        let split_preview = is_theme_menu && w >= 68;
        if split_preview {
            const PREVIEW_ROWS: usize = 6;
            let content_rows = menu.choices.len().max(PREVIEW_ROWS).min(max);
            let start = menu.selected.saturating_sub(content_rows.saturating_sub(1));
            let visible: Vec<_> = menu.choices.iter().skip(start).take(content_rows).collect();
            let has_footer_hint = visible.len() < content_rows;
            let top = anchor.saturating_sub((content_rows + 1) as u16);
            let left_w = (w * 36 / 100).clamp(24, 34);
            let preview_w = w.saturating_sub(left_w + 1);

            let header_text = clip(
                if has_footer_hint {
                    " Theme"
                } else {
                    " Theme · ↑↓ · Enter · Esc"
                },
                left_w,
            );
            let header_pad = " ".repeat(left_w.saturating_sub(header_text.width()));
            queue!(
                out,
                cursor::MoveTo(0, top),
                style::Print(
                    format!("{header_text}{header_pad}")
                        .with(self.theme.accent)
                        .on(self.theme.background)
                ),
                style::Print("│".with(self.theme.subtle).on(self.theme.background))
            )
            .map_err(ServiceError::storage)?;
            self.render_theme_preview_row(out, 0, preview_w, true)?;

            for row_idx in 0..content_rows {
                queue!(out, cursor::MoveTo(0, top + 1 + row_idx as u16))
                    .map_err(ServiceError::storage)?;
                if let Some(choice) = visible.get(row_idx) {
                    let selected = start + row_idx == menu.selected;
                    let text = clip(
                        &format!(" {} {}", if selected { "›" } else { " " }, choice.label),
                        left_w,
                    );
                    let pad = " ".repeat(left_w.saturating_sub(text.width()));
                    let cell = format!("{text}{pad}");
                    queue!(
                        out,
                        style::Print(if selected {
                            cell.with(self.theme.selected_text)
                                .on(self.theme.selected_background)
                        } else {
                            cell.with(self.theme.text).on(self.theme.background)
                        })
                    )
                    .map_err(ServiceError::storage)?;
                } else {
                    let hint = if row_idx + 1 == content_rows {
                        clip(" Enter select · Esc cancel", left_w)
                    } else {
                        String::new()
                    };
                    let pad = " ".repeat(left_w.saturating_sub(hint.width()));
                    queue!(
                        out,
                        style::Print(
                            format!("{hint}{pad}")
                                .with(self.theme.muted)
                                .on(self.theme.background)
                        )
                    )
                    .map_err(ServiceError::storage)?;
                }
                queue!(
                    out,
                    style::Print("│".with(self.theme.subtle).on(self.theme.background))
                )
                .map_err(ServiceError::storage)?;
                self.render_theme_preview_row(out, row_idx, preview_w, false)?;
            }
            return Ok(());
        }

        let start = menu.selected.saturating_sub(max.saturating_sub(1));
        let top = anchor.saturating_sub((menu.choices.len().min(max) + 1) as u16);
        queue!(
            out,
            cursor::MoveTo(0, top),
            terminal::Clear(ClearType::CurrentLine),
            style::Print(
                clip(
                    &format!(
                        " {}{}",
                        menu.title,
                        if self.resume_menu.is_some()
                            || matches!(
                                menu.choices.first().map(|c| &c.value),
                                Some(
                                    Selection::Model(_)
                                        | Selection::Inspector { .. }
                                        | Selection::Permission { .. }
                                )
                            )
                        {
                            ""
                        } else {
                            " · ↑↓ select · Enter · Esc"
                        }
                    ),
                    w
                )
                .with(self.theme.accent)
            )
        )
        .map_err(ServiceError::storage)?;
        for (i, choice) in menu.choices.iter().skip(start).take(max).enumerate() {
            let text = clip(
                &format!(
                    " {} {}",
                    if start + i == menu.selected {
                        "›"
                    } else {
                        " "
                    },
                    match &choice.value {
                        Selection::Model(model) =>
                            format!("{}   ‹ {} ›", choice.label, model.level().label),
                        _ => choice.label.clone(),
                    }
                ),
                w,
            );
            queue!(
                out,
                cursor::MoveTo(0, top + 1 + i as u16),
                style::SetBackgroundColor(self.theme.background),
                terminal::Clear(ClearType::CurrentLine),
                style::Print(if start + i == menu.selected {
                    text.with(self.theme.selected_text)
                        .on(self.theme.selected_background)
                } else {
                    text.with(self.theme.text).on(self.theme.background)
                })
            )
            .map_err(ServiceError::storage)?;
        }
        Ok(())
    }

    fn render_shortcuts_overlay(&self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        if !self.shortcuts_open
            || self.menu.is_some()
            || self.history_clear_confirmation.is_some()
            || self.inspector_zoom
            || layout.transcript.len < 2
        {
            return Ok(());
        }
        let anchor = layout.transcript.end();
        let avail_rows = usize::from(layout.transcript.len);
        let safe_w = Self::safe_row_width(layout.transcript.start, layout.width, layout.height);
        if safe_w == 0 {
            return Ok(());
        }
        let rows = shortcuts_panel_rows(safe_w);
        let total_body = rows.len();
        let visible_body = total_body.min(avail_rows.saturating_sub(1));
        if visible_body == 0 {
            return Ok(());
        }
        let max_scroll = total_body.saturating_sub(visible_body);
        let scroll = self.shortcuts_scroll.min(max_scroll);
        let panel_height = 1 + visible_body;
        let top = anchor.saturating_sub(panel_height as u16);

        // Top border row: `─ Shortcuts ─────────────── [range · ↑↓ scroll · ]Esc / ? close ─`
        let header_w = Self::safe_row_width(top, layout.width, layout.height);
        let title = " Shortcuts ";
        let controls = if max_scroll > 0 {
            format!(
                " {}-{}/{} · ↑↓ scroll · Esc/? close ",
                scroll + 1,
                scroll + visible_body,
                total_body
            )
        } else {
            " Esc / ? close ".to_owned()
        };
        let mut header_line: markdown::Line = Vec::new();
        let title_w = title.width();
        let ctrl_w = controls.width();
        if header_w >= title_w + ctrl_w + 4 {
            let mid_rules = header_w - title_w - ctrl_w - 2;
            header_line.push(Span {
                text: "─".into(),
                face: Face::SUBTLE,
            });
            header_line.push(Span {
                text: title.into(),
                face: Face::ACCENT_BOLD,
            });
            header_line.push(Span {
                text: "─".repeat(mid_rules),
                face: Face::SUBTLE,
            });
            header_line.push(Span {
                text: controls,
                face: Face::MUTED,
            });
            header_line.push(Span {
                text: "─".into(),
                face: Face::SUBTLE,
            });
        } else if header_w >= title_w + 2 {
            let right_rules = header_w - title_w - 1;
            header_line.push(Span {
                text: "─".into(),
                face: Face::SUBTLE,
            });
            header_line.push(Span {
                text: title.into(),
                face: Face::ACCENT_BOLD,
            });
            header_line.push(Span {
                text: "─".repeat(right_rules),
                face: Face::SUBTLE,
            });
        } else {
            header_line.push(Span {
                text: clip(" Shortcuts", header_w),
                face: Face::ACCENT_BOLD,
            });
        }
        let header_rendered_w: usize = header_line.iter().map(|s| s.text.width()).sum();
        queue!(out, cursor::MoveTo(0, top)).map_err(ServiceError::storage)?;
        markdown::write_line(out, &header_line, self.theme).map_err(ServiceError::storage)?;
        Self::pad_and_clear_row(out, top, header_rendered_w, layout, self.theme)?;

        for (i, line) in rows.iter().skip(scroll).take(visible_body).enumerate() {
            let y = top + 1 + i as u16;
            let rendered_w: usize = line.iter().map(|s| s.text.width()).sum();
            queue!(out, cursor::MoveTo(0, y)).map_err(ServiceError::storage)?;
            markdown::write_line(out, line, self.theme).map_err(ServiceError::storage)?;
            Self::pad_and_clear_row(out, y, rendered_w, layout, self.theme)?;
        }
        Ok(())
    }

    fn render_history_clear_overlay(
        &self,
        out: &mut impl Write,
        layout: FrameLayout,
    ) -> Result<Option<(u16, u16)>> {
        let Some(confirmation) = &self.history_clear_confirmation else {
            return Ok(None);
        };
        let w = usize::from(layout.width);
        let top = layout.transcript.end().saturating_sub(4);
        let prefix = " Type DELETE, then Enter: ";
        let lines = [
            format!(" Delete ALL {} chats?", confirmation.provider),
            format!(" Account: {}", confirmation.account),
            " Includes chats created outside anticrumb.".to_owned(),
            format!("{prefix}{}", confirmation.typed),
        ];
        for (index, line) in lines.iter().enumerate() {
            queue!(
                out,
                cursor::MoveTo(0, top + index as u16),
                style::SetBackgroundColor(self.theme.background),
                terminal::Clear(ClearType::CurrentLine),
                style::Print(clip(line, w).with(if index == 0 {
                    self.theme.accent
                } else {
                    self.theme.text
                }))
            )
            .map_err(ServiceError::storage)?;
        }
        Ok(Some((
            (prefix.width() + confirmation.typed.width()).min(w - 1) as u16,
            top + 3,
        )))
    }

    fn render_subagents(&mut self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        let w = usize::from(layout.width);
        let count = usize::from(layout.subagents.len);
        let subagents_selected =
            self.selection.is_some() && self.selection_target == SelectionTarget::Subagents;
        let lines: Vec<markdown::Line> = if subagents_selected {
            self.selection
                .as_ref()
                .map(|s| s.lines.iter().take(count).cloned().collect())
                .unwrap_or_default()
        } else {
            let lines: Vec<markdown::Line> = self
                .subagents
                .iter()
                .take(count)
                .enumerate()
                .map(|(idx, sub)| {
                    let sub_parts = self.subagent_line_spans(idx, sub, w);
                    let sub_refs: Vec<(&str, Face)> = sub_parts
                        .iter()
                        .map(|(text, face)| (text.as_str(), *face))
                        .collect();
                    markdown::spans(&sub_refs, w.max(1))
                        .into_iter()
                        .next()
                        .unwrap_or_default()
                })
                .collect();
            if self.selection.is_none() {
                self.painted_subagents = lines.clone();
            }
            lines
        };
        for (idx, sub_line) in lines.iter().enumerate() {
            let sub_row = layout.subagents.start + idx as u16;
            queue!(out, cursor::MoveTo(0, sub_row)).map_err(ServiceError::storage)?;
            markdown::write_line(out, sub_line, self.theme).map_err(ServiceError::storage)?;
            let sub_width: usize = sub_line.iter().map(|s| s.text.width()).sum();
            if sub_width < w {
                queue!(
                    out,
                    style::SetBackgroundColor(self.theme.background),
                    terminal::Clear(ClearType::UntilNewLine)
                )
                .map_err(ServiceError::storage)?;
            }
            if subagents_selected {
                if let Some((column, selected)) = self
                    .selection
                    .as_ref()
                    .and_then(|selection| selection.fragment_spans(idx))
                {
                    if !selected.is_empty() {
                        queue!(out, cursor::MoveTo(column.min(w - 1) as u16, sub_row))
                            .map_err(ServiceError::storage)?;
                        markdown::write_selection(out, &selected, self.theme)
                            .map_err(ServiceError::storage)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Safe column boundary for a row to prevent bottom-right autowrap scrolling.
    ///
    /// On Windows Console (conhost / ConPTY) and other terminal emulators,
    /// writing a character into the bottom-right corner (`col = width - 1, row = height - 1`)
    /// immediately triggers an autowrap scroll-up. This causes screen jitter on
    /// every frame and displaces the composer prompt row from the caret position.
    /// Restricting the bottom row's printable width to `width - 1` and clearing
    /// via escape sequence prevents scrolling across Windows, macOS, and Linux.
    pub(super) fn safe_row_width(row: u16, width: u16, height: u16) -> usize {
        let w = usize::from(width);
        if row.saturating_add(1) >= height {
            w.saturating_sub(1)
        } else {
            w
        }
    }

    /// Clear and pad the remainder of a line with the theme background without
    /// printing into the bottom-right corner cell.
    fn pad_and_clear_row(
        out: &mut impl Write,
        row: u16,
        rendered_width: usize,
        layout: FrameLayout,
        theme: crate::tui::theme::Theme,
    ) -> Result<()> {
        let safe_w = Self::safe_row_width(row, layout.width, layout.height);
        if safe_w > rendered_width {
            queue!(
                out,
                style::Print(
                    " ".repeat(safe_w - rendered_width)
                        .with(theme.muted)
                        .on(theme.background)
                )
            )
            .map_err(ServiceError::storage)?;
        }
        queue!(
            out,
            style::SetBackgroundColor(theme.background),
            terminal::Clear(ClearType::UntilNewLine)
        )
        .map_err(ServiceError::storage)?;
        Ok(())
    }

    fn render_footer(&mut self, out: &mut impl Write, layout: FrameLayout) -> Result<()> {
        if layout.footer.len == 0 {
            if self.selection.is_none() {
                self.painted_footer.clear();
            }
            return Ok(());
        }
        let safe_w = Self::safe_row_width(layout.footer.start, layout.width, layout.height);
        let footer_selected =
            self.selection.is_some() && self.selection_target == SelectionTarget::Footer;
        let status_line = if footer_selected {
            self.selection
                .as_ref()
                .and_then(|s| s.lines.first())
                .cloned()
                .unwrap_or_default()
        } else {
            let status_parts = self.status_spans(safe_w);
            let status_refs: Vec<(&str, Face)> = status_parts
                .iter()
                .map(|(text, face)| (text.as_str(), *face))
                .collect();
            let line = markdown::spans(&status_refs, safe_w.max(1))
                .into_iter()
                .next()
                .unwrap_or_default();
            if self.selection.is_none() {
                self.painted_footer = vec![line.clone()];
            }
            line
        };
        let rendered_width: usize = status_line.iter().map(|span| span.text.width()).sum();
        queue!(out, cursor::MoveTo(0, layout.footer.start)).map_err(ServiceError::storage)?;
        markdown::write_line(out, &status_line, self.theme).map_err(ServiceError::storage)?;
        Self::pad_and_clear_row(out, layout.footer.start, rendered_width, layout, self.theme)?;
        if footer_selected {
            if let Some((column, selected)) = self
                .selection
                .as_ref()
                .and_then(|selection| selection.fragment_spans(0))
            {
                if !selected.is_empty() && column < safe_w {
                    queue!(out, cursor::MoveTo(column as u16, layout.footer.start))
                        .map_err(ServiceError::storage)?;
                    markdown::write_selection(out, &selected, self.theme)
                        .map_err(ServiceError::storage)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn render_frame(
        &mut self,
        out: &mut impl Write,
        width: u16,
        height: u16,
    ) -> Result<()> {
        let Some((layout, editor_rows)) = self.compute_layout(width, height) else {
            self.clear_painted_regions();
            self.clear_selection();
            return Ok(());
        };
        // Only clear the whole screen on the initial frame or after a terminal resize.
        // Steady-state frames overwrite every row in place with ClearType::UntilNewLine so
        // terminals without DEC Mode 2026 (or large frames split across PTY reads) never flash.
        let full_clear = self.selection.is_none() && self.painted_size != (width, height);
        queue!(
            out,
            terminal::BeginSynchronizedUpdate,
            style::SetBackgroundColor(self.theme.background),
            style::SetForegroundColor(self.theme.text),
            cursor::Hide
        )
        .map_err(ServiceError::storage)?;
        if full_clear {
            // ED2 advances VTE's logical rows even in the alternate screen.
            // Erase from the home position to repaint without accumulating rows.
            queue!(
                out,
                cursor::MoveTo(0, 0),
                terminal::Clear(ClearType::FromCursorDown)
            )
            .map_err(ServiceError::storage)?;
        }
        self.render_header(out, layout)?;
        self.render_transcript(out, layout, full_clear)?;
        self.render_inspector_zoom_overlay(out, layout)?;
        self.render_hint(out, layout)?;
        self.render_composer(out, layout, &editor_rows)?;
        self.render_completion(out, layout)?;
        self.render_menu_overlay(out, layout)?;
        self.render_shortcuts_overlay(out, layout)?;
        let cursor_position = self
            .render_history_clear_overlay(out, layout)?
            .unwrap_or(layout.cursor);
        self.render_subagents(out, layout)?;
        self.render_footer(out, layout)?;
        queue!(
            out,
            cursor::MoveTo(cursor_position.0, cursor_position.1),
            cursor::Show,
            terminal::EndSynchronizedUpdate
        )
        .map_err(ServiceError::storage)
    }
}
