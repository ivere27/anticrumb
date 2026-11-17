//! Core TUI state, event reducer, debug/clipboard helpers, and scroll/selection logic.
use super::{
    clipboard,
    commands::{Choice, Completion, HistoryClearConfirmation, Menu, Selection},
    debug_log::DebugLogger,
    editor::{Draft, Editor},
    layout::{FrameLayout, LayoutConfig},
    markdown::{self, Face},
    preferences::{self, Preferences},
    selection, theme,
    transcript::{clip, inline_notice_parts, Entry, Transcript},
    usage,
    worker::{Command, Update},
};
use crate::cli::PromptOptions;
use crossterm::event::{self, MouseButton, MouseEventKind};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

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

#[derive(Clone, Debug)]
pub(super) struct SubagentView {
    pub id: String,
    pub label: String,
    pub phase: String,
    pub detail: String,
    pub started: Instant,
}

pub(super) struct State {
    pub options: PromptOptions,
    pub preferences: Option<Preferences>,
    pub theme: theme::Theme,
    pub theme_preview_origin: Option<theme::Theme>,
    pub catalog: Value,
    pub parent: Option<String>,
    pub active: Option<String>,
    pub phase: String,
    pub editor: Editor,
    pub transcript: Transcript,
    pub status: Value,
    pub last_status_at: Option<Instant>,
    pub status_disconnected: bool,
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
    pub scroll: usize,
    pub last_response: String,
    pub copy_response: String,
    pub painted_transcript: Vec<markdown::Line>,
    pub painted_size: (u16, u16),
    pub painted_layout: Option<FrameLayout>,
    pub layout: LayoutConfig,
    pub selection: Option<selection::Selection>,
    pub last_click: Option<LastClick>,
    pub clipboard_notice: Option<String>,
    pub stream: String,
    pub thinking_preview: String,
    pub subagents: Vec<SubagentView>,
    pub debug: DebugLogger,
    pub exit_requested: bool,
    pub last_ctrl_c: Option<Instant>,
}

