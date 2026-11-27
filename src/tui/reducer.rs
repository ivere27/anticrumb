//! Core TUI state, event reducer, debug/clipboard helpers, and scroll/selection logic.
use super::{
    clipboard,
    commands::{Choice, Completion, HistoryClearConfirmation, Menu, Selection},
    debug_log::DebugLogger,
    editor::{Draft, Editor},
    inspector::{
        self, BlockId, BlockRowKind, InspectorBadgeAction, PaintedInspectorPane, PaintedToolBlock,
        ToolBlockStore,
    },
    layout::{
        FrameLayout, HorizontalSplit, InspectorConfig, InspectorMode, LayoutConfig,
        INSPECTOR_MAX_WIDTH_PCT, INSPECTOR_MIN_WIDTH_PCT,
    },
    markdown::{self, Face},
    mentions::{self, MentionKind, MentionMatch, MentionPopup},
    preferences::{self, Preferences},
    selection, theme,
    transcript::{clip, inline_notice_parts, Entry, SplitCacheKey, Transcript, TranscriptLines},
    usage,
    worker::{Command, Update},
};
use crate::{agent::OutputStream, cli::PromptOptions};
use crossterm::event::{self, MouseButton, MouseEventKind};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

fn job_session_is_temporary(job: &Value) -> Option<bool> {
    match job["request"]["session_mode"].as_str()? {
        "temporary" => Some(true),
        "chat" => Some(false),
        _ => None,
    }
}

pub(super) trait EffectSink {
    fn send_command(&mut self, command: Command) -> std::result::Result<(), String>;
    fn write_terminal(&mut self, sequence: &str) -> std::result::Result<(), String>;
}

impl EffectSink for mpsc::SyncSender<Command> {
    fn send_command(&mut self, command: Command) -> std::result::Result<(), String> {
        self.try_send(command).map_err(|e| {
            let cmd = match &e {
                mpsc::TrySendError::Full(c) | mpsc::TrySendError::Disconnected(c) => c,
            };
            crate::input::cleanup_temp_files(cmd.temp_files());
            e.to_string()
        })
    }

