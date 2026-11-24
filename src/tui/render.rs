//! Terminal frame, status bar, and subagent banner rendering.
use super::{
    commands::Selection,
    debug_log::DebugLogger,
    layout::FrameLayout,
    markdown::{self, Face, Span},
    reducer::{SelectionTarget, State, SubagentView},
    transcript::clip,
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
        let mut left = clip_spans(parts, left_max);
        let left_w = spans_width(&left);
        let pad = width.saturating_sub(left_w + right_w);
        if pad > 0 {
            left.push((" ".repeat(pad), Face::MUTED));
        }
        left.push((right_text, daemon_face));
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
        let layout = FrameLayout::compute(
            &self.layout,
            width,
            height,
            rows.len(),
            cursor,
            self.mention_panel_rows(),
            self.subagents.len(),
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
            queue!(out, terminal::Clear(ClearType::UntilNewLine)).map_err(ServiceError::storage)?;
        }
        Ok(())
    }

    pub(super) fn render_transcript(
        &mut self,
        out: &mut impl Write,
        layout: FrameLayout,
        full_clear: bool,
    ) -> Result<()> {
        if layout.transcript.len == 0 {
            self.painted_transcript.clear();
            self.painted_all_transcript.clear();
            self.scroll_bottom_button = None;
            self.painted_size = (layout.width, layout.height);
            self.painted_layout = Some(layout);
            return Ok(());
        }
        let w = usize::from(layout.width);
        let viewport = usize::from(layout.transcript.len);
        let transcript_selection = self
            .selection
            .as_ref()
            .filter(|_| self.selection_target == SelectionTarget::Transcript);
        let (start, effective_scroll, painted, all_snapshot) = if let Some(selection) =
            transcript_selection
        {
            let (start, end) = self.selection_view_range(selection.lines.len(), viewport);
            let eff = selection.lines.len().saturating_sub(end);
            (start, eff, selection.lines[start..end].to_vec(), None)
        } else {
            // Release the previous snapshot first so the cache appends in place.
            self.painted_all_transcript.clear();
            let all = self.transcript.lines(&self.stream, w);
            self.sync_scroll_anchor(all.len(), all.evicted_lines(), w, all.visible_stream_len());
            let (start, end) = self.selection_view_range(all.len(), viewport);
            let eff = all.len().saturating_sub(end);
            let painted = all.slice(start, end);
            (start, eff, painted, Some(all))
        };
        for (row, line) in painted.iter().enumerate() {
            let y = layout.transcript.start + row as u16;
            queue!(out, cursor::MoveTo(0, y)).map_err(ServiceError::storage)?;
            markdown::write_line(out, line, self.theme).map_err(ServiceError::storage)?;
            let line_width: usize = line.iter().map(|s| s.text.width()).sum();
            if line_width < w {
                queue!(out, terminal::Clear(ClearType::UntilNewLine))
                    .map_err(ServiceError::storage)?;
            }
            if let Some((column, selected)) = self
                .selection
                .as_ref()
                .filter(|_| self.selection_target == SelectionTarget::Transcript)
                .and_then(|selection| selection.fragment_spans(start + row))
            {
                if !selected.is_empty() {
                    queue!(out, cursor::MoveTo(column.min(w - 1) as u16, y))
                        .map_err(ServiceError::storage)?;
                    markdown::write_selection(out, &selected, self.theme)
                        .map_err(ServiceError::storage)?;
                }
            }
        }
        if !full_clear {
            let clear_from = usize::from(layout.transcript.start) + painted.len();
            for row in clear_from..usize::from(layout.transcript.end()) {
                queue!(
                    out,
                    cursor::MoveTo(0, row as u16),
                    style::SetBackgroundColor(self.theme.background),
                    terminal::Clear(ClearType::UntilNewLine)
                )
                .map_err(ServiceError::storage)?;
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
            let raw_btn = if safe_w >= 22 {
                badge.label(spinner)
            } else {
                badge.compact_label(spinner)
            };
            let btn_text = clip(&raw_btn, safe_w);
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

        let mut hint_is_live = false;
        let hint = if self.history_clear_confirmation.is_some() {
            " Type DELETE + Enter to confirm · Esc cancels".into()
        } else if self.selection.is_some() {
            " Drag to select · Release to copy · Esc cancels".into()
        } else if let Some(notice) = resume_notice {
            format!(" {notice}")
        } else if self.history_index < self.history.len() {
            format!(
                " History {}/{} · ↑↓ edit/history · Enter send",
                self.history_index + 1,
                self.history.len()
            )
        } else if !self.files.is_empty() {
            format!(
                " {} file(s): {} · Enter to send",
                self.files.len(),
                self.files
                    .iter()
                    .map(|f| f.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if let Some(c) = &self.completion {
            format!(
                " Tab → {}",
                c.values
                    .iter()
                    .take(3)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("  ")
            )
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
                format!(" {spinner} {live_text}{suffix}")
            } else {
                format!(" {spinner} {live_text}")
            }
        } else {
            " Alt+Enter newline · ↑↓ edit/history · Alt+E expand paste · Wheel/PgUp scroll · /help"
                .into()
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
                    ("fn", Face::ACCENT_BOLD),
                    (" summarize", Face::HEADING),
                    ("(users: &[User]) -> ", Face::TEXT),
                    ("String", Face::CODE),
                    (" {", Face::TEXT),
                ],
                1 => vec![
                    (" 32 ", Face::MUTED),
                    (
                        "-   let active = users.iter().filter(|u| u.ok).count();",
                        Face::DANGER,
                    ),
                ],
                2 => vec![
                    (" 32 ", Face::MUTED),
                    (
                        "+   let n = users.iter().filter(|u| u.is_active()).count();",
                        Face::HIGHLIGHT,
                    ),
                ],
                3 => vec![
                    (" 33     ", Face::MUTED),
                    ("let", Face::ACCENT),
                    (" label = ", Face::TEXT),
                    ("\"active\"", Face::CODE),
                    (";", Face::TEXT),
                ],
                4 => vec![
                    (" 34 ", Face::MUTED),
                    (
                        "+   format!(\"{n} {label}: {}\", names.join(\", \"))",
                        Face::HIGHLIGHT,
                    ),
                ],
                5 => vec![(" 35   }", Face::MUTED)],
                _ => Vec::new(),
            }
        };
        let line = markdown::spans(&parts, width)
            .into_iter()
            .next()
            .unwrap_or_default();
        let rendered_width: usize = line.iter().map(|s| s.text.width()).sum();
        markdown::write_line(out, &line, self.theme).map_err(ServiceError::storage)?;
        if rendered_width < width {
            queue!(
                out,
                style::SetBackgroundColor(self.theme.background),
                terminal::Clear(ClearType::UntilNewLine)
            )
            .map_err(ServiceError::storage)?;
        }
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
                                Some(Selection::Model(_))
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
                queue!(out, terminal::Clear(ClearType::UntilNewLine))
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
        let full_clear = self.selection.is_none();
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
        self.render_hint(out, layout)?;
        self.render_composer(out, layout, &editor_rows)?;
        self.render_completion(out, layout)?;
        self.render_menu_overlay(out, layout)?;
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