impl State {
    pub(super) fn new(mut options: PromptOptions) -> Self {
        let files = std::mem::take(&mut options.files);
        let mut editor = Editor::default();
        editor.set(options.words.join(" "));
        Self {
            options,
            preferences: None,
            theme: theme::THEMES[0],
            theme_preview_origin: None,
            catalog: Value::Null,
            files,
            editor,
            parent: None,
            active: None,
            phase: "ready".into(),
            transcript: Transcript::default(),
            status: json!({}),
            last_status_at: None,
            status_disconnected: false,
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
            scroll: 0,
            last_response: String::new(),
            copy_response: String::new(),
            painted_transcript: Vec::new(),
            painted_size: (0, 0),
            painted_layout: None,
            layout: LayoutConfig::default(),
            selection: None,
            last_click: None,
            clipboard_notice: None,
            stream: String::new(),
            thinking_preview: String::new(),
            subagents: Vec::new(),
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

    #[cfg(test)]
    pub(super) fn logs(&self) -> String {
        self.transcript.plain_text()
    }

    pub(super) fn push_entry(&mut self, plain: &str, entry: Entry) {
        let max_bytes = if self.debug.level >= 2 {
            1024 * 1024
        } else {
            128 * 1024
        };
        self.transcript.push(plain, entry, max_bytes);
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
        let version = env!("CARGO_PKG_VERSION");
        let commit = crate::cli::commit_head();
        let version_label = match &commit {
            Some(head) => format!("anticrumb {version} ({head})"),
            None => format!("anticrumb {version}"),
        };
        let account_label = format!("{} · {}", self.provider_name(), self.options.account);
        let model_label = format!("{} ({})", self.model_label(), self.thinking_label());
        let workspace_label = Self::format_workspace_home(&self.options.workspace);

        let mut row0 = vec![
            ("    ".into(), Face::TEXT),
            ("▄██████▄".into(), Face::HEADING),
            ("      ".into(), Face::TEXT),
            (format!("anticrumb {version}"), Face::BOLD),
        ];
        if let Some(head) = commit {
            row0.push((format!(" ({head})"), Face::MUTED));
        }

        let row1 = vec![
            ("   ".into(), Face::TEXT),
            ("██".into(), Face::HEADING),
            ("▀████▀".into(), Face::CODE),
            ("█▌".into(), Face::HEADING),
            ("     ".into(), Face::TEXT),
            (account_label.clone(), Face::MUTED),
        ];

        let row2 = vec![
            ("   ".into(), Face::TEXT),
            ("██".into(), Face::HEADING),
            ("▄████".into(), Face::CODE),
            ("  ".into(), Face::TEXT),
            ("▀▄".into(), Face::ACCENT_BOLD),
            ("    ".into(), Face::TEXT),
            (model_label.clone(), Face::TEXT),
        ];

        let row3 = vec![
            ("    ".into(), Face::TEXT),
            ("▀████▀".into(), Face::HEADING),
            ("  ".into(), Face::TEXT),
            ("▪".into(), Face::CODE),
            (" ".into(), Face::TEXT),
            ("·".into(), Face::HIGHLIGHT_BOLD),
            ("   ".into(), Face::TEXT),
            (workspace_label.clone(), Face::MUTED),
        ];

        let row4 = vec![
            ("      ".into(), Face::TEXT),
            ("·".into(), Face::MUTED),
            ("  ".into(), Face::TEXT),
            ("▪".into(), Face::ACCENT_BOLD),
            ("   ".into(), Face::TEXT),
            ("▪".into(), Face::CODE),
        ];

        self.log_spans("", Vec::new());
        self.log_spans(&version_label, row0);
        self.log_spans(&account_label, row1);
        self.log_spans(&model_label, row2);
        self.log_spans(&workspace_label, row3);
        self.log_spans("", row4);
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

    pub(super) fn log_tool(&mut self, tool: &str, detail: &str) {
        let plain = format!("  • {tool} {detail}");
        self.push_entry(
            &plain,
            Entry::Tool {
                tool: tool.into(),
                detail: detail.into(),
            },
        );
    }

    pub(super) fn write_debug_log(&mut self, tag: &str, body: &str) {
        if let Some(err) = self.debug.write(
            self.started,
            &self.options.workspace,
            &self.options.account,
            tag,
            body,
        ) {
            self.push_entry(&err, Entry::Error(err.clone()));
        }
    }

    pub(super) fn log_debug(&mut self, tag: &str, detail: &str) {
        self.write_debug_log(tag, detail);
        let plain = format!("  ⚙ [{tag}] {detail}");
        self.log_spans(
            &plain,
            vec![
                ("  ⚙ ".into(), Face::ACCENT),
                (format!("[{tag}] "), Face::ACCENT),
                (detail.to_owned(), Face::MUTED),
            ],
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
        self.log_spans(&plain, parts);
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

        self.parent = None;
        self.active_model = None;
        self.files.clear();
        self.in_chars = 0;
        self.out_chars = 0;
        self.last_response.clear();
        self.copy_response.clear();
        self.stream.clear();
        self.thinking_preview.clear();
        self.subagents.clear();
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

    pub(super) fn daemon_summary(&self) -> String {
        let connection = &self.status["browser_connection"];
        let location = match connection["transport"].as_str() {
            Some("grpc") => format!(
                "remote {}",
                connection["endpoint"]
                    .as_str()
                    .unwrap_or("unknown endpoint")
            ),
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
            self.phase = "ready".into();
        }
    }

    pub(super) fn load_history(&mut self) {
        if let Some(data) = &self.preferences {
            match data.history(&self.options.account, &self.options.workspace) {
                Ok(items) => self.history = items,
                Err(e) => self.log(&format!("History: {e}")),
            }
        }
        self.history_index = self.history.len();
        self.saved_draft.clear();
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
    }

    pub(super) fn paste(&mut self, text: &str) {
        if !self.editor.paste(text) {
            self.log("Paste exceeds the 48,000-byte input limit; your draft is unchanged. Save the text to a file and attach it with @path.");
        }
        self.completion = None;
    }

    pub(super) fn load(&mut self, job: Value) {
        let Some(id) = job["id"].as_str() else {
            return;
        };
        if let Err(e) = self.options.inherit_from_job(&job) {
            self.error(&e.to_string());
            return;
        }
        self.parent = Some(id.into());
        self.active_model = None;
        self.in_chars = 0;
        self.out_chars = 0;
        self.last_response.clear();
        self.copy_response.clear();
        self.stream.clear();
        self.thinking_preview.clear();
        self.subagents.clear();
        let state_str = job["state"].as_str().unwrap_or("saved");
        let state_face = match state_str {
            "completed" | "ready" => Face::HIGHLIGHT_BOLD,
            "failed" | "cancelled" | "outcome_unknown" | "awaiting_human" => Face::DANGER_BOLD,
            _ => Face::ACCENT,
        };
        self.log_spans(
            &format!("\nResumed {id} · {state_str}"),
            vec![
                ("\n".into(), Face::MUTED),
                ("· ".into(), Face::ACCENT),
                (format!("Resumed {id}"), Face::ACCENT_BOLD),
                (" · ".into(), Face::MUTED),
                (state_str.into(), state_face),
            ],
        );
        self.push_entry(
            &format!("› {}", job["request"]["task"].as_str().unwrap_or("")),
            Entry::Prompt(job["request"]["task"].as_str().unwrap_or("").into()),
        );
        self.load_history();
        if job["state"] == "running" {
            self.active = Some(id.into());
            self.phase = "running".into();
            self.started = Instant::now();
        } else {
            self.phase = "ready".into();
            self.active = None;
            if let Some(answer) = job["result"]["final_response"].as_str() {
                self.log_markdown(answer);
                self.copy_response = answer.into();
            }
            if !job["error"].is_null() {
                self.log_error(&format!("! {}", job["error"]));
            }
            self.log("Send a follow-up to continue. Previous actions will not be replayed automatically.");
        }
    }

    fn apply_status(&mut self, status: Value) {
        self.last_status_at = Some(Instant::now());
        self.status_disconnected = false;
        let was_blocked = self.human_required();
        let previous_connection = self.daemon_summary();
        self.status = status;
        self.notice_chat_cleanup();
        let connection = self.daemon_summary();
        if connection != previous_connection {
            self.log_daemon_rows(&connection);
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
        if let Some(account) = self
            .account()
            .filter(|a| a["models"].as_array().is_some_and(|m| !m.is_empty()))
        {
            self.catalog = account.clone();
        }
    }

    pub(super) fn update(&mut self, update: Update) {
        match update {
            Update::Status(status) => self.apply_status(status),
            Update::StatusReport(status) => {
                match status {
                    Some(status) => self.apply_status(status),
                    None => self.update(Update::StatusDisconnected),
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
            Update::Started(id) => {
                if self.debug.level >= 1 {
                    self.log_debug("agent", &format!("started job {id}"));
                }
                self.active = Some(id);
                self.phase = "running".into();
                self.in_chars = 0;
                self.out_chars = 0;
                self.active_model = None;
                self.last_response.clear();
                self.stream.clear();
                self.thinking_preview.clear();
                self.subagents.clear();
                self.started = Instant::now();
            }
            Update::Error(error) => self.error(&error),
            Update::Loaded(job) => self.load(job),
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
            Update::Conversations(jobs) => {
                self.resume_notice = None;
                self.phase = "ready".into();
                let choices: Vec<_> = jobs
                    .into_iter()
                    .take(30)
                    .filter_map(|j| {
                        Some(Choice {
                            label: format!(
                                "{} · {}",
                                j["state"].as_str().unwrap_or("saved"),
                                clip(j["request"]["task"].as_str().unwrap_or(""), 80)
                            ),
                            value: Selection::Resume(j["id"].as_str()?.into()),
                        })
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
                if self.parent.as_deref() == Some(&id) {
                    self.parent = None;
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
        }
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
        self.parent = job["id"].as_str().map(str::to_owned);
        self.active = None;
        self.active_model = None;
        self.stream.clear();
        self.thinking_preview.clear();
        self.subagents.clear();
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
            "browser_turn" => {
                if self.debug.level >= 2 {
                    let run_id = event.data["run_id"].as_str().unwrap_or("");
                    self.log_debug("browser", &format!("turn submitted run_id={run_id}"));
                }
            }
            "browser_session" => {
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
            "tool_started" => {
                let tool = event.data["tool"].as_str().unwrap_or("tool");
                self.phase = if self.debug.level >= 1 {
                    format!("tool:{tool}")
                } else {
                    "working".into()
                };
                let args = &event.data["args"];
                let detail = args["path"]
                    .as_str()
                    .or(args["command"].as_str())
                    .or(args["url"].as_str())
                    .or(args["query"].as_str())
                    .or(args["label"].as_str())
                    .or(args["task"].as_str())
                    .unwrap_or("");
                self.log_tool(tool, &clip(detail, 100));
                if self.debug.level >= 2 {
                    let pretty_args =
                        serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
                    self.log_debug_block("tool_args", tool, &pretty_args);
                }
            }
            "tool_finished" => {
                let ok = event.data["ok"] != false;
                let elapsed = event.data["elapsed_ms"].as_u64().unwrap_or(0);
                let tool = event.data["tool"].as_str().unwrap_or("tool");
                let out = event.data["output"].as_str().unwrap_or("");
                if !ok {
                    self.log_error(&format!(
                        "    ! {}",
                        clip(if out.is_empty() { "tool failed" } else { out }, 240)
                    ));
                }
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
                if self.debug.level >= 1 && !id.is_empty() {
                    let label = event.data["label"].as_str().unwrap_or("worker");
                    let ok = event.data["ok"] != false;
                    let elapsed = event.data["elapsed_ms"].as_u64().unwrap_or(0);
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

    pub(super) fn transcript_lines(&self, width: usize) -> Vec<markdown::Line> {
        self.transcript.render_lines(&self.stream, width)
    }

    pub(super) fn scroll_transcript(
        &mut self,
        up: bool,
        lines: usize,
        width: usize,
        height: usize,
    ) {
        let viewport = self
            .compute_layout(width as u16, height as u16)
            .map_or(1, |(layout, _)| usize::from(layout.transcript.len).max(1));
        let maximum = self.transcript_lines(width).len().saturating_sub(viewport);
        let current = self.scroll.min(maximum);
        self.scroll = if up {
            current.saturating_add(lines)
        } else {
            current.saturating_sub(lines)
        }
        .min(maximum);
    }

    pub(super) fn scroll_wheel(&mut self, up: bool, width: usize, height: usize) {
        self.selection = None;
        self.last_click = None;
        self.clipboard_notice = None;
        if self.deleting_conversation.is_some() || self.history_clear_confirmation.is_some() {
            return;
        }
        if self.menu.is_some() {
            self.move_menu(up);
        } else {
            self.scroll_transcript(up, 3, width, height);
        }
    }

    pub(super) fn transcript_mouse(&mut self, mouse: event::MouseEvent) -> Option<String> {
        if self.menu.is_some() || self.history_clear_confirmation.is_some() {
            self.selection = None;
            self.last_click = None;
            return None;
        }
        let fallback_start = u16::from(self.layout.show_header);
        let transcript_start = self
            .painted_layout
            .map_or(fallback_start, |layout| layout.transcript.start);
        let within_transcript = self.painted_layout.map_or_else(
            || mouse.row >= fallback_start,
            |layout| layout.transcript.contains(mouse.row),
        );
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.clipboard_notice = None;
                if within_transcript && mouse.column < self.painted_size.0 {
                    const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(600);
                    let is_double = self.last_click.is_some_and(|prev| {
                        prev.at.elapsed() <= DOUBLE_CLICK_WINDOW
                            && prev.row == mouse.row
                            && prev.column.abs_diff(mouse.column) <= 2
                    });
                    let row = usize::from(mouse.row - transcript_start);
                    let column = usize::from(mouse.column);
                    if is_double {
                        self.last_click = None;
                        self.selection = selection::Selection::new_block(
                            self.painted_transcript.clone(),
                            column,
                            row,
                        );
                    } else {
                        self.last_click = Some(LastClick {
                            at: Instant::now(),
                            column: mouse.column,
                            row: mouse.row,
                        });
                        self.selection =
                            selection::Selection::new(self.painted_transcript.clone(), column, row);
                    }
                } else {
                    self.last_click = None;
                    self.selection = None;
                };
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left))
                    && self.last_click.is_some_and(|prev| {
                        prev.row != mouse.row || prev.column.abs_diff(mouse.column) > 1
                    })
                {
                    self.last_click = None;
                }
                if let Some(selection) = self.selection.as_mut() {
                    let (column, row) = if mouse.row < transcript_start {
                        (0, 0)
                    } else if usize::from(mouse.row - transcript_start) >= selection.lines.len() {
                        (
                            usize::from(self.painted_size.0),
                            selection.lines.len().saturating_sub(1),
                        )
                    } else {
                        (
                            usize::from(mouse.column.min(self.painted_size.0.saturating_sub(1))),
                            usize::from(mouse.row - transcript_start),
                        )
                    };
                    selection.extend(column, row);
                }
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
                    let copied = self.selection.take().and_then(|selection| selection.text());
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
            }
            _ => {}
        }
        None
    }
}