    fn write_terminal(&mut self, _sequence: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct LastClick {
    pub at: Instant,
    pub column: u16,
    pub row: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionTarget {
    Transcript,
    Inspector,
    Hint,
    Composer,
    Subagents,
    Footer,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SelectionDrag {
    pub anchor_viewport_row: usize,
    pub column: u16,
    pub row: u16,
    pub moved: bool,
    pub entered_top_at: Option<Instant>,
    pub last_autoscroll: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingToolApproval {
    pub id: String,
    pub step: usize,
    pub source: Option<String>,
    pub call_count: usize,
}

#[derive(Clone, Debug)]
pub(super) struct SubagentView {
    pub id: String,
    pub label: String,
    pub phase: String,
    pub detail: String,
    pub started: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScrollBottomBadge {
    Idle,
    NewOutput,
    Streaming,
}

impl ScrollBottomBadge {
    pub(super) fn label(self, spinner: &str) -> String {
        match self {
            Self::Idle => "[ ↓ Jump to bottom ]".into(),
            Self::NewOutput => "[ ↓ New output ]".into(),
            Self::Streaming => format!("[ {spinner} ↓ New output ]"),
        }
    }

    pub(super) fn compact_label(self, spinner: &str) -> String {
        match self {
            Self::Idle => "[ ↓ Bottom ]".into(),
            Self::NewOutput => "[ ↓ New ]".into(),
            Self::Streaming => format!("[ {spinner} ↓ New ]"),
        }
    }
}

pub(super) struct State {
    pub options: PromptOptions,
    /// Whether `Shift+Tab` toggles `CHAT <-> YOLO` instead of `CHAT <-> AGENT`.
    /// Armed only by explicit CLI `--yolo` or `/mode yolo` (and disarmed by
    /// `/mode agent` or narrowing `/permissions`). Never armed implicitly by `/resume`.
    pub yolo_toggle_armed: bool,
    pub preferences: Option<Preferences>,
    pub theme: theme::Theme,
    pub theme_preview_origin: Option<theme::Theme>,
    pub catalog: Value,
    pub conversation_id: Option<String>,
    pub active: Option<String>,
    pub conversation_temporary: Option<bool>,
    pub pending_temporary: Option<bool>,
    pub phase: String,
    pub editor: Editor,
    pub transcript: Transcript,
    pub status: Value,
    pub last_status_at: Option<Instant>,
    pub status_disconnected: bool,
    pub remote_tls_pin: Option<String>,
    pub remote_tls_endpoint: Option<String>,
    pub last_login_status: Value,
    pub last_chat_cleanup: Value,
    pub web_usage: Option<Value>,
    pub web_usage_stale: bool,
    pub started: Instant,
    pub active_model: Option<String>,
    pub in_chars: usize,
    pub out_chars: usize,
    pub history: Vec<Draft>,
    pub history_index: usize,
    pub saved_draft: Draft,
    pub files: Vec<PathBuf>,
    pub pending: Option<(Draft, Vec<PathBuf>)>,
    pub menu: Option<Menu>,
    pub resume_menu: Option<Menu>,
    pub resume_notice: Option<String>,
    pub deleting_conversation: Option<String>,
    pub history_clear_confirmation: Option<HistoryClearConfirmation>,
    pub clearing_web_history: bool,
    pub web_history_status: Value,
    pub completion: Option<Completion>,
    pub mention: Option<MentionPopup>,
    pub mention_dismissed: Option<(usize, String)>,
    pub recent_mentions: Vec<preferences::RecentMention>,
    pub next_mention_id: u64,
    pub scroll: usize,
    pub last_response: String,
    pub copy_response: String,
    pub painted_transcript: Vec<markdown::Line>,
    pub painted_all_transcript: TranscriptLines,
    pub painted_transcript_start: usize,
    pub painted_effective_scroll: usize,
    pub scroll_anchor_width: usize,
    pub scroll_anchor_total_lines: usize,
    pub scroll_anchor_evicted_lines: u64,
    pub scroll_relative: bool,
    pub scroll_anchor_deficit: Option<usize>,
    pub scroll_anchor_stream_len: usize,
    pub scroll_anchor_revision: u64,
    pub scroll_anchor_tool_rev: u64,
    pub has_unread_below: bool,
    pub painted_hint: Vec<markdown::Line>,
    pub painted_composer: Vec<markdown::Line>,
    pub painted_subagents: Vec<markdown::Line>,
    pub painted_footer: Vec<markdown::Line>,
    pub painted_blocks: Vec<PaintedToolBlock>,
    pub painted_inspector: Option<PaintedInspectorPane>,
    pub painted_inline_inspector_target: Option<BlockId>,
    pub painted_size: (u16, u16),
    pub painted_layout: Option<FrameLayout>,
    pub scroll_bottom_button: Option<(u16, u16, u16)>,
    pub layout: LayoutConfig,
    pub tool_blocks: ToolBlockStore,
    pub inspector_zoom: bool,
    pub inspector_zoom_scroll: usize,
    pub shortcuts_open: bool,
    pub shortcuts_scroll: usize,
    pub inspector_preview_origin: Option<InspectorConfig>,
    pub inspector_last_active_mode: InspectorMode,
    pub divider_drag: bool,
    pub selection: Option<selection::Selection>,
    pub selection_target: SelectionTarget,
    pub selection_drag: Option<SelectionDrag>,
    pub last_click: Option<LastClick>,
    pub clipboard_notice: Option<String>,
    pub stream: String,
    pub thinking_preview: String,
    pub subagents: Vec<SubagentView>,
    pub tool_approvals: VecDeque<PendingToolApproval>,
    pub answered_approvals: HashSet<String>,
    pub debug: DebugLogger,
    pub exit_requested: bool,
    pub last_ctrl_c: Option<Instant>,
}

fn status_visible_changed(prev: &Value, next: &Value) -> bool {
    let (Some(p), Some(n)) = (prev.as_object(), next.as_object()) else {
        return prev != next;
    };
    p.iter().any(|(k, v)| k != "runtime" && n.get(k) != Some(v))
        || n.iter().any(|(k, v)| k != "runtime" && p.get(k) != Some(v))
}

impl State {
    pub(super) fn new(mut options: PromptOptions) -> Self {
        options.no_shell = !options.effective_permissions().shell;
        let yolo_toggle_armed = options.yolo;
        let files = std::mem::take(&mut options.files);
        let mut editor = Editor::default();
        editor.set(options.words.join(" "));
        Self {
            options,
            yolo_toggle_armed,
            preferences: None,
            theme: theme::THEMES[0],
            theme_preview_origin: None,
            catalog: Value::Null,
            files,
            editor,
            conversation_id: None,
            active: None,
            conversation_temporary: None,
            pending_temporary: None,
            phase: "ready".into(),
            transcript: Transcript::default(),
            status: json!({}),
            last_status_at: None,
            status_disconnected: false,
            remote_tls_pin: None,
            remote_tls_endpoint: None,
            last_login_status: Value::Null,
            last_chat_cleanup: Value::Null,
            web_usage: None,
            web_usage_stale: false,
            started: Instant::now(),
            active_model: None,
            in_chars: 0,
            out_chars: 0,
            history: Vec::new(),
            history_index: 0,
            saved_draft: Draft::default(),
            pending: None,
            menu: None,
            resume_menu: None,
            resume_notice: None,
            deleting_conversation: None,
            history_clear_confirmation: None,
            clearing_web_history: false,
            web_history_status: Value::Null,
            completion: None,
            mention: None,
            mention_dismissed: None,
            recent_mentions: Vec::new(),
            next_mention_id: 1,
            scroll: 0,
            last_response: String::new(),
            copy_response: String::new(),
            painted_transcript: Vec::new(),
            painted_all_transcript: TranscriptLines::default(),
            painted_transcript_start: 0,
            painted_effective_scroll: 0,
            scroll_anchor_width: 0,
            scroll_anchor_total_lines: 0,
            scroll_anchor_evicted_lines: 0,
            scroll_relative: false,
            scroll_anchor_deficit: None,
            scroll_anchor_stream_len: 0,
            scroll_anchor_revision: 0,
            scroll_anchor_tool_rev: 0,
            has_unread_below: false,
            painted_hint: Vec::new(),
            painted_composer: Vec::new(),
            painted_subagents: Vec::new(),
            painted_footer: Vec::new(),
            painted_blocks: Vec::new(),
            painted_inspector: None,
            painted_inline_inspector_target: None,
            painted_size: (0, 0),
            painted_layout: None,
            scroll_bottom_button: None,
            layout: LayoutConfig::default(),
            tool_blocks: ToolBlockStore::default(),
            inspector_zoom: false,
            inspector_zoom_scroll: 0,
            shortcuts_open: false,
            shortcuts_scroll: 0,
            inspector_preview_origin: None,
            inspector_last_active_mode: InspectorMode::Inline,
            divider_drag: false,
            selection: None,
            selection_target: SelectionTarget::Transcript,
            selection_drag: None,
            last_click: None,
            clipboard_notice: None,
            stream: String::new(),
            thinking_preview: String::new(),
            subagents: Vec::new(),
            tool_approvals: VecDeque::new(),
            answered_approvals: HashSet::new(),
            debug: DebugLogger::default(),
            exit_requested: false,
            last_ctrl_c: None,
        }
    }

    pub(super) fn busy(&self) -> bool {
        self.active.is_some()
            || self.pending.is_some()
            || self.deleting_conversation.is_some()
            || self.clearing_web_history
            || self.phase == "loading"
    }

    /// True only when visual animation (spinner frames) is actively needed.
    /// Static `Y/N` approval waiting pauses spinner wakeups for near-zero idle CPU.
    pub(super) fn needs_animation(&self) -> bool {
        let active_animating = self.active.is_some()
            && (self.tool_approvals.is_empty()
                || self.tool_blocks.has_running_tools()
                || !self.stream.is_empty()
                || !self.thinking_preview.is_empty()
                || !self.subagents.is_empty());
        active_animating
            || self.pending.is_some()
            || self.deleting_conversation.is_some()
            || self.clearing_web_history
            || self.phase == "loading"
    }

    #[cfg(test)]
    pub(super) fn logs(&self) -> String {
        self.transcript.plain_text()
    }

    fn max_transcript_bytes(&self) -> usize {
        if self.debug.level >= 2 {
            1024 * 1024
        } else {
            128 * 1024
        }
    }

    pub(super) fn seal_active_tool_block(&mut self, cancel_incomplete: bool) {
        if let Some((block_id, plain)) = self
            .tool_blocks
            .seal_active_block(self.layout.inspector.after_turn, cancel_incomplete)
        {
            self.push_entry_raw(&plain, Entry::ToolBlock(block_id));
        }
    }

    pub(super) fn push_entry_raw(&mut self, plain: &str, entry: Entry) {
        let max_bytes = self.max_transcript_bytes();
        let evicted = self.transcript.push(plain, entry, max_bytes);
        if !evicted.is_empty() {
            self.tool_blocks.remove_blocks(&evicted);
        }
    }

    pub(super) fn push_entry(&mut self, plain: &str, entry: Entry) {
        // Ordering invariant: seal any active tool block immediately before pushing
        // any non-tool entry so prose, prompts, and summaries stay in chronological order.
        if !matches!(entry, Entry::ToolBlock(_)) {
            self.seal_active_tool_block(false);
        }
        self.push_entry_raw(plain, entry);
    }

    pub(super) fn log(&mut self, text: &str) {
        self.push_entry(text, Entry::Notice(text.into()));
    }

    pub(super) fn log_markdown(&mut self, text: &str) {
        if self.debug.level > 0 {
            self.write_debug_log("assistant_markdown", text.trim_start_matches('\n'));
        }
        self.push_entry(text, Entry::Markdown(text.into()));
    }

    pub(super) fn log_heading(&mut self, text: &str) {
        self.push_entry(text, Entry::Heading(text.into()));
    }

    pub(super) fn log_command(&mut self, key: &str, description: &str) {
        let plain = format!("{key} {description}");
        self.push_entry(
            &plain,
            Entry::Command {
                key: key.into(),
                description: format!(" {description}"),
                highlight_key: false,
            },
        );
    }

    pub(super) fn log_shortcut(&mut self, key: &str, description: &str) {
        let plain = format!("{key} {description}");
        self.push_entry(
            &plain,
            Entry::Command {
                key: key.into(),
                description: format!(" {description}"),
                highlight_key: true,
            },
        );
    }

    pub(super) fn log_spans(&mut self, plain: &str, parts: Vec<(String, Face)>) {
        self.push_entry(plain, Entry::Spans(parts));
    }

    fn format_workspace_home(workspace: &std::path::Path) -> String {
        if let Some(home) = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
        {
            if let Ok(rel) = workspace.strip_prefix(&home) {
                return if rel.as_os_str().is_empty() {
                    "~".into()
                } else {
                    format!("~/{}", rel.display())
                };
            }
        }
        workspace.display().to_string()
    }

    pub(super) fn log_startup_banner(&mut self) {
        let notice = super::notices::pick_startup_notice();
        self.log_startup_banner_with_notice(notice.as_ref());
    }

    pub(super) fn log_startup_banner_with_notice(
        &mut self,
        notice: Option<&super::notices::NoticeEntry>,
    ) {
        let version = crate::build_info::display_version();
        let commit = crate::build_info::commit_head();
        let version_label = crate::build_info::format_tui_title(&version, commit.as_deref());
        let account_label = format!("{} · {}", self.provider_name(), self.options.account);
        let model_label = format!("{} ({})", self.model_label(), self.thinking_label());
        let workspace_label = Self::format_workspace_home(&self.options.workspace);

        // Theme-independent baked cookie + falling crumbs palette,
        // tuned for contrast on both dark and light terminal themes.
        const CRUST_TOP: Face = Face::rgb_bold(0xD9, 0x88, 0x38);
        const CRUST_LEFT: Face = Face::rgb(0xC4, 0x71, 0x2B);
        const CRUST_RIGHT: Face = Face::rgb(0xA8, 0x58, 0x20);
        const CRUST_BOTTOM: Face = Face::rgb(0x9E, 0x51, 0x1E);
        const DOUGH: Face = Face::rgb(0xEA, 0xB6, 0x5B);
        const DOUGH_SHADE: Face = Face::rgb(0xDC, 0xA0, 0x45);
        const CHIP: Face = Face::rgb(0x6B, 0x3A, 0x1E);
        const CHUNK_GOLD: Face = Face::rgb_bold(0xF2, 0xBC, 0x57);
        const CHUNK_CRUST: Face = Face::rgb_bold(0xD6, 0x7B, 0x29);
        const CRUMB_HONEY: Face = Face::rgb_bold(0xED, 0x9F, 0x38);
        const CRUMB_OAT: Face = Face::rgb(0xD9, 0xA7, 0x60);
        const CRUMB_CINNAMON: Face = Face::rgb(0xB8, 0x6B, 0x35);
        const CRUMB_AMBER: Face = Face::rgb_bold(0xDF, 0x86, 0x2F);
        const CRUMB_COCOA: Face = Face::rgb_bold(0x8F, 0x4B, 0x24);

        let mut row0 = vec![
            ("    ".into(), Face::TEXT),
            ("▄██████▄".into(), CRUST_TOP),
            ("      ".into(), Face::TEXT),
            (format!("anticrumb {version}"), Face::BOLD),
        ];
        if let Some(head) = commit {
            row0.push((format!(" ({head})"), Face::MUTED));
        }

        let row1 = vec![
            ("   ".into(), Face::TEXT),
            ("██".into(), CRUST_LEFT),
            ("▀█".into(), DOUGH),
            ("█".into(), CHIP),
            ("██▀".into(), DOUGH),
            ("█▌".into(), CRUST_RIGHT),
            ("     ".into(), Face::TEXT),
            (account_label.clone(), Face::MUTED),
        ];

        let row2 = vec![
            ("   ".into(), Face::TEXT),
            ("██".into(), CRUST_LEFT),
            ("▄██".into(), DOUGH_SHADE),
            ("█".into(), CHIP),
            ("█".into(), DOUGH_SHADE),
            ("  ".into(), Face::TEXT),
            ("▀".into(), CHUNK_GOLD),
            ("▄".into(), CHUNK_CRUST),
            ("    ".into(), Face::TEXT),
            (model_label.clone(), Face::TEXT),
        ];

        let row3 = vec![
            ("    ".into(), Face::TEXT),
            ("▀████▀".into(), CRUST_BOTTOM),
            ("  ".into(), Face::TEXT),
            ("▪".into(), CRUMB_HONEY),
            (" ".into(), Face::TEXT),
            ("·".into(), CRUMB_OAT),
            ("   ".into(), Face::TEXT),
            (workspace_label.clone(), Face::MUTED),
        ];

        let row4 = vec![
            ("      ".into(), Face::TEXT),
            ("·".into(), CRUMB_CINNAMON),
            ("  ".into(), Face::TEXT),
            ("▪".into(), CRUMB_AMBER),
            ("   ".into(), Face::TEXT),
            ("▪".into(), CRUMB_COCOA),
        ];

        self.log_spans("", Vec::new());
        self.log_spans(&version_label, row0);
        self.log_spans(&account_label, row1);
        self.log_spans(&model_label, row2);
        self.log_spans(&workspace_label, row3);
        self.log_spans("", row4);
        if let Some(entry) = notice {
            let lines = super::notices::format_notice_lines(entry);
            if !lines.is_empty() {
                self.log_spans("", Vec::new());
                for line in lines {
                    self.log_spans(&line.plain, line.spans);
                }
            }
        }
        self.log_spans("", Vec::new());
    }

    pub(super) fn log_kv(
        &mut self,
        prefix: &str,
        prefix_face: Face,
        key: &str,
        value: &str,
        value_face: Face,
    ) {
        let plain = format!("{key}: {value}");
        let mut parts = Vec::new();
        if !prefix.is_empty() {
            parts.push((prefix.to_owned(), prefix_face));
        }
        parts.push((format!("{key}: "), Face::MUTED));
        parts.push((value.to_owned(), value_face));
        self.log_spans(&plain, parts);
    }

    pub(super) fn log_prompt(&mut self, text: &str) {
        if self.debug.level > 0 {
            self.write_debug_log("user_prompt", text);
        }
        self.push_entry(&format!("\n› {text}"), Entry::Prompt(format!("\n{text}")));
    }

    pub(super) fn log_error(&mut self, text: &str) {
        if self.debug.level > 0 {
            self.write_debug_log("error", text);
        }
        self.push_entry(text, Entry::Error(text.into()));
    }

    pub(super) fn log_warning(&mut self, text: &str) {
        if self.debug.level > 0 {
            self.write_debug_log("warning", text);
        }
        let plain = format!("  Warning: {text}");
        self.log_spans(
            &plain,
            vec![
                ("  Warning: ".into(), Face::HEADING),
                (text.to_owned(), Face::MUTED),
            ],
        );
    }

    pub(super) fn respond_tool_approval(
        &mut self,
        approved: bool,
        sink: &mut dyn EffectSink,
    ) -> bool {
        let Some(agent_id) = self.active.clone() else {
            self.tool_approvals.clear();
            return false;
        };
        let Some(approval) = self.tool_approvals.pop_front() else {
            return false;
        };
        self.answered_approvals.insert(approval.id.clone());
        self.tool_blocks
            .on_approval_resolved(&approval.id, approved);
        self.phase = if !self.tool_approvals.is_empty() {
            "approval (Y/N)".into()
        } else if approved {
            "working".into()
        } else {
            "thinking".into()
        };
        self.send(
            sink,
            Command::ApproveTool {
                id: agent_id,
                approval_id: approval.id,
                approved,
            },
        );
        true
    }

    pub(super) fn write_debug_log(&mut self, tag: &str, body: &str) {
        if let Some(err) = self.debug.write(
            self.started,
            &self.options.workspace,
            &self.options.account,
            tag,
            body,
        ) {
            self.push_entry_raw(&err, Entry::Error(err.clone()));
        }
    }

    pub(super) fn log_debug(&mut self, tag: &str, detail: &str) {
        self.write_debug_log(tag, detail);
        let plain = format!("  ⚙ [{tag}] {detail}");
        self.push_entry_raw(
            &plain,
            Entry::Spans(vec![
                ("  ⚙ ".into(), Face::ACCENT),
                (format!("[{tag}] "), Face::ACCENT),
                (detail.to_owned(), Face::MUTED),
            ]),
        );
    }

    pub(super) fn log_debug_block(&mut self, tag: &str, header: &str, body: &str) {
        if header.is_empty() {
            self.write_debug_log(tag, body);
        } else {
            self.write_debug_log(tag, &format!("{header}\n{body}"));
        }
        let mut parts = vec![
            ("  ⚙ ".into(), Face::ACCENT),
            (format!("[{tag}] "), Face::ACCENT),
        ];
        if !header.is_empty() {
            parts.push((header.to_owned(), Face::MUTED));
        }
        for line in body.split('\n') {
            parts.push(("\n  │ ".into(), Face::MUTED));
            if !line.is_empty() {
                parts.push((line.to_owned(), Face::TEXT));
            }
        }
        let plain = if header.is_empty() {
            format!("  ⚙ [{tag}]\n{body}")
        } else {
            format!("  ⚙ [{tag}] {header}\n{body}")
        };
        self.push_entry_raw(&plain, Entry::Spans(parts));
    }

    pub(super) fn set_debug_level(&mut self, level: u8) {
        let level = level.min(2);
        self.debug.level = level;
        if let Some(data) = &self.preferences {
            if let Err(e) = data.set_debug_level(level) {
                self.log_error(&format!("Debug setting could not be saved: {e}"));
            }
        }
        let closed_path = if level == 0 {
            self.debug.close(self.started)
        } else {
            if let Some(err) = self
                .debug
                .ensure_file(&self.options.workspace, &self.options.account)
            {
                self.push_entry(&err, Entry::Error(err.clone()));
            }
            self.write_debug_log(
                "debug_level",
                &format!("changed to {level} ({})", DebugLogger::label(level)),
            );
            None
        };
        let label = DebugLogger::label(level);
        let plain = format!("Debug level: {level} ({label})");
        self.log_spans(
            &plain,
            vec![
                ("· ".into(), Face::ACCENT),
                ("Debug level: ".into(), Face::MUTED),
                (format!("{level} ({label})"), Face::HIGHLIGHT_BOLD),
            ],
        );
        if level > 0 {
            if let Some(path) = &self.debug.path {
                let p = path.display().to_string();
                self.log_kv("· ", Face::ACCENT, "Debug log", &p, Face::MUTED);
            }
        } else if let Some(path) = closed_path {
            let p = path.display().to_string();
            self.log_kv("· ", Face::ACCENT, "Debug log saved", &p, Face::MUTED);
        }
    }

    pub(super) fn init_debug_session(&mut self) {
        if self.debug.level == 0 {
            return;
        }
        if let Some(err) = self
            .debug
            .ensure_file(&self.options.workspace, &self.options.account)
        {
            self.push_entry(&err, Entry::Error(err.clone()));
        }
        let label = DebugLogger::label(self.debug.level);
        let path_str = self
            .debug
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let plain = if path_str.is_empty() {
            format!("Debug level: {} ({label})", self.debug.level)
        } else {
            format!(
                "Debug level: {} ({label}) · Debug log: {path_str}",
                self.debug.level
            )
        };
        let mut parts = vec![
            ("· ".into(), Face::ACCENT),
            ("Debug level: ".into(), Face::MUTED),
            (
                format!("{} ({label})", self.debug.level),
                Face::HIGHLIGHT_BOLD,
            ),
        ];
        if !path_str.is_empty() {
            parts.push((" · Debug log: ".into(), Face::MUTED));
            parts.push((path_str, Face::MUTED));
        }
        self.log_spans(&plain, parts);
    }

    pub(super) fn log_daemon_rows(&mut self, details: &str) {
        for line in details.lines() {
            if let Some(rest) = line.strip_prefix("Local daemon: ") {
                let face = if rest == "responding" {
                    Face::HIGHLIGHT_BOLD
                } else {
                    Face::DANGER_BOLD
                };
                self.log_kv("  ", Face::MUTED, "Local daemon", rest, face);
            } else if let Some(rest) = line.strip_prefix("Daemon: ") {
                if let Some((loc, status)) = rest.rsplit_once(" · ") {
                    let sface = if status == "connected" {
                        Face::HIGHLIGHT_BOLD
                    } else {
                        Face::DANGER_BOLD
                    };
                    self.log_spans(
                        line,
                        vec![
                            ("  Daemon: ".into(), Face::MUTED),
                            (loc.into(), Face::TEXT),
                            (" · ".into(), Face::MUTED),
                            (status.into(), sface),
                        ],
                    );
                    continue;
                }
                self.log_kv("  ", Face::MUTED, "Daemon", rest, Face::DANGER_BOLD);
            } else if let Some(rest) = line.strip_prefix("Runtime: ") {
                let face = if rest == "local" {
                    Face::HIGHLIGHT
                } else {
                    Face::DANGER_BOLD
                };
                self.log_kv("  ", Face::MUTED, "Runtime", rest, face);
            } else if let Some(rest) = line.strip_prefix("Daemon transport: ") {
                self.log_kv("  ", Face::MUTED, "Daemon transport", rest, Face::ACCENT);
            } else if let Some(rest) = line.strip_prefix("Connection error: ") {
                self.log_kv(
                    "  ",
                    Face::MUTED,
                    "Connection error",
                    rest,
                    Face::DANGER_BOLD,
                );
            } else {
                let mut parts = vec![("  ".into(), Face::MUTED)];
                parts.extend(inline_notice_parts(line, Face::MUTED));
                self.log_spans(line, parts);
            }
        }
    }

    pub(super) fn log_status_rows(&mut self, primary: bool, text: &str) {
        let prefix = if primary { "· " } else { "  " };
        let prefix_face = if primary { Face::ACCENT } else { Face::MUTED };
        for line in text.lines() {
            if let Some((key, value)) = line.split_once(": ") {
                let value_face = if key.contains("issue") {
                    Face::DANGER
                } else if primary {
                    Face::ACCENT
                } else {
                    Face::TEXT
                };
                self.log_kv(prefix, prefix_face, key, value, value_face);
            } else {
                self.push_entry(
                    line,
                    Entry::StatusRow {
                        primary,
                        text: line.into(),
                    },
                );
            }
        }
    }

    pub(super) fn account(&self) -> Option<&Value> {
        self.status["accounts"]
            .as_array()?
            .iter()
            .find(|a| a["id"] == self.options.account)
    }

    pub(super) fn provider_id(&self) -> &str {
        self.account()
            .and_then(|a| a["provider"].as_str())
            .filter(|s| !s.is_empty())
            .or(self.options.provider.as_deref())
            .unwrap_or(crate::provider::DEFAULT_PROVIDER_ID)
    }

    pub(super) fn provider_display_name<'a>(&'a self, pid: &'a str) -> &'a str {
        if let Some(name) = self.status["providers"]
            .as_array()
            .and_then(|ps| ps.iter().find(|p| p["id"].as_str() == Some(pid)))
            .and_then(|p| p["name"].as_str())
            .filter(|s| !s.is_empty())
        {
            return name;
        }
        if self.options.provider.as_deref() == Some(pid) {
            if let Some(name) = self
                .options
                .provider_name
                .as_deref()
                .filter(|s| !s.is_empty())
            {
                return name;
            }
        }
        match pid {
            "gemini-web" => "Gemini Web",
            "claude-web" => "Claude Web",
            "chatgpt-web" => "ChatGPT Web",
            _ => pid,
        }
    }

    pub(super) fn provider_name(&self) -> &str {
        self.provider_display_name(self.provider_id())
    }

    pub(super) fn is_provider_registered(&self, provider_id: &str) -> bool {
        let Some(providers) = self.status["providers"].as_array() else {
            return true;
        };
        if providers.is_empty() {
            return provider_id == crate::provider::DEFAULT_PROVIDER_ID;
        }
        providers
            .iter()
            .any(|p| p["id"].as_str() == Some(provider_id))
    }

    pub(super) fn provider_temporary_support(&self) -> Option<bool> {
        let pid = self.provider_id();
        let providers = self.status["providers"].as_array()?;
        let provider = providers.iter().find(|p| p["id"].as_str() == Some(pid))?;
        provider["capabilities"]["temporary_sessions"].as_bool()
    }

    pub(super) fn warn_if_temporary_unsupported(&mut self) {
        if self.options.temporary && self.provider_temporary_support() == Some(false) {
            let name = self.provider_name().to_owned();
            let pid = self.provider_id().to_owned();
            self.log_warning(&format!(
                "{name} ({pid}) does not support temporary sessions; turns will fail instead of saving to server history. Use /temp off or /provider to switch."
            ));
        }
    }

    pub(super) fn switch_provider(
        &mut self,
        account_id: &str,
        provider_id: &str,
        provider_name: &str,
    ) {
        if self.busy() {
            self.log("Interrupt the current task before switching provider or account.");
            return;
        }
        if !self.is_provider_registered(provider_id) {
            self.log_error(&format!(
                "Cannot switch to account '{account_id}': provider '{provider_id}' is not registered in this runtime. Register it with `anticrumb providers add <DIR_OR_FILE>`."
            ));
            return;
        }
        if account_id == self.options.account {
            self.options.provider = Some(provider_id.to_owned());
            self.options.provider_name = Some(provider_name.to_owned());
            self.log(&format!(
                "Already using {provider_name} ({provider_id}) · account {account_id}."
            ));
            return;
        }
        self.options.account = account_id.to_owned();
        self.options.provider = Some(provider_id.to_owned());
        self.options.provider_name = Some(provider_name.to_owned());
        self.options.overrides.insert("account".into());
        self.options.overrides.insert("provider".into());
        self.options.overrides.remove("model");
        self.options.overrides.remove("thinking");
        self.options.model = None;
        self.options.thinking = None;
        self.load_model_preference();

        self.conversation_id = None;
        self.conversation_temporary = None;
        self.pending_temporary = None;
        self.active_model = None;
        self.files.clear();
        self.clear_mention();
        self.in_chars = 0;
        self.out_chars = 0;
        self.last_response.clear();
        self.copy_response.clear();
        self.stream.clear();
        self.thinking_preview.clear();
        self.subagents.clear();
        self.tool_approvals.clear();
        self.answered_approvals.clear();
        self.inspector_zoom = false;
        self.inspector_zoom_scroll = 0;
        self.shortcuts_open = false;
        self.shortcuts_scroll = 0;
        self.seal_active_tool_block(true);
        self.reset_scroll();
        self.clear_selection();
        self.clear_painted_regions();
        self.web_usage = None;
        self.web_usage_stale = false;
        self.web_history_status = Value::Null;
        self.last_login_status = Value::Null;
        self.phase = "ready".into();
        self.catalog = self
            .account()
            .filter(|a| a["models"].as_array().is_some_and(|m| !m.is_empty()))
            .cloned()
            .unwrap_or(Value::Null);
        self.load_history();

        let plain = format!(
            "Provider: {provider_name} ({provider_id}) · account {account_id} · new conversation"
        );
        self.log_spans(
            &plain,
            vec![
                ("· ".into(), Face::ACCENT),
                ("Provider: ".into(), Face::MUTED),
                (
                    format!("{provider_name} ({provider_id})"),
                    Face::ACCENT_BOLD,
                ),
                (" · account ".into(), Face::MUTED),
                (account_id.to_owned(), Face::HIGHLIGHT_BOLD),
                (" · new conversation".into(), Face::MUTED),
            ],
        );
        if self.human_required() {
            let auth_required = self
                .account()
                .is_some_and(|account| account["state"] == "auth_required");
            self.log_human_action_guide(auth_required, None, false);
        }
        self.warn_if_temporary_unsupported();
    }

    pub(super) fn copy_text(&mut self, text: &str, selected: bool, sink: &mut dyn EffectSink) {
        let result = clipboard::write_text(text).and_then(|outcome| match outcome {
            clipboard::CopyOutcome::Copied => Ok(format!(
                "Copied {} characters to clipboard.",
                text.chars().count()
            )),
            clipboard::CopyOutcome::TerminalSequence(sequence) => {
                sink.write_terminal(&sequence).map_err(|error| {
                    format!("Could not send the clipboard request to your terminal: {error}")
                })?;
                Ok(
                    "Copy request sent to terminal; clipboard access depends on terminal settings."
                        .into(),
                )
            }
        });
        let failed = result.is_err();
        let message = match result {
            Ok(message) => message,
            Err(error) => format!("Could not copy: {error}"),
        };
        if selected {
            if failed {
                self.log_error(&message);
            }
            self.clipboard_notice = Some(message);
        } else if failed {
            self.log_error(&message);
        } else if let Some(rest) = message.strip_prefix("Copied ") {
            if let Some((count, tail)) = rest.split_once(' ') {
                self.log_spans(
                    &message,
                    vec![
                        ("· ".into(), Face::ACCENT),
                        ("Copied ".into(), Face::MUTED),
                        (format!("{count} "), Face::HIGHLIGHT_BOLD),
                        (tail.into(), Face::MUTED),
                    ],
                );
                return;
            }
            self.log(&message);
        } else {
            self.log(&message);
        }
    }

    pub(super) fn copy_last_response(&mut self, sink: &mut dyn EffectSink) {
        if self.copy_response.is_empty() {
            self.log("No assistant response to copy yet.");
        } else {
            self.copy_text(&self.copy_response.clone(), false, sink);
        }
    }

    pub(super) fn human_required(&self) -> bool {
        matches!(
            self.account().and_then(|a| a["state"].as_str()),
            Some("awaiting_human" | "auth_required")
        )
    }

    pub(super) fn login_in_progress(&self) -> bool {
        matches!(
            self.account().and_then(|a| a["login"]["state"].as_str()),
            Some("waiting" | "refreshing")
        )
    }

    pub(super) fn log_human_action_guide(
        &mut self,
        auth_required: bool,
        headless: Option<bool>,
        draft_kept: bool,
    ) {
        let sign_in_target = if self.provider_id() == crate::provider::DEFAULT_PROVIDER_ID {
            "Google".to_owned()
        } else {
            self.provider_name().to_owned()
        };
        let headline = if auth_required {
            format!("{sign_in_target} sign-in required")
        } else {
            "Human verification (CAPTCHA) required".into()
        };
        let badge = if draft_kept {
            " · Draft kept"
        } else {
            " · Account paused"
        };
        self.log_spans(
            &format!("! {headline}{badge}"),
            vec![
                ("! ".into(), Face::DANGER_BOLD),
                (headline, Face::DANGER_BOLD),
                (badge.into(), Face::HIGHLIGHT_BOLD),
            ],
        );
        if auth_required {
            if headless == Some(false) {
                let step_plain = format!(
                    "  1. Visible Chrome window stays open — sign in to {sign_in_target} (or run /browser login)."
                );
                let step_prefix = format!(
                    "Visible Chrome window stays open — sign in to {sign_in_target} (or run "
                );
                self.log_spans(
                    &step_plain,
                    vec![
                        ("  1. ".into(), Face::HIGHLIGHT_BOLD),
                        (step_prefix, Face::TEXT),
                        ("/browser login".into(), Face::ACCENT_BOLD),
                        (").".into(), Face::TEXT),
                    ],
                );
            } else {
                self.log_spans(
                    "  1. Run /browser login (or /browser show) to open Chrome and sign in.",
                    vec![
                        ("  1. ".into(), Face::HIGHLIGHT_BOLD),
                        ("Run ".into(), Face::TEXT),
                        ("/browser login".into(), Face::ACCENT_BOLD),
                        (" (or ".into(), Face::TEXT),
                        ("/browser show".into(), Face::ACCENT_BOLD),
                        (") to open Chrome and sign in.".into(), Face::TEXT),
                    ],
                );
            }
            self.log_spans(
                "  2. Model choices load automatically after sign-in (or run /browser refresh).",
                vec![
                    ("  2. ".into(), Face::HIGHLIGHT_BOLD),
                    (
                        "Model choices load automatically after sign-in (or run ".into(),
                        Face::TEXT,
                    ),
                    ("/browser refresh".into(), Face::ACCENT_BOLD),
                    (").".into(), Face::TEXT),
                ],
            );
        } else {
            if headless == Some(false) {
                self.log_spans(
                    "  1. Visible Chrome window stays open — complete the verification / CAPTCHA yourself.",
                    vec![
                        ("  1. ".into(), Face::HIGHLIGHT_BOLD),
                        (
                            "Visible Chrome window stays open — complete the verification / CAPTCHA yourself."
                                .into(),
                            Face::TEXT,
                        ),
                    ],
                );
            } else {
                self.log_spans(
                    "  1. Run /browser show to open the Chrome window and solve the CAPTCHA.",
                    vec![
                        ("  1. ".into(), Face::HIGHLIGHT_BOLD),
                        ("Run ".into(), Face::TEXT),
                        ("/browser show".into(), Face::ACCENT_BOLD),
                        (
                            " to open the Chrome window and solve the CAPTCHA.".into(),
                            Face::TEXT,
                        ),
                    ],
                );
            }
            self.log_spans(
                "  2. Run /browser refresh to verify and resume (use /browser hide when done).",
                vec![
                    ("  2. ".into(), Face::HIGHLIGHT_BOLD),
                    ("Run ".into(), Face::TEXT),
                    ("/browser refresh".into(), Face::ACCENT_BOLD),
                    (" to verify and resume (use ".into(), Face::TEXT),
                    ("/browser hide".into(), Face::ACCENT_BOLD),
                    (" when done).".into(), Face::TEXT),
                ],
            );
            self.log_spans(
                "  3. Send your prompt again or run /resume — interrupted requests are never retried automatically.",
                vec![
                    ("  3. ".into(), Face::HIGHLIGHT_BOLD),
                    ("Send your prompt again or run ".into(), Face::TEXT),
                    ("/resume".into(), Face::ACCENT_BOLD),
                    (" — interrupted requests are ".into(), Face::MUTED),
                    ("never retried automatically".into(), Face::DANGER),
                    (".".into(), Face::MUTED),
                ],
            );
        }
    }

    pub(super) fn notice_login(&mut self, login: Value) {
        if login == self.last_login_status {
            return;
        }
        match login["state"].as_str() {
            Some("waiting") => {
                self.log("Complete sign-in in the browser. Model choices will load automatically.")
            }
            Some("refreshing") => self.log("Signed in. Loading model choices…"),
            Some("ready") => self.log("Model choices are ready. Use /model to choose one."),
            Some("failed") => {
                let error = login["error"]
                    .as_str()
                    .map(str::trim)
                    .filter(|error| !error.is_empty())
                    .unwrap_or("model discovery did not complete")
                    .trim_end_matches('.');
                if error.contains("/model refresh") {
                    self.log_error(&format!("Model setup failed: {error}."));
                } else {
                    self.log_error(&format!(
                        "Model setup failed: {error}. Retry with /model refresh."
                    ));
                }
            }
            Some("timed_out") => {
                self.log("Sign-in was not completed in time. Use /browser login to try again.")
            }
            _ => (),
        }
        self.last_login_status = login;
    }

    pub(super) fn model_label(&self) -> &str {
        if let Some(active) = &self.active_model {
            return active;
        }
        if let Some(selected) = &self.options.model {
            return self.catalog["models"]
                .as_array()
                .and_then(|ms| ms.iter().find(|m| m["mode_id"] == *selected))
                .map(super::model_picker::name)
                .unwrap_or(selected);
        }
        self.account()
            .and_then(|a| a["selected_model"].as_str())
            .unwrap_or("web default")
    }

    pub(super) fn thinking_label(&self) -> &str {
        let Some(id) = self.options.thinking.as_deref() else {
            return "default";
        };
        self.catalog["model_thinking"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
            .chain(std::iter::once(&self.catalog["thinking"]))
            .filter_map(Value::as_array)
            .flatten()
            .find(|v| v["level"] == id || v["label"] == id)
            .and_then(|v| v["label"].as_str())
            .unwrap_or(id)
    }

    pub(super) fn daemon_label(&self) -> &'static str {
        let connection = &self.status["browser_connection"];
        match (
            connection["transport"].as_str(),
            connection["connected"] == true,
        ) {
            (Some("grpc"), true) => "remote",
            (Some("grpc"), false) => "remote/offline",
            (Some("local"), true) => "local",
            (Some("local"), false) => "local/offline",
            _ if connection["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty()) =>
            {
                "offline"
            }
            _ if self
                .status
                .as_object()
                .is_some_and(|status| status.is_empty()) =>
            {
                "checking"
            }
            _ => "unknown",
        }
    }

    pub(super) fn daemon_remote_addr(&self) -> Option<String> {
        let connection = &self.status["browser_connection"];
        if connection["transport"].as_str() != Some("grpc") {
            return None;
        }
        let endpoint = connection["endpoint"].as_str()?.trim();
        if endpoint.is_empty() || endpoint == "<invalid endpoint>" {
            return None;
        }
        if let Ok(url) = crate::url_lite::Url::parse(endpoint) {
            let host = url.host_str()?;
            return Some(match url.port_or_known_default() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            });
        }
        let stripped = endpoint
            .strip_prefix("https://")
            .or_else(|| endpoint.strip_prefix("http://"))
            .unwrap_or(endpoint)
            .trim_end_matches('/');
        (!stripped.is_empty()).then(|| stripped.to_owned())
    }

    pub(super) fn daemon_tls_emoji(&self) -> Option<String> {
        let connection = &self.status["browser_connection"];
        if connection["transport"].as_str() != Some("grpc") {
            return None;
        }
        let pin = connection["tls_pin"].as_str().or_else(|| {
            let endpoint = connection["endpoint"].as_str();
            let matches = match (self.remote_tls_endpoint.as_deref(), endpoint) {
                (Some(saved), Some(reported)) => {
                    let norm_saved = crate::rpc::client::endpoint_description(saved);
                    let norm_reported = crate::rpc::client::endpoint_description(reported);
                    norm_saved != "<invalid endpoint>" && norm_saved == norm_reported
                }
                _ => false,
            };
            matches.then_some(self.remote_tls_pin.as_deref()).flatten()
        })?;
        crate::rpc::fingerprint::short_emoji_from_pin(pin)
    }

    pub(super) fn daemon_summary(&self) -> String {
        let connection = &self.status["browser_connection"];
        let location = match connection["transport"].as_str() {
            Some("grpc") => {
                let endpoint = connection["endpoint"]
                    .as_str()
                    .unwrap_or("unknown endpoint");
                match self.daemon_tls_emoji() {
                    Some(emoji) => format!("remote {endpoint} ({emoji})"),
                    None => format!("remote {endpoint}"),
                }
            }
            Some("local") => return format!("Runtime: {}", self.daemon_label()),
            _ => return format!("Daemon: {}", self.daemon_label()),
        };
        format!(
            "Daemon: {location} · {}",
            if connection["connected"] == true {
                "connected"
            } else {
                "disconnected"
            }
        )
    }

    pub(super) fn daemon_details(&self) -> String {
        let connection = &self.status["browser_connection"];
        let mut details = format!(
            "{}\nDaemon transport: {}\nDaemon server: {}\nWorkspace server: {}",
            self.daemon_summary(),
            connection["transport"].as_str().unwrap_or("unavailable"),
            connection["server_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .unwrap_or("unavailable"),
            self.status["server_id"].as_str().unwrap_or("unavailable")
        );
        if let Some(error) = connection["error"]
            .as_str()
            .filter(|error| !error.is_empty())
        {
            details.push_str(&format!("\nConnection error: {error}"));
        }
        if connection["transport"] == "grpc" && connection["connected"] == false {
            details.push_str("\nInspect saved remote settings with `anticrumb connect status`.");
        }
        details
    }

    pub(super) fn runtime_details(&self) -> String {
        let age = self.last_status_at.map(|at| at.elapsed());
        let stale =
            self.status_disconnected || age.is_some_and(|elapsed| elapsed > Duration::from_secs(5));
        let runtime = &self.status["runtime"];
        let health = if self.status_disconnected {
            "disconnected"
        } else if self.last_status_at.is_none() {
            "waiting for status"
        } else if stale {
            "status is stale"
        } else if !runtime.is_object() {
            "not reported"
        } else if runtime["stopping"] == true {
            "stopping"
        } else {
            "responding"
        };
        let checked = age
            .map(|elapsed| format!("{}s ago", elapsed.as_secs()))
            .unwrap_or_else(|| "no reply yet".into());
        let mut details = format!("Local daemon: {health}\nLast status reply: {checked}");
        if !runtime.is_object() {
            return details;
        }
        if stale {
            details.push_str("\nRuntime details: last known values");
        }
        let number = |key: &str| {
            runtime[key]
                .as_u64()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unavailable".into())
        };
        let uptime = runtime["uptime_ms"].as_u64().unwrap_or(0) / 1000;
        details.push_str(&format!(
            "\nPID: {}\nVersion: {}\nLifecycle: {}\nIPC: {}\nIPC endpoint: {}\nUptime: {}h {:02}m {:02}s\nConnected clients: {} (includes this TUI)",
            number("pid"), runtime["version"].as_str().unwrap_or("unavailable"),
            if runtime["automatic"] == true { "automatic; exits after last client" } else { "persistent; explicitly started" },
            runtime["transport"].as_str().unwrap_or("unavailable"), runtime["endpoint"].as_str().unwrap_or("unavailable"),
            uptime / 3600, (uptime / 60) % 60, uptime % 60, number("connected_clients"),
        ));
        let jobs = |key: &str| {
            self.status[key]
                .as_u64()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unavailable".into())
        };
        details.push_str(&format!(
            "\nWorkspace jobs: {} agents, {} queued turns, {} running turns",
            jobs("active_agents"),
            jobs("queued"),
            jobs("running")
        ));
        details
    }

    pub(super) fn chat_cleanup_details(&self) -> Option<String> {
        let cleanup = &self.status["chat_cleanup"];
        if !cleanup.is_object() {
            return None;
        }
        let count = |field: &str| cleanup[field].as_u64().unwrap_or(0);
        let mut message = format!(
            "Web chat cleanup: {} queued, {} running, {} completed, {} failed, {} without chat links",
            count("pending"), count("running"), count("completed"), count("failed"), count("unmapped"),
        );
        if let Some(error) = cleanup["last_error"].as_str().filter(|s| !s.is_empty()) {
            message.push_str(&format!("\nLatest cleanup issue: {}", clip(error, 300)));
        }
        Some(message)
    }

    pub(super) fn notice_chat_cleanup(&mut self) {
        let cleanup = self.status["chat_cleanup"].clone();
        let previous = std::mem::replace(&mut self.last_chat_cleanup, cleanup.clone());
        // Counts are durable totals. The first snapshot is a baseline, not a
        // notification that earlier cleanup jobs just completed again.
        if !previous.is_object() || previous == cleanup || !cleanup.is_object() {
            return;
        }
        let needs_attention = cleanup["failed"].as_u64().unwrap_or(0) > 0
            || cleanup["unmapped"].as_u64().unwrap_or(0) > 0;
        let new_issue = ["failed", "unmapped"].iter().any(|&field| {
            cleanup[field].as_u64().unwrap_or(0) > previous[field].as_u64().unwrap_or(0)
        }) || cleanup["last_error"] != previous["last_error"];
        let message = if let Some(error) = cleanup["last_error"]
            .as_str()
            .filter(|error| needs_attention && new_issue && !error.is_empty())
        {
            format!(
                "Web chat cleanup needs attention: {} · /status",
                clip(error, 240)
            )
        } else {
            self.chat_cleanup_details().unwrap_or_default()
        };
        self.log(&message);
        self.resume_notice = Some(message);
    }

    pub(super) fn web_history_details(&self) -> Option<String> {
        let value = &self.web_history_status;
        if !value.is_object() {
            return None;
        }
        let account = clip(value["account_id"].as_str().unwrap_or("unknown"), 80);
        let state = value["state"].as_str().unwrap_or("unknown");
        let deleted = value["deleted"].as_u64().unwrap_or(0);
        let provider = self.provider_name().trim_end_matches(" Web");
        let mut message =
            format!("{provider} history cleanup ({account}): {state} · {deleted} chats deleted");
        if let Some(error) = value["error"].as_str().filter(|error| !error.is_empty()) {
            message.push_str(&format!("\nHistory cleanup issue: {}", clip(error, 300)));
        }
        Some(message)
    }

    pub(super) fn notice_web_history(&mut self, value: Value, announce: bool) {
        let changed = self.web_history_status.is_object()
            && ["account_id", "state", "deleted", "error", "started_at"]
                .iter()
                .any(|field| self.web_history_status[*field] != value[*field]);
        let active = matches!(value["state"].as_str(), Some("pending" | "running"));
        let was_known = self.web_history_status.is_object();
        self.web_history_status = value;
        if announce || changed || !was_known && active {
            if let Some(message) = self.web_history_details() {
                self.log(&message);
            }
        }
    }

    pub(super) fn send(&mut self, sink: &mut dyn EffectSink, command: Command) {
        if let Err(e) = sink.send_command(command) {
            self.error(&format!("could not queue command: {e}"));
        }
    }

    pub(super) fn error(&mut self, error: &str) {
        self.exit_requested = false;
        self.log_error(&format!("! {error}"));
        if let Some((draft, files)) = self.pending.take() {
            if self.editor.text.is_empty() {
                self.editor.restore(draft);
            } else {
                self.saved_draft = draft;
                self.log("Previous draft retained in input history (Up).");
            }
            self.files = files;
        }
        if self.active.is_none() {
            self.pending_temporary = None;
            self.phase = "ready".into();
        }
    }

    pub(super) fn load_history(&mut self) {
        if let Some(data) = &self.preferences {
            let history_res = data.history(&self.options.account, &self.options.workspace);
            let recents_res = data.recent_mentions(&self.options.workspace);
            match history_res {
                Ok(items) => self.history = items,
                Err(e) => self.log(&format!("History: {e}")),
            }
            if let Ok(recents) = recents_res {
                self.recent_mentions = recents;
            }
        }
        self.history_index = self.history.len();
        self.saved_draft.clear();
    }

    pub(super) fn remember_mention(&mut self, mention: preferences::RecentMention) {
        if mention.display.is_empty() || self.recent_mentions.first() == Some(&mention) {
            return;
        }
        preferences::push_recent_mention(&mut self.recent_mentions, &mention);
        if let Some(data) = &self.preferences {
            if let Ok(items) = data.record_recent_mention(&self.options.workspace, &mention) {
                self.recent_mentions = items;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn remember(&mut self, input: &str) {
        self.remember_draft(&Draft::plain(input.to_owned()));
    }

    pub(super) fn remember_draft(&mut self, input: &Draft) {
        preferences::append(&mut self.history, input);
        if let Some(data) = &self.preferences {
            match data.record(&self.options.account, &self.options.workspace, input) {
                Ok(items) => self.history = items,
                Err(e) => self.log(&format!("History could not be saved: {e}")),
            }
        }
        self.history_index = self.history.len();
        self.saved_draft.clear();
    }

    pub(super) fn previous_input(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index == self.history.len() {
            self.saved_draft = self.editor.draft();
        }
        self.history_index = self.history_index.saturating_sub(1);
        self.editor
            .restore(self.history[self.history_index].clone());
        self.clear_mention();
    }

    pub(super) fn next_input(&mut self) {
        if self.history_index >= self.history.len() {
            return;
        }
        self.history_index += 1;
        self.editor.restore(
            self.history
                .get(self.history_index)
                .cloned()
                .unwrap_or_else(|| self.saved_draft.clone()),
        );
        self.clear_mention();
    }

    pub(super) fn paste(&mut self, text: &str) {
        if !self.editor.paste(text) {
            self.log("Paste exceeds the 48,000-byte input limit; your draft is unchanged. Save the text to a file and attach it with @path.");
        }
        self.completion = None;
        self.clear_mention();
    }

    pub(super) fn clear_mention(&mut self) {
        self.mention = None;
        self.mention_dismissed = None;
    }

    pub(super) fn mention_loading(&self) -> bool {
        self.mention.as_ref().is_some_and(|m| m.loading)
    }

    pub(super) fn mention_panel_rows(&self) -> usize {
        if self.menu.is_some() || self.history_clear_confirmation.is_some() {
            return 0;
        }
        self.mention.as_ref().map_or(0, MentionPopup::desired_rows)
    }

    pub(super) fn refresh_mention(&mut self, sink: &mut dyn EffectSink) {
        let Some(parsed) = mentions::parse_at_cursor(&self.editor) else {
            self.mention = None;
            self.mention_dismissed = None;
            return;
        };
        let prefix = &self.editor.text[..parsed.start];
        if self
            .mention_dismissed
            .as_ref()
            .is_some_and(|(start, dismissed_prefix)| {
                *start == parsed.start && dismissed_prefix == prefix
            })
        {
            return;
        }
        self.mention_dismissed = None;
        let unchanged = self.mention.as_ref().is_some_and(|m| {
            m.start == parsed.start
                && m.end == parsed.end
                && m.query == parsed.query
                && m.quoted == parsed.quoted
        });
        if unchanged {
            return;
        }
        let request_id = self.next_mention_id;
        self.next_mention_id = self.next_mention_id.saturating_add(1);
        let popup = MentionPopup::new(&parsed, request_id, self.mention.as_ref());
        self.mention = Some(popup);
        self.send(
            sink,
            Command::SearchMentions {
                id: request_id,
                workspace: self.options.workspace.clone(),
                query: parsed.query,
                recent: self.recent_mentions.clone(),
            },
        );
    }

    pub(super) fn dismiss_mention(&mut self) {
        if let Some(m) = self.mention.take() {
            let prefix = self
                .editor
                .text
                .get(..m.start)
                .unwrap_or_default()
                .to_owned();
            self.mention_dismissed = Some((m.start, prefix));
        }
    }

    pub(super) fn move_mention(&mut self, up: bool) {
        if let Some(m) = self.mention.as_mut() {
            m.move_selection(up);
        }
    }

    fn apply_mention_match(
        &mut self,
        start: usize,
        end: usize,
        quoted: bool,
        matched: &MentionMatch,
        sink: &mut dyn EffectSink,
    ) {
        let insertion = matched.format_insertion(quoted);
        self.editor.replace(start..end, &insertion);
        self.mention_dismissed = None;
        if matched.kind == MentionKind::Dir {
            self.refresh_mention(sink);
        } else {
            let recent_kind = match matched.kind {
                MentionKind::File => preferences::RecentMentionKind::File,
                MentionKind::Git => preferences::RecentMentionKind::Git,
                MentionKind::Dir => unreachable!(),
            };
            self.remember_mention(preferences::RecentMention {
                display: matched.display.clone(),
                kind: recent_kind,
                detail: matched.detail.clone(),
            });
            self.mention = None;
        }
    }

    pub(super) fn select_mention(&mut self, sink: &mut dyn EffectSink) -> bool {
        let Some(m) = self.mention.as_ref() else {
            return false;
        };
        let Some(matched) = m.matches.get(m.selected).cloned() else {
            return false;
        };
        let (start, end, quoted) = (m.start, m.end, m.quoted);
        self.apply_mention_match(start, end, quoted, &matched, sink);
        true
    }

    pub(super) fn tab_complete_mention(&mut self, sink: &mut dyn EffectSink) -> bool {
        let Some(m) = self.mention.as_ref() else {
            return false;
        };
        if let Some(matched) = m.matches.get(m.selected).cloned() {
            let (start, end, quoted) = (m.start, m.end, m.quoted);
            self.apply_mention_match(start, end, quoted, &matched, sink);
            return true;
        }
        if m.loading {
            if let Some((matches, _)) = mentions::search_mentions_with_recent(
                &self.options.workspace,
                &m.query,
                &self.recent_mentions,
                None,
                m.request_id,
            ) {
                if let Some(first) = matches.first() {
                    let (start, end, quoted) = (m.start, m.end, m.quoted);
                    self.apply_mention_match(start, end, quoted, first, sink);
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn completion_mouse(
        &mut self,
        mouse: event::MouseEvent,
        sink: &mut dyn EffectSink,
    ) -> bool {
        let Some(layout) = self.painted_layout else {
            return false;
        };
        if !layout.completion.contains(mouse.row) || self.mention.is_none() {
            return false;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            let rel = usize::from(mouse.row - layout.completion.start);
            let total_rows = usize::from(layout.completion.len);
            if rel >= 1 {
                let item_offset = rel - 1;
                let picked = self.mention.as_ref().and_then(|m| {
                    let (visible, local_offset) = m.visible_window(total_rows);
                    visible.get(item_offset).cloned().map(|matched| {
                        (
                            m.start,
                            m.end,
                            m.quoted,
                            m.scroll + local_offset + item_offset,
                            matched,
                        )
                    })
                });
                if let Some((start, end, quoted, idx, matched)) = picked {
                    if let Some(m) = self.mention.as_mut() {
                        m.selected = idx;
                    }
                    self.apply_mention_match(start, end, quoted, &matched, sink);
                }
            }
        }
        true
    }

    pub(super) fn load(&mut self, conversation: crate::domain::ConversationRecord) {
        let Some(latest) = conversation.latest_turn() else {
            return;
        };
        // Clone options and reset `temporary` to the saved preference (unless overridden by
        // `--temp` or an unpersisted `/temp on`) before inheriting from the resumed job.
        // Applying only on success prevents partial mutation on error and avoids leaking
        // temporary state when consecutively `/resume`-ing `[temp]` -> normal conversations.
        let mut next_options = self.options.clone();
        if !next_options.overrides.contains("temporary") {
            next_options.temporary = self.saved_temporary_preference();
        }
        let was_temporary = next_options.temporary;
        if let Err(e) = next_options.inherit(&latest.request) {
            self.error(&e.to_string());
            return;
        }
        let conv_id = conversation.summary.id.clone();
        let latest_id = latest.id.clone();
        let is_temp = conversation.summary.is_temporary();
        let turn_count = conversation.turns.len();
        self.options = next_options;
        self.conversation_id = Some(conv_id.clone());
        self.conversation_temporary = Some(is_temp);
        self.pending_temporary = None;
        self.active_model = None;
        self.clear_mention();
        self.in_chars = 0;
        self.out_chars = 0;
        self.last_response.clear();
        self.copy_response.clear();
        self.stream.clear();
        self.thinking_preview.clear();
        self.subagents.clear();
        self.tool_approvals.clear();
        self.answered_approvals.clear();
        self.inspector_zoom = false;
        self.inspector_zoom_scroll = 0;
        self.shortcuts_open = false;
        self.shortcuts_scroll = 0;
        self.reset_scroll();
        self.clear_selection();
        self.clear_painted_regions();
        self.transcript.clear();
        self.tool_blocks.clear();
        let state_str = conversation.summary.state.as_str();
        let state_face = match state_str {
            "completed" | "ready" => Face::HIGHLIGHT_BOLD,
            "failed" | "cancelled" | "outcome_unknown" | "awaiting_human" => Face::DANGER_BOLD,
            _ => Face::ACCENT,
        };
        let turns_label = if turn_count == 1 {
            "1 turn".to_owned()
        } else {
            format!("{turn_count} turns")
        };
        let plain_resumed = if is_temp {
            format!("Resumed {conv_id} [temp] · {state_str} · {turns_label}")
        } else {
            format!("Resumed {conv_id} · {state_str} · {turns_label}")
        };
        let mut resumed_spans = vec![
            ("· ".into(), Face::ACCENT),
            (format!("Resumed {conv_id}"), Face::ACCENT_BOLD),
        ];
        if is_temp {
            resumed_spans.push((" ".into(), Face::MUTED));
            resumed_spans.push(("[temp]".into(), Face::CODE_BOLD));
        }
        resumed_spans.push((" · ".into(), Face::MUTED));
        resumed_spans.push((state_str.into(), state_face));
        resumed_spans.push((" · ".into(), Face::MUTED));
        resumed_spans.push((turns_label, Face::MUTED));
        self.log_spans(&plain_resumed, resumed_spans);
        for turn in &conversation.turns {
            let task = turn.request.task.as_str();
            self.push_entry(&format!("› {task}"), Entry::Prompt(task.into()));
            if let Some(result) = &turn.result {
                if !result.steps.is_empty() {
                    if let Some((block_id, plain)) = self.tool_blocks.build_from_turn_steps(
                        &result.steps,
                        result.inspector_pruned,
                        self.layout.inspector.after_turn,
                        self.layout.inspector.follow,
                    ) {
                        self.push_entry_raw(&plain, Entry::ToolBlock(block_id));
                    }
                }
                if !result.final_response.is_empty() {
                    self.log_markdown(&result.final_response);
                    self.copy_response = result.final_response.clone();
                }
                if !result.files_changed.is_empty()
                    || (turn.state != "completed" && !turn.is_active())
                {
                    let changed = result.files_changed.len();
                    let files_label = format!("{changed} file(s) changed");
                    let turn_face = if turn.state == "completed" {
                        Face::HIGHLIGHT_BOLD
                    } else {
                        Face::DANGER_BOLD
                    };
                    self.log_spans(
                        &format!("  {} · {files_label}", turn.state),
                        vec![
                            ("  ".into(), Face::MUTED),
                            (turn.state.clone(), turn_face),
                            (" · ".into(), Face::MUTED),
                            (
                                files_label,
                                if changed > 0 {
                                    Face::HIGHLIGHT_BOLD
                                } else {
                                    Face::MUTED
                                },
                            ),
                        ],
                    );
                }
            } else if !matches!(turn.state.as_str(), "completed" | "ready")
                && !turn.is_active()
                && turn.error.is_none()
            {
                self.log_spans(
                    &format!("  {}", turn.state),
                    vec![
                        ("  ".into(), Face::MUTED),
                        (turn.state.clone(), Face::DANGER_BOLD),
                    ],
                );
            }
            if let Some(error) = &turn.error {
                self.log_error(&format!("! {error}"));
            }
        }
        self.load_history();
        if latest.is_active() {
            self.active = Some(latest_id);
            self.phase = "running".into();
            self.started = Instant::now();
        } else {
            self.phase = "ready".into();
            self.active = None;
            self.log("Send a follow-up to continue. Previous actions will not be replayed automatically.");
        }
        if self.options.temporary && self.conversation_temporary == Some(false) {
            self.log("  Temp chat remains ON: next turn starts a new temporary chat with local context instead of continuing the server chat (use /temp off to continue on server).");
        } else if !was_temporary && self.options.temporary && is_temp {
            self.log("  Temp chat turned ON for this resumed temporary conversation (no server history; /new returns to saved normal mode).");
        }
        self.warn_if_temporary_unsupported();
    }

    fn apply_status(&mut self, status: Value) -> bool {
        let was_disconnected = self.status_disconnected;
        let prev_rev = self.transcript.revision();
        let visible_changed = was_disconnected || status_visible_changed(&self.status, &status);
        self.last_status_at = Some(Instant::now());
        self.status_disconnected = false;
        let was_blocked = self.human_required();
        let previous_connection = self.daemon_summary();
        let prev_pid = self.provider_id().to_owned();
        let prev_temp_support = self.provider_temporary_support();
        self.status = status;
        self.notice_chat_cleanup();
        let connection = self.daemon_summary();
        if connection != previous_connection {
            self.log_daemon_rows(&connection);
        }
        let next_pid = self.provider_id().to_owned();
        let next_temp_support = self.provider_temporary_support();
        if self.options.temporary
            && next_temp_support == Some(false)
            && (prev_pid != next_pid || prev_temp_support != Some(false))
        {
            self.warn_if_temporary_unsupported();
        }
        let login = self
            .account()
            .map(|account| account["login"].clone())
            .unwrap_or(Value::Null);
        self.notice_login(login.clone());
        if self.human_required() && !was_blocked && !self.login_in_progress() {
            let auth_required = self
                .account()
                .is_some_and(|account| account["state"] == "auth_required");
            self.log_human_action_guide(auth_required, None, false);
        } else if was_blocked
            && self
                .account()
                .is_some_and(|account| account["state"] == "ready")
            && login["state"] != "ready"
        {
            self.log("Account is available again. Review the recorded outcome, then send your draft or resume explicitly.");
        }
        let mut catalog_changed = false;
        if let Some(account) = self
            .account()
            .filter(|a| a["models"].as_array().is_some_and(|m| !m.is_empty()))
        {
            // Compare before cloning: status polls usually repeat the same catalog.
            if self.catalog != *account {
                self.catalog = account.clone();
                catalog_changed = true;
            }
        }
        visible_changed || catalog_changed || self.transcript.revision() != prev_rev
    }

    pub(super) fn update(&mut self, update: Update) -> bool {
        match update {
            Update::Status(status) => return self.apply_status(status),
            Update::StatusReport(status) => {
                match status {
                    Some(status) => {
                        self.apply_status(status);
                    }
                    None => {
                        self.update(Update::StatusDisconnected);
                    }
                }
                let details = self.runtime_details();
                self.log_daemon_rows(&details);
                let browser = self.daemon_details();
                self.log_daemon_rows(&browser);
                if let Some(cleanup) = self.chat_cleanup_details() {
                    self.log_status_rows(false, &cleanup);
                }
            }
            Update::Providers(status, target) => {
                if self.active.is_none() {
                    self.phase = "ready".into();
                }
                self.apply_status(status);
                match target {
                    Some(arg) => self.apply_provider_arg(&arg, None),
                    None => self.open_providers(),
                }
            }
            Update::ProviderSetup {
                status,
                account_id,
                provider_id,
                provider_name,
                login_error,
            } => {
                if self.active.is_none() {
                    self.phase = "ready".into();
                }
                self.apply_status(status);
                self.switch_provider(&account_id, &provider_id, &provider_name);
                if let Some(err) = login_error {
                    self.log_error(&format!(
                        "Account '{account_id}' created, but opening login failed: {err}"
                    ));
                }
            }
            Update::StatusDisconnected => {
                let was_disconnected = self.status_disconnected;
                let prev_rev = self.transcript.revision();
                let prev_conn = self.status["browser_connection"].clone();
                self.status_disconnected = true;
                let previous_connection = self.daemon_summary();
                if !self.status["browser_connection"].is_object() {
                    self.status["browser_connection"] = json!({});
                }
                self.status["browser_connection"]["connected"] = json!(false);
                self.status["browser_connection"]["error"] =
                    json!("Workspace status is unavailable; retrying");
                let connection = self.daemon_summary();
                if connection != previous_connection {
                    self.log_daemon_rows(&connection);
                }
                return !was_disconnected
                    || self.transcript.revision() != prev_rev
                    || self.status["browser_connection"] != prev_conn;
            }
            Update::Accepted(count) => {
                self.pending = None;
                if count > 0 {
                    let tail = if self.provider_id() == crate::provider::DEFAULT_PROVIDER_ID {
                        " file(s) queued for native Gemini upload."
                    } else {
                        " file(s) queued for native web upload."
                    };
                    let plain = format!("  {count}{tail}");
                    self.log_spans(
                        &plain,
                        vec![
                            ("  ".into(), Face::MUTED),
                            (count.to_string(), Face::HIGHLIGHT_BOLD),
                            (tail.into(), Face::MUTED),
                        ],
                    );
                }
            }
            Update::Started {
                id,
                conversation_id,
            } => {
                if self.debug.level >= 1 {
                    self.log_debug("agent", &format!("started job {id} ({conversation_id})"));
                }
                self.active = Some(id);
                self.conversation_id = Some(conversation_id);
                if let Some(temp) = self.pending_temporary.take() {
                    self.conversation_temporary = Some(temp);
                }
                self.phase = "running".into();
                self.in_chars = 0;
                self.out_chars = 0;
                self.active_model = None;
                self.last_response.clear();
                self.stream.clear();
                self.thinking_preview.clear();
                self.subagents.clear();
                self.tool_approvals.clear();
                self.answered_approvals.clear();
                self.started = Instant::now();
            }
            Update::Error(error) => self.error(&error),
            Update::Loaded(conversation) => self.load(conversation),
            Update::Models(capability) => {
                if self.active.is_none() {
                    self.phase = "ready".into();
                }
                self.catalog = capability;
                if self.catalog["models"].as_array().is_none_or(Vec::is_empty) {
                    self.log("No cached models. Run /model refresh to update from the browser.");
                } else {
                    self.open_models();
                }
            }
            Update::LoginOpened(browser) => {
                if browser["login"].is_object() {
                    self.notice_login(browser["login"].clone());
                } else {
                    self.log("Sign-in browser opened. This browser host does not report automatic model setup; use /browser refresh after signing in.");
                }
            }
            Update::WebUsage(value) => {
                if self.active.is_none() && self.pending.is_none() && self.phase == "loading" {
                    self.phase = "ready".into();
                }
                self.log_markdown(&usage::report(&value));
                self.web_usage = Some(value);
                self.web_usage_stale = false;
            }
            Update::WebUsageError(error) => {
                self.web_usage_stale = true;
                if self.active.is_none() && self.pending.is_none() {
                    self.error(&error);
                } else {
                    self.log_error(&format!("! {error}"));
                }
            }
            Update::Browser(browser) => self.log_browser_status(&browser),
            Update::BrowserControl(text) => self.log(&text),
            Update::Cache(report, options) => self.log(&super::cache::report(&report, options)),
            Update::CacheError(error) => self.log_error(&format!("Cache: {error}")),
            Update::Conversations(conversations) => {
                self.resume_notice = None;
                self.phase = "ready".into();
                let choices: Vec<_> = conversations
                    .into_iter()
                    .take(30)
                    .map(|c| {
                        let temp = if c.is_temporary() { " [temp]" } else { "" };
                        Choice {
                            label: format!(
                                "{}{} · {}t · {}",
                                c.state,
                                temp,
                                c.turn_count.max(1),
                                clip(&c.title, 72)
                            ),
                            value: Selection::Resume(c.id),
                        }
                    })
                    .collect();
                if choices.is_empty() {
                    self.log("No saved conversations in this directory. Use /resume clear-history to delete all account web chats.");
                } else {
                    self.menu = Some(Menu {
                        title: "Resume conversation · d delete · D all web chats".into(),
                        choices,
                        selected: 0,
                    });
                }
            }
            Update::ConversationDeleted(id) => {
                self.deleting_conversation = None;
                self.phase = "ready".into();
                if self.conversation_id.as_deref() == Some(&id) {
                    self.conversation_id = None;
                    self.conversation_temporary = None;
                    self.pending_temporary = None;
                    self.reset_temporary_to_preference();
                    self.active_model = None;
                    self.in_chars = 0;
                    self.out_chars = 0;
                    self.last_response.clear();
                    self.copy_response.clear();
                }
                if let Some(menu) = self.menu.as_mut() {
                    menu.choices.retain(
                        |choice| !matches!(&choice.value, Selection::Resume(value) if value == &id),
                    );
                    menu.selected = menu.selected.min(menu.choices.len().saturating_sub(1));
                    if menu.choices.is_empty() {
                        self.menu = None;
                        self.log("No saved conversations in this directory.");
                    }
                }
                let message = format!("Deleted saved conversation {id}. Web cleanup: /status.");
                self.log(&message);
                self.resume_notice = Some(message);
            }
            Update::ConversationDeleteError(error) => {
                self.deleting_conversation = None;
                self.phase = "ready".into();
                let message = format!("Could not delete conversation: {error}");
                self.log_error(&message);
                self.resume_notice = Some(message);
            }
            Update::WebHistoryQueued(value) => {
                self.clearing_web_history = false;
                self.phase = "ready".into();
                self.notice_web_history(value, true);
            }
            Update::WebHistoryStatus(value, announce) => self.notice_web_history(value, announce),
            Update::WebHistoryError(error, submission) => {
                if submission {
                    self.clearing_web_history = false;
                    self.phase = "ready".into();
                }
                self.log_error(&format!(
                    "Could not {} {} history cleanup: {error}",
                    if submission { "queue" } else { "check" },
                    self.provider_name().trim_end_matches(" Web")
                ));
            }
            Update::Finished(job) => self.handle_finished(job),
            Update::Event(event) => self.handle_agent_event(event),
            Update::MentionMatches {
                id,
                query,
                matches,
                total,
            } => {
                return self
                    .mention
                    .as_mut()
                    .is_some_and(|mention| mention.apply_results(id, &query, matches, total));
            }
        }
        true
    }

    fn log_browser_status(&mut self, browser: &Value) {
        self.log_heading(&format!(
            "Browser · {}",
            browser["account_id"].as_str().unwrap_or("")
        ));
        let connected = browser["connected"] == true;
        let conn_state = if connected {
            "connected"
        } else {
            "disconnected"
        };
        let browser_label = browser["browser"].as_str().unwrap_or("unavailable");
        self.log_spans(
            &format!("Connection: {conn_state} · {browser_label}"),
            vec![
                ("· ".into(), Face::ACCENT),
                ("Connection: ".into(), Face::MUTED),
                (
                    conn_state.into(),
                    if connected {
                        Face::HIGHLIGHT_BOLD
                    } else {
                        Face::DANGER_BOLD
                    },
                ),
                (" · ".into(), Face::MUTED),
                (browser_label.into(), Face::ACCENT),
            ],
        );
        let acct_state = browser["account_state"].as_str().unwrap_or("unknown");
        self.log_kv(
            "· ",
            Face::ACCENT,
            "Account state",
            acct_state,
            match acct_state {
                "ready" => Face::HIGHLIGHT_BOLD,
                "awaiting_human" | "auth_required" => Face::DANGER_BOLD,
                _ => Face::ACCENT,
            },
        );
        let mode_label = match browser["headless"].as_bool() {
            Some(true) => "headless (background)",
            Some(false) => "visible (window open)",
            None => "not running (starts headless)",
        };
        self.log_kv(
            "  ",
            Face::MUTED,
            "Mode",
            mode_label,
            match browser["headless"].as_bool() {
                Some(false) => Face::ACCENT_BOLD,
                Some(true) => Face::HIGHLIGHT_BOLD,
                None => Face::MUTED,
            },
        );
        self.log_kv(
            "  ",
            Face::MUTED,
            "Endpoint",
            browser["endpoint"].as_str().unwrap_or(""),
            Face::TEXT,
        );
        self.log_kv(
            "  ",
            Face::MUTED,
            "Profile",
            browser["profile"].as_str().unwrap_or(""),
            Face::TEXT,
        );
        let profile_matches = browser["profile_matches"].to_string();
        self.log_kv(
            "  ",
            Face::MUTED,
            "Profile match",
            &profile_matches,
            if browser["profile_matches"] == true {
                Face::HIGHLIGHT_BOLD
            } else if browser["profile_matches"] == false {
                Face::DANGER_BOLD
            } else {
                Face::MUTED
            },
        );
        self.log_kv(
            "  ",
            Face::MUTED,
            "Last verified sign-in",
            &format!(
                "{} (checked at {} ms UTC)",
                browser["last_verified_sign_in"], browser["checked_at"]
            ),
            Face::MUTED,
        );
        self.log_kv(
            "  ",
            Face::MUTED,
            "Owned sessions",
            &browser["owned_sessions"].to_string(),
            Face::ACCENT,
        );
        self.log(
            "  The endpoint and profile above belong to the Chrome host, which may be remote.",
        );
        self.log_spans(
            "/browser show: show window · /browser hide: hide to headless · /browser refresh: verify · /browser login: sign in",
            vec![
                ("· ".into(), Face::ACCENT),
                ("/browser show".into(), Face::ACCENT_BOLD),
                (": show window · ".into(), Face::MUTED),
                ("/browser hide".into(), Face::ACCENT_BOLD),
                (": hide · ".into(), Face::MUTED),
                ("/browser refresh".into(), Face::ACCENT_BOLD),
                (": verify · ".into(), Face::MUTED),
                ("/browser login".into(), Face::ACCENT_BOLD),
                (": sign in".into(), Face::MUTED),
            ],
        );
        if matches!(
            browser["login"]["state"].as_str(),
            Some("waiting" | "refreshing")
        ) {
            self.log("Complete sign-in on the Chrome host. Model choices will load automatically.");
        } else if matches!(
            browser["account_state"].as_str(),
            Some("awaiting_human" | "auth_required")
        ) {
            let auth_required = browser["account_state"] == "auth_required";
            self.log_human_action_guide(auth_required, browser["headless"].as_bool(), false);
        }
    }

    fn handle_finished(&mut self, job: Value) {
        if let Some(conv_id) = job["conversation_id"].as_str().filter(|s| !s.is_empty()) {
            self.conversation_id = Some(conv_id.to_owned());
        }
        let pending = self.pending_temporary.take();
        self.conversation_temporary = job_session_is_temporary(&job)
            .or(pending)
            .or(self.conversation_temporary);
        self.active = None;
        self.active_model = None;
        self.stream.clear();
        self.thinking_preview.clear();
        self.subagents.clear();
        self.tool_approvals.clear();
        self.answered_approvals.clear();
        self.seal_active_tool_block(true);
        self.phase = job["state"].as_str().unwrap_or("finished").into();
        if let Some(text) = job["result"]["final_response"].as_str() {
            if text != self.last_response {
                self.log_markdown(text);
            }
            if !text.is_empty() {
                self.copy_response = text.into();
            }
        }
        if !job["error"].is_null() {
            self.log_error(&format!("! {}", job["error"]));
        }
        let changed = job["result"]["files_changed"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        let elapsed = format!("{:.1}s", self.started.elapsed().as_secs_f64());
        let files_label = format!("{changed} file(s) changed");
        let phase_face = if self.phase == "completed" {
            Face::HIGHLIGHT_BOLD
        } else {
            Face::DANGER_BOLD
        };
        let files_face = if changed > 0 {
            Face::HIGHLIGHT_BOLD
        } else {
            Face::MUTED
        };
        let phase_copy = self.phase.clone();
        if self.debug.level > 0 {
            let err_str = if job["error"].is_null() {
                String::new()
            } else {
                format!(" · error={}", job["error"])
            };
            self.write_debug_log(
                "job_finished",
                &format!("state={phase_copy} · elapsed={elapsed} · {files_label}{err_str}"),
            );
        }
        self.log_spans(
            &format!("\n  {phase_copy} · {elapsed} · {files_label}"),
            vec![
                ("\n  ".into(), Face::MUTED),
                (phase_copy, phase_face),
                (" · ".into(), Face::MUTED),
                (elapsed, Face::ACCENT),
                (" · ".into(), Face::MUTED),
                (files_label, files_face),
            ],
        );
        if job["state"] != "completed" {
            self.log("Send a follow-up to continue from the recorded state.");
        }
    }

    fn handle_agent_event(&mut self, event: crate::domain::Event) {
        match event.kind.as_str() {
            "started" => {
                if self.debug.level >= 1 {
                    let ws = event.data["workspace"].as_str().unwrap_or("");
                    let temp = event.data["temp_chat"].as_bool().unwrap_or(false);
                    self.log_debug(
                        "agent",
                        &format!(
                            "initialized · workspace={} · temp_chat={temp}",
                            clip(ws, 60)
                        ),
                    );
                }
            }
            crate::domain::EVENT_BROWSER_TURN => {
                if self.debug.level >= 2 {
                    let run_id = event.data["run_id"].as_str().unwrap_or("");
                    self.log_debug("browser", &format!("turn submitted run_id={run_id}"));
                }
            }
            crate::domain::EVENT_BROWSER_SESSION => {
                if self.debug.level >= 1 {
                    let sid = event.data["session_id"].as_str().unwrap_or("");
                    self.log_debug("session", &format!("active session={sid}"));
                }
            }
            "browser_stage" => {
                let stage = event.data["stage"].as_str().unwrap_or("browser");
                let detail = event.data["detail"].as_str().unwrap_or("");
                match stage {
                    "thinking" => {
                        let label = detail
                            .split_once(" thinking ")
                            .map_or(detail, |(_, rest)| {
                                &detail[detail.len() - rest.len() - "thinking ".len()..]
                            })
                            .trim();
                        self.phase = if label.is_empty() {
                            "thinking".into()
                        } else {
                            label.to_owned()
                        };
                        if self.stream.is_empty() && !detail.is_empty() {
                            self.thinking_preview = detail.to_owned();
                        }
                        if self.debug.level >= 1 {
                            self.log_debug(stage, detail);
                        }
                    }
                    "generating" => {
                        self.thinking_preview.clear();
                        self.phase = "answering".into();
                        if self.debug.level >= 1 {
                            self.log_debug(stage, detail);
                        }
                    }
                    _ => {
                        if self.debug.level >= 1 {
                            self.phase = match stage {
                                "acquiring_tab" => "tab:open",
                                "checking_tab" => "tab:check",
                                "navigating_tab" => "tab:nav",
                                "selecting_model" => "set model",
                                "uploading" => "uploading",
                                "prompt_prepared" => "prompting",
                                "submitting" => "submitting",
                                "waiting_stream" => "waiting",
                                "cache_hit" => "cache hit",
                                "session_fallback" => "new tab",
                                other => other,
                            }
                            .into();
                            self.log_debug(stage, detail);
                        }
                    }
                }
            }
            "model_input" => {
                let chars = event.data["chars"].as_u64().unwrap_or(0) as usize;
                self.in_chars += chars;
                if self.debug.level == 1 {
                    self.log_debug(
                        "prompt",
                        &format!("step input {chars}c (turn total {}c)", self.in_chars),
                    );
                }
            }
            "request_payload" => {
                if self.debug.level >= 2 {
                    let req_json = serde_json::to_string_pretty(&event.data["request"])
                        .unwrap_or_else(|_| event.data["request"].to_string());
                    let prompt = event.data["prompt"].as_str().unwrap_or("");
                    self.log_debug_block("request_meta", "", &req_json);
                    self.log_debug_block(
                        "request_prompt",
                        &format!(
                            "{} chars (turn total {}c)",
                            prompt.chars().count(),
                            self.in_chars
                        ),
                        prompt,
                    );
                }
            }
            "model_info" => {
                self.active_model = event.data["model"].as_str().map(str::to_owned);
                if self.debug.level >= 1 {
                    let model = self.active_model.as_deref().unwrap_or("unknown");
                    self.log_debug("model", &format!("active model={model}"));
                }
            }
            "model_chunk" => {
                let text = event.data["text"].as_str().unwrap_or("");
                let was_empty = self.stream.is_empty();
                if !text.is_empty() {
                    self.thinking_preview.clear();
                }
                self.out_chars += text.chars().count();
                if self.stream.len() + text.len() <= 512 * 1024 {
                    self.stream.push_str(text);
                }
                if self.debug.level >= 1 && !text.is_empty() {
                    if was_empty {
                        self.log_debug("stream", "receiving response stream from Gemini Web");
                    }
                    self.phase = format!("stream {}c", self.out_chars);
                }
            }
            "model_response" => {
                self.stream.clear();
                self.thinking_preview.clear();
                let response = event.data["text"].as_str().unwrap_or("");
                if self.debug.level >= 2 {
                    let call_count = crate::tool_codec::parse(response)
                        .map(|(calls, _)| calls.len())
                        .unwrap_or(0);
                    self.log_debug(
                        "response",
                        &format!(
                            "step {} completed · {}c · {call_count} tool call(s)",
                            event.data["step"],
                            response.chars().count()
                        ),
                    );
                    if call_count > 0 || response.contains("[TOOL_CALL]") {
                        self.log_debug_block("response_raw", "", response);
                    }
                }
                if let Ok((_, prose)) = crate::tool_codec::parse(response) {
                    if !prose.is_empty() {
                        self.log_markdown(&format!("\n{prose}"));
                        self.copy_response = crate::tool_codec::visible_prose(response);
                    }
                }
                self.last_response = response.into();
            }
            "tool_approval_requested" => {
                let approval_id = event.data["id"].as_str().unwrap_or("").to_owned();
                let step = event.data["step"].as_u64().unwrap_or(0) as usize;
                let source = event.data["source"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned);
                let calls = event.data["calls"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let call_indices: Vec<usize> = event.data["call_indices"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as usize))
                            .collect()
                    })
                    .unwrap_or_default();
                if !approval_id.is_empty()
                    && !self.answered_approvals.contains(&approval_id)
                    && !self
                        .tool_approvals
                        .iter()
                        .any(|item| item.id == approval_id)
                {
                    self.tool_approvals.push_back(PendingToolApproval {
                        id: approval_id.clone(),
                        step,
                        source: source.clone(),
                        call_count: calls.len(),
                    });
                    self.tool_blocks.on_approval_requested(
                        approval_id,
                        step,
                        calls,
                        &call_indices,
                        source,
                        self.layout.inspector.follow,
                    );
                    self.phase = "approval (Y/N)".into();
                    if self.debug.level >= 1 {
                        self.log_debug(
                            "approval",
                            &format!("waiting Y/N for {} tool(s) at step {step}", calls.len()),
                        );
                    }
                }
            }
            "tool_approval_resolved" => {
                let approval_id = event.data["id"].as_str().unwrap_or("");
                let approved = event.data["approved"] == true;
                if let Some(idx) = self
                    .tool_approvals
                    .iter()
                    .position(|item| item.id == approval_id)
                {
                    self.tool_approvals.remove(idx);
                }
                self.tool_blocks.on_approval_resolved(approval_id, approved);
                if !self.answered_approvals.remove(approval_id)
                    && self.active.is_some()
                    && self.tool_approvals.is_empty()
                {
                    self.phase = if approved { "working" } else { "thinking" }.into();
                }
            }
            "tool_started" => {
                let step = event.data["step"].as_u64().unwrap_or(0) as usize;
                let index = event.data["index"].as_u64().unwrap_or(0) as usize;
                let tool = event.data["tool"].as_str().unwrap_or("tool");
                self.phase = if self.debug.level >= 1 {
                    format!("tool:{tool}")
                } else {
                    "working".into()
                };
                let args = &event.data["args"];
                self.tool_blocks.on_tool_started(
                    step,
                    index,
                    tool.to_owned(),
                    args.clone(),
                    self.layout.inspector.follow,
                );
                if self.debug.level >= 2 {
                    let pretty_args =
                        serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
                    self.log_debug_block("tool_args", tool, &pretty_args);
                }
            }
            "tool_output_chunk" => {
                let step = event.data["step"].as_u64().unwrap_or(0) as usize;
                let index = event.data["index"].as_u64().unwrap_or(0) as usize;
                let stream = match event.data["stream"].as_str() {
                    Some("stderr") => OutputStream::Stderr,
                    _ => OutputStream::Stdout,
                };
                let text = event.data["text"].as_str().unwrap_or("");
                self.tool_blocks
                    .on_tool_output_chunk(step, index, stream, text);
            }
            "tool_finished" => {
                let step = event.data["step"].as_u64().unwrap_or(0) as usize;
                let index = event.data["index"].as_u64().unwrap_or(0) as usize;
                let ok = event.data["ok"] != false;
                let elapsed = event.data["elapsed_ms"].as_u64().unwrap_or(0);
                let tool = event.data["tool"].as_str().unwrap_or("tool");
                let out = event.data["output"].as_str().unwrap_or("");
                self.tool_blocks
                    .on_tool_finished(step, index, tool, ok, out.to_owned(), elapsed);
                if self.debug.level >= 2 {
                    let header =
                        format!("{tool} · {} · {elapsed}ms", if ok { "ok" } else { "err" });
                    if out.is_empty() {
                        self.log_debug("tool_done", &header);
                    } else {
                        self.log_debug_block("tool_done", &header, out);
                    }
                } else if self.debug.level == 1 {
                    self.log_debug(
                        "tool_done",
                        &format!("{tool} · {} · {elapsed}ms", if ok { "ok" } else { "err" }),
                    );
                }
            }
            "retrying" => {
                if self.debug.level >= 1 {
                    self.phase = format!("retry #{}", event.data["attempt"]);
                    let err = event.data["error"].as_str().unwrap_or("");
                    self.log_debug(
                        "retry",
                        &format!(
                            "step {} attempt #{}: {}",
                            event.data["step"],
                            event.data["attempt"],
                            if self.debug.level >= 2 {
                                err.to_owned()
                            } else {
                                clip(err, 160)
                            }
                        ),
                    );
                }
            }
            "subagent_started" => {
                let id = event.data["id"].as_str().unwrap_or("").to_owned();
                let label = event.data["label"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| event.data["task"].as_str())
                    .unwrap_or("worker")
                    .to_owned();
                if self.debug.level >= 1 && !id.is_empty() {
                    self.log_debug("subagent", &format!("started [{label}] ({id})"));
                }
                if !id.is_empty() {
                    self.tool_blocks.on_subagent_started(&id, &label);
                    if let Some(existing) = self.subagents.iter_mut().find(|s| s.id == id) {
                        existing.label = label;
                        existing.phase = "starting".into();
                        existing.detail.clear();
                    } else {
                        self.subagents.push(SubagentView {
                            id,
                            label,
                            phase: "starting".into(),
                            detail: String::new(),
                            started: Instant::now(),
                        });
                    }
                }
            }
            "subagent_progress" => {
                let id = event.data["id"].as_str().unwrap_or("");
                let phase = event.data["phase"].as_str().unwrap_or("working").to_owned();
                let detail = event.data["detail"].as_str().unwrap_or("").to_owned();
                if self.debug.level >= 2 && !id.is_empty() {
                    let label = event.data["label"].as_str().unwrap_or("worker");
                    self.log_debug(
                        "subagent",
                        &format!("[{label}] {phase} · {}", clip(&detail, 120)),
                    );
                }
                if !id.is_empty() {
                    self.tool_blocks.on_subagent_progress(id, &phase, &detail);
                }
                if let Some(sub) = self.subagents.iter_mut().find(|s| s.id == id) {
                    sub.phase = phase;
                    sub.detail = detail;
                } else if !id.is_empty() {
                    let label = event.data["label"].as_str().unwrap_or("worker").to_owned();
                    self.subagents.push(SubagentView {
                        id: id.to_owned(),
                        label,
                        phase,
                        detail,
                        started: Instant::now(),
                    });
                }
            }
            "subagent_finished" => {
                let id = event.data["id"].as_str().unwrap_or("");
                let ok = event.data["ok"] != false;
                let elapsed = event.data["elapsed_ms"].as_u64().unwrap_or(0);
                if !id.is_empty() {
                    self.tool_blocks.on_subagent_finished(id, ok, elapsed);
                }
                if self.debug.level >= 1 && !id.is_empty() {
                    let label = event.data["label"].as_str().unwrap_or("worker");
                    self.log_debug(
                        "subagent",
                        &format!(
                            "finished [{label}] · {} · {elapsed}ms",
                            if ok { "ok" } else { "err" }
                        ),
                    );
                }
                self.subagents.retain(|s| s.id != id);
            }
            "step_started" => {
                self.stream.clear();
                self.thinking_preview.clear();
                let step = event.data["step"].as_u64().unwrap_or(0);
                self.phase = format!("thinking {step}");
                if self.debug.level >= 1 {
                    self.log_debug("step", &format!("step {step}: building prompt"));
                }
            }
            "steered" => self.log("  Additional instruction delivered."),
            "warning" => self.log(&format!(
                "  {}",
                event.data["message"].as_str().unwrap_or("")
            )),
            other => {
                if self.debug.level >= 2 {
                    self.write_debug_log(&format!("event:{other}"), &event.data.to_string());
                }
            }
        }
    }

    pub(super) fn toggle_inspector_mode(&mut self) {
        let mut cfg = self.layout.inspector;
        if cfg.mode == InspectorMode::Off {
            cfg.mode = if self.inspector_last_active_mode == InspectorMode::Off {
                InspectorMode::Inline
            } else {
                self.inspector_last_active_mode
            };
        } else {
            self.inspector_last_active_mode = cfg.mode;
            cfg.mode = InspectorMode::Off;
        }
        self.apply_inspector_config(cfg, false);
    }

    pub(super) fn toggle_inspector_zoom(&mut self) {
        self.shortcuts_open = false;
        self.shortcuts_scroll = 0;
        if self.inspector_zoom {
            self.inspector_zoom = false;
            self.inspector_zoom_scroll = 0;
        } else if self.tool_blocks.focused_block().is_some() {
            self.inspector_zoom = true;
            self.inspector_zoom_scroll = 0;
            self.clear_selection();
        }
    }

    pub(super) fn can_open_shortcuts(&self) -> bool {
        self.editor.text.is_empty()
            && self.history_clear_confirmation.is_none()
            && self.menu.is_none()
            && !self.inspector_zoom
            && self.tool_approvals.is_empty()
            && self.completion.is_none()
            && self.mention.is_none()
    }

    pub(super) fn show_footer_shortcut_hint(&self) -> bool {
        !self.busy() && !self.shortcuts_open && self.can_open_shortcuts()
    }

    pub(super) fn open_shortcuts(&mut self) {
        self.shortcuts_open = true;
        self.shortcuts_scroll = 0;
        self.clear_selection();
    }

    pub(super) fn close_shortcuts(&mut self) {
        self.shortcuts_open = false;
        self.shortcuts_scroll = 0;
    }

    pub(super) fn scroll_shortcuts(&mut self, up: bool, lines: usize, to_edge: Option<bool>) {
        if !self.shortcuts_open {
            return;
        }
        let width = self
            .painted_layout
            .map(|l| usize::from(l.width))
            .unwrap_or_else(|| usize::from(self.painted_size.0.max(80)));
        let avail_rows = self
            .painted_layout
            .map(|l| usize::from(l.transcript.len))
            .unwrap_or(16);
        let (total_rows, visible_rows) = super::render::shortcuts_panel_metrics(width, avail_rows);
        let max_scroll = total_rows.saturating_sub(visible_rows);
        self.shortcuts_scroll = match to_edge {
            Some(true) => 0,
            Some(false) => max_scroll,
            None => {
                let current = self.shortcuts_scroll.min(max_scroll);
                if up {
                    current.saturating_sub(lines)
                } else {
                    current.saturating_add(lines).min(max_scroll)
                }
            }
        };
    }

    pub(super) fn select_prev_next_tool(&mut self, next: bool) -> bool {
        let changed = self.tool_blocks.select_prev_next(next);
        if changed {
            self.inspector_zoom_scroll = 0;
        }
        changed
    }

    pub(super) fn toggle_inspector_follow(&mut self) -> bool {
        let changed = self.tool_blocks.toggle_follow();
        if changed {
            self.inspector_zoom_scroll = 0;
        }
        changed
    }

    pub(super) fn scroll_focused_inspector(&mut self, up: bool, lines: usize) {
        let Some(block_id) = self.tool_blocks.focused_id() else {
            return;
        };
        let (total, body_rows) = if let Some(pane) = self
            .painted_inspector
            .as_ref()
            .filter(|p| p.block_id == block_id)
        {
            (pane.detail_lines.len(), pane.body_rows)
        } else if let Some(pb) = self
            .painted_blocks
            .iter()
            .find(|p| p.block_id == block_id && p.inspector_detail_lines.is_some())
        {
            (
                pb.inspector_detail_lines.as_ref().map_or(0, |d| d.len()),
                pb.inspector_body_rows,
            )
        } else {
            let width = self.painted_size.0.max(80);
            let split = HorizontalSplit::compute(width, &self.layout.inspector);
            let right_w = split.map_or(usize::from(width), |s| usize::from(s.right.len));
            let detail = self
                .tool_blocks
                .detail_lines(block_id, right_w.saturating_sub(1).max(1));
            let rows = self.painted_layout.map_or(10, |l| {
                usize::from(l.transcript.len).saturating_sub(2).max(1)
            });
            (detail.len(), rows)
        };
        let max_scroll = total.saturating_sub(body_rows.max(1));
        self.tool_blocks
            .scroll_block(block_id, up, lines, max_scroll);
    }

    pub(super) fn scroll_zoom_overlay(&mut self, up: bool, lines: usize, to_edge: Option<bool>) {
        let Some(block_id) = self.tool_blocks.focused_id() else {
            return;
        };
        let (total, body_rows) = if let Some(pane) = self.painted_inspector.as_ref() {
            (pane.detail_lines.len(), pane.body_rows)
        } else {
            let width = self.painted_size.0.max(80);
            let detail = self
                .tool_blocks
                .detail_lines(block_id, usize::from(width).saturating_sub(2).max(1));
            let rows = self.painted_layout.map_or(16, |l| {
                usize::from(l.transcript.len).saturating_sub(2).max(1)
            });
            (detail.len(), rows)
        };
        let max_scroll = total.saturating_sub(body_rows.max(1));
        self.inspector_zoom_scroll = match to_edge {
            Some(true) => max_scroll,
            Some(false) => 0,
            None => {
                let current = self.inspector_zoom_scroll.min(max_scroll);
                if up {
                    current.saturating_add(lines).min(max_scroll)
                } else {
                    current.saturating_sub(lines)
                }
            }
        };
    }

    pub(super) fn transcript_lines_for(
        &self,
        layout: FrameLayout,
        spinner: &str,
    ) -> (TranscriptLines, Option<HorizontalSplit>, SplitCacheKey) {
        let viewport = usize::from(layout.transcript.len);
        let side_split = layout.inspector_rect.is_some();
        let inline_split = if self.layout.inspector.mode == InspectorMode::Inline {
            HorizontalSplit::compute(layout.width, &self.layout.inspector)
        } else {
            None
        };
        let split_active = side_split || inline_split.is_some();
        let wrap_width = if side_split {
            usize::from(layout.transcript_left.len)
        } else {
            usize::from(layout.width)
        };
        let block_left_width = if let Some(split) = inline_split {
            usize::from(split.left.len)
        } else if side_split {
            usize::from(layout.transcript_left.len)
        } else {
            usize::from(layout.width).saturating_sub(1)
        };
        let split_key = if self.tool_blocks.is_empty() {
            SplitCacheKey::default()
        } else {
            SplitCacheKey {
                block_left_width,
                split_active,
                inline_split_left: inline_split.map_or(0, |s| s.left.len),
                inline_split_right: inline_split.map_or(0, |s| s.right.len),
                after_turn: self.layout.inspector.after_turn,
                min_block_rows: if inline_split.is_some() {
                    inspector::sealed_expanded_block_rows(viewport)
                } else {
                    0
                },
            }
        };
        let lines = self.transcript.lines_with_blocks(
            &self.stream,
            wrap_width,
            &self.tool_blocks,
            &self.tool_approvals,
            split_key,
            inline_split,
            viewport,
            spinner,
        );
        (lines, inline_split, split_key)
    }

    #[cfg(test)]
    pub(super) fn transcript_lines(&self, width: usize) -> Vec<markdown::Line> {
        let layout = self
            .compute_layout(width as u16, 24)
            .map(|(l, _)| l)
            .unwrap_or_else(|| {
                FrameLayout::compute_with_inspector(
                    &self.layout,
                    (width as u16).max(12),
                    24,
                    1,
                    (0, 0),
                    0,
                    0,
                    !self.tool_blocks.is_empty(),
                )
                .unwrap()
            });
        self.transcript_lines_for(layout, "⠋").0.to_vec()
    }

    pub(super) fn reset_scroll(&mut self) {
        self.scroll = 0;
        self.scroll_relative = false;
        self.scroll_anchor_width = 0;
        self.scroll_anchor_total_lines = 0;
        self.scroll_anchor_evicted_lines = 0;
        self.scroll_anchor_deficit = None;
        self.scroll_anchor_stream_len = 0;
        self.scroll_anchor_revision = 0;
        self.scroll_anchor_tool_rev = 0;
        self.has_unread_below = false;
    }

    pub(super) fn set_scroll_relative(&mut self, next: usize) {
        self.scroll = next;
        self.scroll_relative = next > 0;
        self.scroll_anchor_deficit = None;
        if next == 0 {
            self.has_unread_below = false;
        }
    }

    pub(super) fn scroll_bottom_badge(&self) -> ScrollBottomBadge {
        if !self.has_unread_below {
            ScrollBottomBadge::Idle
        } else if self.needs_animation() {
            ScrollBottomBadge::Streaming
        } else {
            ScrollBottomBadge::NewOutput
        }
    }

    pub(super) fn sync_scroll_anchor(
        &mut self,
        current_lines: usize,
        evicted_lines: u64,
        wrap_width: usize,
        visible_stream_len: usize,
    ) {
        let current_revision = self.transcript.revision();
        let current_tool_rev = self.tool_blocks.content_rev();
        let content_changed = current_revision != self.scroll_anchor_revision
            || current_tool_rev != self.scroll_anchor_tool_rev;
        let anchored = self.scroll > 0 || self.scroll_anchor_deficit.is_some();
        let has_valid_anchor = anchored
            && self.scroll_anchor_width > 0
            && self.scroll_anchor_total_lines > 0
            && (self.scroll == self.painted_effective_scroll || self.scroll_relative);
        if has_valid_anchor {
            if self.scroll_anchor_width == wrap_width {
                let evicted_delta = evicted_lines.saturating_sub(self.scroll_anchor_evicted_lines);
                let adjusted_lines = (current_lines as u64).saturating_add(evicted_delta);
                let anchor_lines = self.scroll_anchor_total_lines as u64;
                if adjusted_lines > anchor_lines {
                    let delta =
                        usize::try_from(adjusted_lines - anchor_lines).unwrap_or(usize::MAX);
                    if let Some(deficit) = self.scroll_anchor_deficit {
                        if delta > deficit {
                            self.scroll = self.scroll.saturating_add(delta - deficit);
                            self.scroll_anchor_deficit = None;
                        } else {
                            self.scroll_anchor_deficit = Some(deficit - delta);
                        }
                    } else {
                        self.scroll = self.scroll.saturating_add(delta);
                    }
                    if visible_stream_len > 0 || content_changed {
                        self.has_unread_below = true;
                    }
                } else if adjusted_lines < anchor_lines {
                    let delta =
                        usize::try_from(anchor_lines - adjusted_lines).unwrap_or(usize::MAX);
                    if let Some(deficit) = self.scroll_anchor_deficit {
                        self.scroll_anchor_deficit = Some(deficit.saturating_add(delta));
                    } else if delta >= self.scroll {
                        self.scroll_anchor_deficit = Some(delta - self.scroll);
                        self.scroll = 0;
                    } else {
                        self.scroll -= delta;
                    }
                    if content_changed || visible_stream_len > self.scroll_anchor_stream_len {
                        self.has_unread_below = true;
                    }
                } else if visible_stream_len > self.scroll_anchor_stream_len || content_changed {
                    self.has_unread_below = true;
                }
            } else {
                self.scroll_anchor_deficit = None;
                if content_changed || visible_stream_len > self.scroll_anchor_stream_len {
                    self.has_unread_below = true;
                }
            }
        } else {
            self.scroll_anchor_deficit = None;
        }
        if self.active.is_none() {
            self.scroll_anchor_deficit = None;
        }
        if self.scroll == 0 && self.scroll_anchor_deficit.is_none() {
            self.has_unread_below = false;
        }
        self.scroll_anchor_total_lines = current_lines;
        self.scroll_anchor_evicted_lines = evicted_lines;
        self.scroll_anchor_width = wrap_width;
        self.scroll_anchor_stream_len = visible_stream_len;
        self.scroll_anchor_revision = current_revision;
        self.scroll_anchor_tool_rev = current_tool_rev;
    }

    pub(super) fn scroll_transcript(
        &mut self,
        up: bool,
        lines: usize,
        width: usize,
        height: usize,
    ) {
        let computed = self.compute_layout(width as u16, height as u16);
        let viewport = computed
            .as_ref()
            .map_or(1, |(layout, _)| usize::from(layout.transcript.len).max(1));
        if self.selection.is_some() && self.selection_target == SelectionTarget::Transcript {
            self.scroll_selection(up, lines, viewport);
            return;
        }
        // Release the previous frame's Rc snapshot so the cache can append new
        // items in-place (Rc refcount → 1) instead of deep-copying everything.
        self.painted_all_transcript.clear();
        let (total_lines, evicted_lines, visible_stream_len, anchor_w) =
            if let Some((layout, _)) = computed {
                let (all, _, _) = self.transcript_lines_for(layout, "⠋");
                let w = if layout.inspector_rect.is_some() {
                    usize::from(layout.transcript_left.len)
                } else {
                    usize::from(layout.width)
                };
                (all.len(), all.evicted_lines(), all.visible_stream_len(), w)
            } else {
                let empty_approvals = VecDeque::new();
                let split_key = SplitCacheKey {
                    block_left_width: width.saturating_sub(1),
                    ..SplitCacheKey::default()
                };
                let all = self.transcript.lines_with_blocks(
                    &self.stream,
                    width,
                    &self.tool_blocks,
                    &empty_approvals,
                    split_key,
                    None,
                    viewport,
                    "⠋",
                );
                (
                    all.len(),
                    all.evicted_lines(),
                    all.visible_stream_len(),
                    width,
                )
            };
        self.sync_scroll_anchor(total_lines, evicted_lines, anchor_w, visible_stream_len);
        let maximum = total_lines.saturating_sub(viewport);
        let current = self.scroll.min(maximum);
        let next = if up {
            current.saturating_add(lines)
        } else {
            current.saturating_sub(lines)
        }
        .min(maximum);
        self.set_scroll_relative(next);
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_drag = None;
        self.painted_inline_inspector_target = None;
    }

    pub(super) fn clear_painted_regions(&mut self) {
        if (self.scroll > 0 || self.scroll_anchor_deficit.is_some())
            && self.scroll == self.painted_effective_scroll
        {
            self.scroll_relative = true;
        }
        self.painted_transcript.clear();
        self.painted_all_transcript.clear();
        self.painted_transcript_start = 0;
        self.painted_effective_scroll = 0;
        self.painted_hint.clear();
        self.painted_composer.clear();
        self.painted_subagents.clear();
        self.painted_footer.clear();
        self.painted_blocks.clear();
        self.painted_inspector = None;
        self.painted_inline_inspector_target = None;
        self.divider_drag = false;
        self.painted_layout = None;
        self.scroll_bottom_button = None;
    }

    pub(super) fn scroll_wheel(&mut self, up: bool, width: usize, height: usize) {
        self.clear_selection();
        self.last_click = None;
        self.clipboard_notice = None;
        if self.deleting_conversation.is_some() || self.history_clear_confirmation.is_some() {
            return;
        }
        if self.menu.is_some() {
            self.move_menu(up);
        } else if self.inspector_zoom {
            self.scroll_zoom_overlay(up, 3, None);
        } else if self.shortcuts_open {
            self.scroll_shortcuts(up, 3, None);
        } else {
            self.scroll_transcript(up, 3, width, height);
        }
    }

    pub(super) fn scroll_wheel_at(
        &mut self,
        up: bool,
        column: u16,
        row: u16,
        width: usize,
        height: usize,
    ) {
        if self.deleting_conversation.is_some() || self.history_clear_confirmation.is_some() {
            return;
        }
        if self.menu.is_some() {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            self.move_menu(up);
            return;
        }
        if self.inspector_zoom {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            self.scroll_zoom_overlay(up, 3, None);
            return;
        }
        if self.shortcuts_open {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            self.scroll_shortcuts(up, 3, None);
            return;
        }
        if self.selection.is_some() && self.selection_target == SelectionTarget::Transcript {
            let (_, _, viewport) = self.transcript_bounds();
            self.scroll_selection(up, 3, viewport);
            self.extend_transcript_selection(column, row);
            if let Some(drag) = self.selection_drag.as_mut() {
                drag.moved = true;
                drag.column = column;
                drag.row = row;
            }
            return;
        }
        if self.mention.is_some()
            && self
                .painted_layout
                .is_some_and(|layout| layout.completion.contains(row))
        {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            self.move_mention(up);
            return;
        }
        // Right Inspector wheel (Side mode pane)
        if let Some(pane) = self
            .painted_inspector
            .as_ref()
            .filter(|pane| pane.rect.contains(column, row))
            .map(|pane| (pane.block_id, pane.detail_lines.len(), pane.body_rows))
        {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            let max_scroll = pane.1.saturating_sub(pane.2.max(1));
            self.tool_blocks.scroll_block(pane.0, up, 3, max_scroll);
            return;
        }
        // Right Inspector wheel (Inline sticky block pane)
        if let Some((block_id, total, body_rows)) = self.painted_blocks.iter().find_map(|pb| {
            pb.inspector_rect
                .filter(|rect| rect.contains(column, row))
                .map(|_| {
                    (
                        pb.block_id,
                        pb.inspector_detail_lines.as_ref().map_or(0, |d| d.len()),
                        pb.inspector_body_rows,
                    )
                })
        }) {
            self.clear_selection();
            self.last_click = None;
            self.clipboard_notice = None;
            let max_scroll = total.saturating_sub(body_rows.max(1));
            self.tool_blocks.scroll_block(block_id, up, 3, max_scroll);
            return;
        }
        self.scroll_wheel(up, width, height);
    }

    fn transcript_bounds(&self) -> (u16, u16, usize) {
        let fallback_start = u16::from(self.layout.show_header);
        if let Some(layout) = self.painted_layout {
            (
                layout.transcript.start,
                layout.transcript.end(),
                usize::from(layout.transcript.len).max(1),
            )
        } else {
            let len = self.painted_transcript.len().max(1);
            let end = fallback_start.saturating_add(len.min(usize::from(u16::MAX)) as u16);
            (fallback_start, end, len)
        }
    }

    pub(super) fn selection_view_range(&self, lines_len: usize, viewport: usize) -> (usize, usize) {
        let viewport = viewport.max(1);
        let max_scroll = lines_len.saturating_sub(viewport);
        let effective_scroll = self.scroll.min(max_scroll);
        let end = lines_len.saturating_sub(effective_scroll);
        let start = end.saturating_sub(viewport);
        (start, end)
    }

    pub(super) fn scroll_selection(&mut self, up: bool, lines: usize, viewport: usize) -> bool {
        let Some(selection) = self.selection.as_ref() else {
            return false;
        };
        if self.selection_target != SelectionTarget::Transcript {
            return false;
        }
        let viewport = viewport.max(1);
        let max_scroll = selection.lines.len().saturating_sub(viewport);
        if max_scroll == 0 {
            return false;
        }
        let current = self.scroll.min(max_scroll);
        let next = if up {
            current.saturating_add(lines)
        } else {
            current.saturating_sub(lines)
        }
        .min(max_scroll);
        self.set_scroll_relative(next);
        next != current
    }

    fn extend_transcript_selection(&mut self, column: u16, row: u16) {
        let (transcript_start, transcript_end, viewport) = self.transcript_bounds();
        let max_col = self
            .painted_layout
            .map_or(self.painted_size.0, |l| l.transcript_left.end())
            .max(1);
        let Some(selection_len) = self.selection.as_ref().map(|s| s.lines.len()) else {
            return;
        };
        let (view_start, view_end) = self.selection_view_range(selection_len, viewport);
        let (sel_col, sel_row) = if row < transcript_start {
            (0, view_start)
        } else if row >= transcript_end {
            (usize::from(max_col), view_end.saturating_sub(1))
        } else {
            let abs_row = view_start + usize::from(row - transcript_start);
            if abs_row >= selection_len {
                (usize::from(max_col), selection_len.saturating_sub(1))
            } else {
                (usize::from(column.min(max_col.saturating_sub(1))), abs_row)
            }
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.extend(sel_col, sel_row);
        }
    }

    fn active_inspector_selection_metrics(
        &self,
    ) -> Option<(super::layout::Rect, u16, usize, usize)> {
        if let Some(target_id) = self.painted_inline_inspector_target {
            if let Some(pb) = self.painted_blocks.iter().find(|p| p.block_id == target_id) {
                if let Some(rect) = pb.inspector_rect {
                    return Some((
                        rect,
                        pb.inspector_header_rows,
                        pb.inspector_body_rows,
                        pb.inspector_body_start,
                    ));
                }
            }
        }
        self.painted_inspector
            .as_ref()
            .map(|pane| (pane.rect, pane.header_rows, pane.body_rows, pane.body_start))
    }

    fn extend_inspector_selection(&mut self, column: u16, row: u16) {
        let Some((rect, header_rows, body_rows, body_start)) =
            self.active_inspector_selection_metrics()
        else {
            return;
        };
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        if selection.lines.is_empty() || body_rows == 0 {
            return;
        }
        let body_top = rect.row.start.saturating_add(header_rows);
        let body_bottom = body_top.saturating_add(body_rows as u16);
        let text_col_start = rect.col.start.saturating_add(1);
        let max_rel_col = usize::from(rect.col.len.saturating_sub(2));
        let (sel_col, sel_row) = if row < body_top {
            (0, body_start.min(selection.lines.len() - 1))
        } else if row >= body_bottom {
            let last_vis = (body_start + body_rows.saturating_sub(1))
                .min(selection.lines.len().saturating_sub(1));
            (max_rel_col, last_vis)
        } else {
            let rel_row = usize::from(row - body_top);
            let abs_row = (body_start + rel_row).min(selection.lines.len().saturating_sub(1));
            let rel_col = usize::from(column.saturating_sub(text_col_start)).min(max_rel_col);
            (rel_col, abs_row)
        };
        selection.extend(sel_col, sel_row);
    }

    fn target_band(&self, target: SelectionTarget) -> Option<(u16, u16, usize)> {
        let layout = self.painted_layout?;
        let (span, col_offset) = match target {
            SelectionTarget::Transcript | SelectionTarget::Inspector => return None,
            SelectionTarget::Hint => (layout.hint, 0),
            SelectionTarget::Composer => (layout.composer, 2),
            SelectionTarget::Subagents => (layout.subagents, 0),
            SelectionTarget::Footer => (layout.footer, 0),
        };
        (span.len > 0).then_some((span.start, span.end(), col_offset))
    }

    fn extend_band_selection(&mut self, column: u16, row: u16) {
        let Some((band_start, band_end, col_offset)) = self.target_band(self.selection_target)
        else {
            return;
        };
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        let (sel_col, sel_row) = if row < band_start {
            (0, 0)
        } else if row >= band_end || usize::from(row - band_start) >= selection.lines.len() {
            (
                usize::from(self.painted_size.0),
                selection.lines.len().saturating_sub(1),
            )
        } else {
            (
                usize::from(column.min(self.painted_size.0.saturating_sub(1)))
                    .saturating_sub(col_offset),
                usize::from(row - band_start),
            )
        };
        selection.extend(sel_col, sel_row);
    }

    fn hit_test_selection_target(&self, row: u16) -> Option<(SelectionTarget, u16, usize)> {
        if let Some(layout) = self.painted_layout {
            if layout.transcript.contains(row) {
                Some((SelectionTarget::Transcript, layout.transcript.start, 0))
            } else if layout.hint.contains(row) {
                Some((SelectionTarget::Hint, layout.hint.start, 0))
            } else if layout.composer.contains(row) {
                Some((SelectionTarget::Composer, layout.composer.start, 2))
            } else if layout.subagents.contains(row) {
                Some((SelectionTarget::Subagents, layout.subagents.start, 0))
            } else if layout.footer.contains(row) {
                Some((SelectionTarget::Footer, layout.footer.start, 0))
            } else {
                None
            }
        } else {
            let fallback_start = u16::from(self.layout.show_header);
            (row >= fallback_start).then_some((SelectionTarget::Transcript, fallback_start, 0))
        }
    }

    pub(super) fn tick_selection_autoscroll(&mut self) -> bool {
        if self.selection.is_none() || self.selection_target != SelectionTarget::Transcript {
            return false;
        }
        let Some(drag) = self.selection_drag else {
            return false;
        };
        let (transcript_start, transcript_end, viewport) = self.transcript_bounds();
        let now = Instant::now();
        let interval = Duration::from_millis(50);
        if now.saturating_duration_since(drag.last_autoscroll) < interval {
            return false;
        }
        let scrolled = if drag.row < transcript_start {
            self.scroll_selection(true, 1, viewport)
        } else if drag.row >= transcript_end {
            let step = if drag.row >= transcript_end.saturating_add(2) {
                2
            } else {
                1
            };
            self.scroll_selection(false, step, viewport)
        } else if transcript_start == 0 && drag.row == 0 && drag.moved {
            let dwell = if drag.anchor_viewport_row > 0 {
                Duration::from_millis(180)
            } else {
                Duration::from_millis(350)
            };
            if drag
                .entered_top_at
                .is_some_and(|at| now.saturating_duration_since(at) >= dwell)
            {
                self.scroll_selection(true, 1, viewport)
            } else {
                false
            }
        } else {
            false
        };
        if !scrolled {
            return false;
        }
        if let Some(drag) = self.selection_drag.as_mut() {
            drag.last_autoscroll = now;
        }
        self.extend_transcript_selection(drag.column, drag.row);
        true
    }

    fn start_inspector_selection(
        &mut self,
        mouse: event::MouseEvent,
        rect: super::layout::Rect,
        header_rows: u16,
        body_rows: usize,
        body_start: usize,
        lines: Vec<markdown::Line>,
        inline_target: Option<BlockId>,
    ) {
        let body_top = rect.row.start.saturating_add(header_rows);
        let body_bottom = body_top.saturating_add(body_rows as u16);
        if body_rows == 0 || mouse.row < body_top || mouse.row >= body_bottom {
            self.clear_selection();
            self.last_click = None;
            return;
        }
        const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(600);
        let is_double = self.last_click.is_some_and(|prev| {
            prev.at.elapsed() <= DOUBLE_CLICK_WINDOW
                && prev.row == mouse.row
                && prev.column.abs_diff(mouse.column) <= 2
        });
        let rel_row = usize::from(mouse.row - body_top);
        let abs_row = body_start + rel_row;
        let text_col_start = rect.col.start.saturating_add(1);
        let rel_col = usize::from(mouse.column.saturating_sub(text_col_start));
        if is_double {
            self.last_click = None;
            self.selection = selection::Selection::new_block(lines, rel_col, abs_row);
        } else {
            self.last_click = Some(LastClick {
                at: Instant::now(),
                column: mouse.column,
                row: mouse.row,
            });
            self.selection = selection::Selection::new(lines, rel_col, abs_row);
        }
        if self.selection.is_some() {
            self.selection_target = SelectionTarget::Inspector;
            self.selection_drag = None;
            self.painted_inline_inspector_target = inline_target;
        } else {
            self.last_click = None;
            self.painted_inline_inspector_target = None;
        }
    }

    pub(super) fn transcript_mouse(&mut self, mouse: event::MouseEvent) -> Option<String> {
        if self.menu.is_some()
            || self.history_clear_confirmation.is_some()
            || self.shortcuts_open
        {
            self.clear_selection();
            self.divider_drag = false;
            self.last_click = None;
            return None;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.scroll_bottom_button.is_some_and(|(row, start, end)| {
                    mouse.row == row && mouse.column >= start && mouse.column < end
                }) {
                    self.reset_scroll();
                    self.clear_selection();
                    self.last_click = None;
                    self.clipboard_notice = None;
                    return None;
                }
                self.clipboard_notice = None;

                // 1. Divider drag hit (±1 col), disabled during Alt+Z zoom overlay.
                if !self.inspector_zoom {
                    let hit_side_divider = self.painted_layout.is_some_and(|layout| {
                        layout.transcript.contains(mouse.row)
                            && layout
                                .divider_x
                                .is_some_and(|dx| mouse.column.abs_diff(dx) <= 1)
                    });
                    let hit_inline_divider = self.painted_blocks.iter().any(|pb| {
                        mouse.row >= pb.vis_row_start
                            && mouse.row < pb.vis_row_end
                            && pb
                                .divider_x
                                .is_some_and(|dx| mouse.column.abs_diff(dx) <= 1)
                    });
                    if hit_side_divider || hit_inline_divider {
                        self.divider_drag = true;
                        self.clear_selection();
                        self.last_click = None;
                        return None;
                    }
                }

                // 2. Inspector footer badge click (`[● Follow]` / `[▶ Follow]` / `[↓ Live]`).
                if let Some((block_id, action)) = self.painted_inspector.as_ref().and_then(|pane| {
                    pane.live_badge.and_then(|(r, s, e, act)| {
                        (mouse.row == r && mouse.column >= s && mouse.column < e)
                            .then_some((pane.block_id, act))
                    })
                }) {
                    match action {
                        InspectorBadgeAction::ToggleFollow => {
                            self.toggle_inspector_follow();
                        }
                        InspectorBadgeAction::JumpToBottom => {
                            if self.inspector_zoom {
                                self.inspector_zoom_scroll = 0;
                            } else {
                                self.tool_blocks.jump_to_live(block_id);
                            }
                        }
                    }
                    self.clear_selection();
                    self.last_click = None;
                    return None;
                }
                if let Some((block_id, action)) = self.painted_blocks.iter().find_map(|pb| {
                    pb.live_badge.and_then(|(r, s, e, act)| {
                        (mouse.row == r && mouse.column >= s && mouse.column < e)
                            .then_some((pb.block_id, act))
                    })
                }) {
                    match action {
                        InspectorBadgeAction::ToggleFollow => {
                            self.toggle_inspector_follow();
                        }
                        InspectorBadgeAction::JumpToBottom => {
                            self.tool_blocks.jump_to_live(block_id);
                        }
                    }
                    self.clear_selection();
                    self.last_click = None;
                    return None;
                }

                // 3. Alt+Z fullscreen zoom overlay body selection.
                if self.inspector_zoom {
                    if let Some((rect, header_rows, body_rows, body_start, lines)) =
                        self.painted_inspector.as_ref().and_then(|pane| {
                            pane.rect.contains(mouse.column, mouse.row).then(|| {
                                (
                                    pane.rect,
                                    pane.header_rows,
                                    pane.body_rows,
                                    pane.body_start,
                                    (*pane.detail_lines).clone(),
                                )
                            })
                        })
                    {
                        self.start_inspector_selection(
                            mouse,
                            rect,
                            header_rows,
                            body_rows,
                            body_start,
                            lines,
                            None,
                        );
                    } else {
                        self.clear_selection();
                        self.last_click = None;
                    }
                    return None;
                }

                // 4. Side mode right Inspector selection.
                if let Some((block_id, rect, header_rows, body_rows, body_start, lines)) =
                    self.painted_inspector.as_ref().and_then(|pane| {
                        pane.rect.contains(mouse.column, mouse.row).then(|| {
                            (
                                pane.block_id,
                                pane.rect,
                                pane.header_rows,
                                pane.body_rows,
                                pane.body_start,
                                (*pane.detail_lines).clone(),
                            )
                        })
                    })
                {
                    self.tool_blocks.set_focused(block_id);
                    self.start_inspector_selection(
                        mouse,
                        rect,
                        header_rows,
                        body_rows,
                        body_start,
                        lines,
                        None,
                    );
                    return None;
                }

                // 5. Inline ToolBlock right Inspector selection or left list item / collapse click.
                if let Some((block_id, rect, header_rows, body_rows, body_start, lines)) =
                    self.painted_blocks.iter().find_map(|pb| {
                        let rect = pb.inspector_rect?;
                        rect.contains(mouse.column, mouse.row).then(|| {
                            (
                                pb.block_id,
                                rect,
                                pb.inspector_header_rows,
                                pb.inspector_body_rows,
                                pb.inspector_body_start,
                                pb.inspector_detail_lines
                                    .as_ref()
                                    .map(|d| (**d).clone())
                                    .unwrap_or_default(),
                            )
                        })
                    })
                {
                    self.tool_blocks.set_focused(block_id);
                    self.start_inspector_selection(
                        mouse,
                        rect,
                        header_rows,
                        body_rows,
                        body_start,
                        lines,
                        Some(block_id),
                    );
                    return None;
                }

                if let Some((block_id, kind)) = self.painted_blocks.iter().find_map(|pb| {
                    (mouse.row >= pb.vis_row_start
                        && mouse.row < pb.vis_row_end
                        && mouse.column >= pb.left_col_start
                        && mouse.column < pb.left_col_end)
                        .then(|| {
                            pb.row_map
                                .iter()
                                .find(|(r, _)| *r == mouse.row)
                                .map(|(_, k)| (pb.block_id, *k))
                        })
                        .flatten()
                }) {
                    match kind {
                        BlockRowKind::CollapsedHeader | BlockRowKind::Header => {
                            self.tool_blocks.toggle_collapse(block_id);
                            self.clear_selection();
                            self.last_click = None;
                            return None;
                        }
                        BlockRowKind::ToolItem(idx) => {
                            self.tool_blocks.select_item(block_id, idx);
                            self.clear_selection();
                            self.last_click = None;
                            return None;
                        }
                        _ => {}
                    }
                }

                // 6. Standard Transcript / band selection.
                let hit = self
                    .hit_test_selection_target(mouse.row)
                    .filter(|(target, _, _)| {
                        *target != SelectionTarget::Composer || !self.editor.text.is_empty()
                    });
                if let Some((target, band_start, col_offset)) = hit {
                    if mouse.column >= self.painted_size.0 {
                        self.last_click = None;
                        self.clear_selection();
                        return None;
                    }
                    const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(600);
                    let is_double = self.last_click.is_some_and(|prev| {
                        prev.at.elapsed() <= DOUBLE_CLICK_WINDOW
                            && prev.row == mouse.row
                            && prev.column.abs_diff(mouse.column) <= 2
                    });
                    let viewport_row = usize::from(mouse.row - band_start);
                    let column = usize::from(mouse.column).saturating_sub(col_offset);
                    let (lines, abs_row) = match target {
                        SelectionTarget::Transcript => {
                            if !self.painted_all_transcript.is_empty() {
                                let (_, _, viewport) = self.transcript_bounds();
                                let max_scroll =
                                    self.painted_all_transcript.len().saturating_sub(viewport);
                                let next = self.scroll.min(max_scroll);
                                if next == 0 {
                                    self.set_scroll_relative(0);
                                } else {
                                    self.scroll = next;
                                }
                                (
                                    self.painted_all_transcript.to_vec(),
                                    self.painted_transcript_start + viewport_row,
                                )
                            } else {
                                (self.painted_transcript.clone(), viewport_row)
                            }
                        }
                        SelectionTarget::Inspector => return None,
                        SelectionTarget::Hint => (self.painted_hint.clone(), viewport_row),
                        SelectionTarget::Composer => (self.painted_composer.clone(), viewport_row),
                        SelectionTarget::Subagents => {
                            (self.painted_subagents.clone(), viewport_row)
                        }
                        SelectionTarget::Footer => (self.painted_footer.clone(), viewport_row),
                    };
                    if is_double {
                        self.last_click = None;
                        self.selection = selection::Selection::new_block(lines, column, abs_row);
                    } else {
                        self.last_click = Some(LastClick {
                            at: Instant::now(),
                            column: mouse.column,
                            row: mouse.row,
                        });
                        self.selection = selection::Selection::new(lines, column, abs_row);
                    }
                    if self.selection.is_some() {
                        self.selection_target = target;
                        self.selection_drag =
                            (target == SelectionTarget::Transcript).then(|| SelectionDrag {
                                anchor_viewport_row: viewport_row,
                                column: mouse.column,
                                row: mouse.row,
                                moved: false,
                                entered_top_at: None,
                                last_autoscroll: Instant::now(),
                            });
                    } else {
                        self.last_click = None;
                        self.selection_drag = None;
                    }
                } else {
                    self.last_click = None;
                    self.clear_selection();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.divider_drag {
                    let total = self.painted_size.0.max(1);
                    let right_cols = total.saturating_sub(mouse.column.saturating_add(1));
                    let pct = ((u32::from(right_cols) * 100 + u32::from(total) / 2)
                        / u32::from(total))
                    .clamp(
                        u32::from(INSPECTOR_MIN_WIDTH_PCT),
                        u32::from(INSPECTOR_MAX_WIDTH_PCT),
                    ) as u8;
                    if self.layout.inspector.width_pct != pct {
                        self.layout.inspector.width_pct = pct;
                        self.tool_blocks.bump_layout_rev();
                    }
                    return None;
                }
                if self.last_click.is_some_and(|prev| {
                    prev.row != mouse.row || prev.column.abs_diff(mouse.column) > 1
                }) {
                    self.last_click = None;
                }
                self.selection.as_ref()?;
                if self.selection_target == SelectionTarget::Transcript {
                    let (transcript_start, transcript_end, viewport) = self.transcript_bounds();
                    let now = Instant::now();
                    let same_top_cell = self.selection_drag.as_ref().is_some_and(|drag| {
                        drag.moved && drag.column == mouse.column && drag.row == mouse.row
                    });
                    let scrolled = if mouse.row < transcript_start {
                        self.scroll_selection(true, 1, viewport)
                    } else if mouse.row >= transcript_end {
                        let step = if mouse.row >= transcript_end.saturating_add(2) {
                            2
                        } else {
                            1
                        };
                        self.scroll_selection(false, step, viewport)
                    } else if transcript_start == 0 && mouse.row == 0 && same_top_cell {
                        self.scroll_selection(true, 1, viewport)
                    } else {
                        false
                    };
                    if let Some(drag) = self.selection_drag.as_mut() {
                        let pos_changed = drag.column != mouse.column || drag.row != mouse.row;
                        drag.moved = true;
                        drag.column = mouse.column;
                        drag.row = mouse.row;
                        if transcript_start == 0 && mouse.row == 0 {
                            if drag.entered_top_at.is_none() || pos_changed {
                                drag.entered_top_at = Some(now);
                            }
                        } else {
                            drag.entered_top_at = None;
                        }
                        if scrolled {
                            drag.last_autoscroll = now;
                        }
                    }
                    self.extend_transcript_selection(mouse.column, mouse.row);
                } else if self.selection_target == SelectionTarget::Inspector {
                    self.extend_inspector_selection(mouse.column, mouse.row);
                } else {
                    self.extend_band_selection(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.divider_drag {
                    self.divider_drag = false;
                    let cfg = self.layout.inspector.normalized();
                    self.layout.inspector = cfg;
                    if let Some(prefs) = &self.preferences {
                        let _ = prefs.set_inspector_config(&cfg);
                    }
                    return None;
                }
                if self.selection.is_some() {
                    if self.selection_target == SelectionTarget::Transcript {
                        self.extend_transcript_selection(mouse.column, mouse.row);
                    } else if self.selection_target == SelectionTarget::Inspector {
                        self.extend_inspector_selection(mouse.column, mouse.row);
                    } else {
                        self.extend_band_selection(mouse.column, mouse.row);
                    }
                }
                let copied = self.selection.take().and_then(|selection| selection.text());
                self.selection_drag = None;
                self.painted_inline_inspector_target = None;
                let keep_arm_on_tap_jitter = self.last_click.is_some_and(|prev| {
                    prev.at.elapsed() <= Duration::from_millis(250)
                        && prev.row == mouse.row
                        && prev.column.abs_diff(mouse.column) <= 1
                });
                if copied.is_some() && !keep_arm_on_tap_jitter {
                    self.last_click = None;
                }
                return copied;
            }
            _ => {}
        }
        None
    }
}
