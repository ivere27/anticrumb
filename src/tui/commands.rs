//! Slash commands, completion, interactive menus, and submission handling.
use super::{
    debug_log::DebugLogger,
    editor::Editor,
    markdown::Face,
    model_picker::{self, ModelChoice},
    reducer::{EffectSink, State},
    theme,
    transcript::clip,
    worker::{Command, Update},
};
use crossterm::event::{self, KeyCode, KeyModifiers};
use serde_json::Value;
use std::time::Instant;

pub(super) const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Keyboard shortcuts and commands"),
    (
        "/mode",
        "Switch between Chat mode (default) and Agent mode (--yolo)",
    ),
    (
        "/copy",
        "Copy the last assistant response, including Markdown",
    ),
    (
        "/provider",
        "Choose the provider / account for the next turn",
    ),
    ("/model", "Choose the model for the next turn"),
    ("/usage", "Check web account usage limits and reset times"),
    (
        "/cache",
        "Storage usage and cleanup; /cache prune [DAYS] [--dry-run] (default: 7)",
    ),
    (
        "/status",
        "Runtime health, clients, jobs, browser connection, model and workspace",
    ),
    ("/theme", "Choose a terminal color palette"),
    (
        "/browse",
        "Control web pages: open, snapshot, click, fill, press, tabs, close; /browse help",
    ),
    (
        "/browser",
        "Browser status, show/hide window, login, and clear-history",
    ),
    (
        "/resume",
        "Reopen saved conversations; clear-history deletes all account web chats",
    ),
    ("/new", "Start a fresh conversation"),
    ("/clear", "Clear the screen and start fresh"),
    (
        "/permissions",
        "Configure tool permissions (read, edit, shell, browser, subagent)",
    ),
    (
        "/pace",
        "Set anti-bot humanization pace (/pace fast|low|high)",
    ),
    (
        "/prompts",
        "Show or initialize editable prompt/tool description files and AGENTS.md",
    ),
    (
        "/debug",
        "Set debug verbosity level (/debug 0|1|2 or /debug off|info|trace)",
    ),
    ("/exit", "Exit and stop this application's running work"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PermissionKind {
    Read,
    Edit,
    Shell,
    Browser,
    Subagent,
}

pub(super) enum Selection {
    Provider {
        account_id: String,
        provider_id: String,
        provider_name: String,
    },
    SetupProvider {
        provider_id: String,
        provider_name: String,
    },
    Model(ModelChoice),
    Theme(theme::Theme),
    Resume(String),
    DeleteConversation(String),
    CancelDelete,
    Mode {
        agent: bool,
        yolo: bool,
    },
    Permission {
        kind: PermissionKind,
        perms: crate::domain::ToolPermissions,
    },
    #[allow(dead_code)]
    Shell(bool),
    Pace(crate::gemini::HumanPace),
    Debug(u8),
}

pub(super) struct Choice {
    pub label: String,
    pub value: Selection,
}

pub(super) struct Menu {
    pub title: String,
    pub choices: Vec<Choice>,
    pub selected: usize,
}

pub(super) struct HistoryClearConfirmation {
    pub account: String,
    pub provider: String,
    pub typed: String,
}

pub(super) struct Completion {
    pub start: usize,
    pub end: usize,
    pub values: Vec<String>,
}

pub(super) fn complete(editor: &Editor, workspace: &std::path::Path) -> Option<Completion> {
    if editor.is_literal_before_cursor() {
        return None;
    }
    let prefix = &editor.text[..editor.cursor];
    if prefix.starts_with('/') && !prefix.contains(char::is_whitespace) {
        return Some(Completion {
            start: 0,
            end: editor.cursor,
            values: COMMANDS
                .iter()
                .filter(|(s, _)| s.starts_with(prefix))
                .map(|(s, _)| format!("{s} "))
                .collect(),
        });
    }
    let start = prefix
        .char_indices()
        .rev()
        .find(|(i, c)| {
            *c == '@' && crate::input::is_mention_boundary(prefix[..*i].chars().next_back())
        })?
        .0;
    let raw = &prefix[start + 1..];
    let quoted = raw.starts_with('"') || raw.starts_with('\'');
    if !quoted && raw.contains(char::is_whitespace) {
        return None;
    }
    let raw = raw.trim_start_matches(['"', '\'']);
    if raw.contains(['"', '\'', '\n']) {
        return None;
    }
    if let Some(git_prefix) = raw.strip_prefix("git:") {
        let mut values = Vec::new();
        let format_item = |item: &str| {
            if quoted {
                format!("@\"{item}\" ")
            } else {
                format!("@{item} ")
            }
        };
        let git_prefix_lower = git_prefix.to_ascii_lowercase();
        for special in ["diff", "staged"] {
            if special.starts_with(&git_prefix_lower) {
                values.push(format_item(&format!("git:{special}")));
            }
        }
        let commit_limit = if git_prefix.is_empty() {
            20usize.saturating_sub(values.len())
        } else {
            20
        };
        let is_hex = !git_prefix_lower.is_empty()
            && git_prefix_lower.chars().all(|c| c.is_ascii_hexdigit());
        let mut matched_recent_sha = false;
        let mut sha_values = Vec::new();
        let mut subject_matches: Vec<(i64, usize, String)> = Vec::new();
        for (idx, (sha, subject)) in
            super::mentions::recent_git_commits(workspace, commit_limit)
                .into_iter()
                .enumerate()
        {
            if git_prefix.is_empty()
                || (is_hex && sha.to_ascii_lowercase().starts_with(&git_prefix_lower))
            {
                matched_recent_sha = true;
                sha_values.push(format_item(&format!("git:{sha}")));
            } else if !git_prefix.is_empty() && !subject.is_empty() {
                if let Some(subj_score) = super::mentions::fuzzy_score(git_prefix, &subject) {
                    subject_matches.push((
                        1_200 + subj_score.clamp(-400, 500) - idx as i64,
                        idx,
                        format_item(&format!("git:{sha}")),
                    ));
                }
            }
        }
        if is_hex && !matched_recent_sha {
            if let Some((old_sha, _)) =
                super::mentions::resolve_git_commit_entry(workspace, git_prefix)
            {
                let item = format_item(&format!("git:{old_sha}"));
                if !sha_values.contains(&item) && !subject_matches.iter().any(|m| m.2 == item) {
                    sha_values.push(item);
                }
            }
        }
        values.extend(sha_values);
        subject_matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        values.extend(subject_matches.into_iter().map(|(_, _, val)| val));
        values.truncate(20);
        return Some(Completion {
            start,
            end: editor.cursor,
            values,
        });
    }
    let (directory, name) = raw
        .rsplit_once('/')
        .map(|(d, n)| (format!("{d}/"), n))
        .unwrap_or((String::new(), raw));
    let expanded = if directory.starts_with("~/") {
        crate::input::expand_home_path(&directory)?
    } else {
        workspace.join(&directory)
    };
    let mut values: Vec<_> = std::fs::read_dir(expanded)
        .ok()?
        .take(2000)
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let filename = entry.file_name().to_string_lossy().into_owned();
            if !filename.starts_with(name) || (!name.starts_with('.') && filename.starts_with('.'))
            {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            let path = format!("{directory}{filename}{}", if is_dir { "/" } else { "" });
            if path.contains(['"', '\n', '\r']) {
                return None;
            }
            Some(if path.contains(char::is_whitespace) {
                format!("@\"{path}{}", if is_dir { "" } else { "\" " })
            } else {
                format!("@{path}{}", if is_dir { "" } else { " " })
            })
        })
        .collect();
    if directory.is_empty() {
        for special in ["git:diff", "git:staged"] {
            if special.starts_with(name) {
                values.push(if quoted {
                    format!("@\"{special}\" ")
                } else {
                    format!("@{special} ")
                });
            }
        }
    }
    values.sort();
    values.truncate(20);
    Some(Completion {
        start,
        end: editor.cursor,
        values,
    })
}

impl State {
    pub(super) fn open_history_clear(&mut self) {
        if self.busy() {
            self.log("Finish the current task before deleting web chat history.");
            return;
        }
        self.history_clear_confirmation = Some(HistoryClearConfirmation {
            account: self.options.account.clone(),
            provider: self.provider_name().trim_end_matches(" Web").to_owned(),
            typed: String::new(),
        });
        self.completion = None;
        self.clear_mention();
    }

    pub(super) fn history_clear_key(&mut self, key: event::KeyEvent, sink: &mut dyn EffectSink) {
        if key.kind != event::KeyEventKind::Press {
            return;
        }
        let Some(confirmation) = self.history_clear_confirmation.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.history_clear_confirmation = None,
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.history_clear_confirmation = None;
            }
            KeyCode::Backspace => {
                confirmation.typed.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control()
                    && confirmation.typed.len() < 32 =>
            {
                confirmation.typed.push(ch);
            }
            KeyCode::Enter if confirmation.typed.is_empty() => {
                self.history_clear_confirmation = None;
            }
            KeyCode::Enter if confirmation.typed == "DELETE" && key.modifiers.is_empty() => {
                let account = confirmation.account.clone();
                self.history_clear_confirmation = None;
                if self.busy() {
                    self.log("Finish the current task before deleting web chat history.");
                    return;
                }
                self.menu = None;
                self.resume_menu = None;
                self.resume_notice = None;
                self.clearing_web_history = true;
                self.phase = "loading".into();
                if let Err(error) = sink.send_command(Command::ClearWebHistory(account)) {
                    self.update(Update::WebHistoryError(
                        format!("could not queue history cleanup: {error}"),
                        true,
                    ));
                }
            }
            _ => {}
        }
    }

    pub(super) fn open_models(&mut self) {
        let mut choices = Vec::new();
        let mut selected = 0;
        if let Some(models) = self.catalog["models"].as_array() {
            for model in models.iter().filter(|m| m["locked"] != true) {
                let Some(id) = model["mode_id"].as_str() else {
                    continue;
                };
                if self.options.model.as_deref() == Some(id)
                    || (self.options.model.is_none() && model["selected"] == true)
                {
                    selected = choices.len();
                }
                let levels = self.catalog["model_thinking"]
                    .get(id)
                    .unwrap_or(&Value::Null);
                choices.push(Choice {
                    label: model_picker::title(model),
                    value: Selection::Model(ModelChoice::new(
                        Some(id.into()),
                        levels,
                        self.options.thinking.as_deref(),
                    )),
                });
            }
        }
        choices.push(Choice {
            label: "Web default".into(),
            value: Selection::Model(ModelChoice::new(
                None,
                &self.catalog["thinking"],
                self.options.thinking.as_deref(),
            )),
        });
        self.menu = Some(Menu {
            title: "Model · ↑↓ model · ←→ thinking · Enter apply · Esc cancel".into(),
            choices,
            selected,
        });
    }

    pub(super) fn open_providers(&mut self) {
        let mut choices = Vec::new();
        let mut selected = 0;
        let mut configured_providers = std::collections::HashSet::new();
        if let Some(accounts) = self.status["accounts"].as_array() {
            for acct in accounts {
                let Some(account_id) = acct["id"].as_str() else {
                    continue;
                };
                let provider_id = acct["provider"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(crate::provider::DEFAULT_PROVIDER_ID);
                configured_providers.insert(provider_id.to_owned());
                let provider_name = self.provider_display_name(provider_id).to_owned();
                let state_str = if self.is_provider_registered(provider_id) {
                    acct["state"].as_str().unwrap_or("unknown")
                } else {
                    "unregistered"
                };
                if account_id == self.options.account {
                    selected = choices.len();
                }
                choices.push(Choice {
                    label: format!(
                        "{provider_name} ({provider_id}) · account {account_id} · {state_str}"
                    ),
                    value: Selection::Provider {
                        account_id: account_id.to_owned(),
                        provider_id: provider_id.to_owned(),
                        provider_name,
                    },
                });
            }
        }
        if let Some(providers) = self.status["providers"].as_array() {
            for prov in providers {
                let Some(provider_id) = prov["id"].as_str() else {
                    continue;
                };
                if configured_providers.contains(provider_id) {
                    continue;
                }
                let provider_name = prov["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(provider_id)
                    .to_owned();
                choices.push(Choice {
                    label: format!("{provider_name} ({provider_id}) · setup & sign in"),
                    value: Selection::SetupProvider {
                        provider_id: provider_id.to_owned(),
                        provider_name,
                    },
                });
            }
        }
        if choices.is_empty() {
            self.log("No providers found. Run `anticrumb providers add <DIR_OR_FILE>`.");
            return;
        }
        self.menu = Some(Menu {
            title: "Provider & account".into(),
            choices,
            selected,
        });
    }

    pub(super) fn apply_provider_arg(&mut self, arg: &str, sink: Option<&mut dyn EffectSink>) {
        let accounts = self.status["accounts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let chosen = accounts
            .iter()
            .find(|a| a["id"].as_str() == Some(arg))
            .or_else(|| {
                let mut matching = accounts
                    .iter()
                    .filter(|a| a["provider"].as_str() == Some(arg));
                matching
                    .clone()
                    .find(|a| a["state"].as_str() == Some("ready"))
                    .or_else(|| matching.next())
            });
        if let Some(account_id) = chosen.and_then(|acct| {
            acct["id"].as_str().map(|id| {
                let provider_id = acct["provider"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(crate::provider::DEFAULT_PROVIDER_ID)
                    .to_owned();
                (id.to_owned(), provider_id)
            })
        }) {
            let (account_id, provider_id) = account_id;
            let provider_name = self.provider_display_name(&provider_id).to_owned();
            self.switch_provider(&account_id, &provider_id, &provider_name);
            return;
        }
        if let Some((provider_id, provider_name)) =
            self.status["providers"].as_array().and_then(|ps| {
                ps.iter()
                    .find(|p| {
                        p["id"].as_str() == Some(arg)
                            || p["name"]
                                .as_str()
                                .is_some_and(|n| n.eq_ignore_ascii_case(arg))
                    })
                    .and_then(|p| {
                        let id = p["id"].as_str()?.to_owned();
                        let name = p["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&id)
                            .to_owned();
                        Some((id, name))
                    })
            })
        {
            if let Some(sink) = sink {
                self.begin_provider_setup(&provider_id, &provider_name, sink);
            } else {
                self.menu = Some(Menu {
                    title: "Setup provider".into(),
                    choices: vec![Choice {
                        label: format!("{provider_name} ({provider_id}) · setup & sign in"),
                        value: Selection::SetupProvider {
                            provider_id,
                            provider_name,
                        },
                    }],
                    selected: 0,
                });
            }
        } else {
            self.log(&format!(
                "Unknown provider or account '{arg}'. Run /provider to choose or set up a provider."
            ));
        }
    }

    pub(super) fn begin_provider_setup(
        &mut self,
        provider_id: &str,
        provider_name: &str,
        sink: &mut dyn EffectSink,
    ) {
        if self.busy() {
            self.log("Interrupt the current task before switching provider or account.");
            return;
        }
        self.phase = "loading".into();
        self.log(&format!(
            "Creating account for {provider_name} ({provider_id}) and opening Chrome sign-in…"
        ));
        self.send(
            sink,
            Command::SetupProvider {
                provider_id: provider_id.to_owned(),
                provider_name: provider_name.to_owned(),
            },
        );
    }

    pub(super) fn choose_theme(&mut self, theme: theme::Theme) {
        self.theme = theme;
        if let Some(data) = &self.preferences {
            if let Err(e) = data.set_theme(theme.id) {
                self.log_error(&format!("Theme could not be saved: {e}"));
            }
        }
        let plain = format!("Theme: {}", theme.name);
        self.log_spans(
            &plain,
            vec![
                ("· ".into(), Face::ACCENT),
                (plain.clone(), Face::HIGHLIGHT_BOLD),
            ],
        );
    }

    pub(super) fn save_model_preference(&mut self) {
        if let Some(data) = &self.preferences {
            if let Err(e) = data.set_model_selection(
                &self.options.account,
                self.options.model.as_deref(),
                self.options.thinking.as_deref(),
            ) {
                self.log(&format!("Model selection could not be saved: {e}"));
            }
        }
    }

    pub(super) fn load_model_preference(&mut self) {
        let Some(data) = &self.preferences else {
            return;
        };
        let pref = data.model_selection(&self.options.account);
        let model_overridden = self.options.overrides.contains("model");
        if !model_overridden {
            self.options.model = pref.model.clone();
        }
        if !self.options.overrides.contains("thinking")
            && (!model_overridden || self.options.model == pref.model)
        {
            self.options.thinking = pref.thinking;
        }
        if self.options.yolo {
            self.options.permissions = crate::domain::ToolPermissions::all();
            self.options.no_shell = false;
        } else if !self.options.overrides.contains("permissions") {
            if let Some(mut perms) = data.permissions() {
                if self.options.overrides.contains("no_shell") {
                    perms.shell = !self.options.no_shell;
                }
                self.options.no_shell = !perms.shell;
                self.options.permissions = perms;
            } else if !self.options.overrides.contains("no_shell") {
                if let Some(no_shell) = data.no_shell() {
                    self.options.no_shell = no_shell;
                    self.options.permissions.shell = !no_shell;
                }
            }
        }
    }

    fn log_help(&mut self) {
        self.log_heading("Commands");
        for (command, description) in COMMANDS {
            self.log_command(&format!("{command:<14}"), description);
        }
        self.log_heading("Input & Shortcuts");
        for (key, description) in [
            (
                "@path",
                "Attach file through Gemini's picker (@\"path with spaces\")",
            ),
            ("@git:diff", "Attach unstaged git diff as a patch file"),
            ("@git:staged", "Attach staged git diff as a patch file"),
            ("@git:<commit>", "Attach a specific git commit as a patch file"),
            ("Tab", "Complete slash commands or @path"),
            (
                "Up / Down",
                "Move within input, or recall history at top/bottom row",
            ),
            (
                "Wheel / PgUp",
                "Scroll transcript; drag transcript to select & copy",
            ),
            ("/copy", "Copy last assistant response with Markdown"),
            (
                "Ctrl+A/E/U/K/W",
                "Editing shortcuts · Left/Right/Home/End move cursor",
            ),
            (
                "Ctrl/Alt+←/→",
                "Move by word/path component · Alt+B/F also works · paste is one block",
            ),
            (
                "Alt/Shift+Enter",
                "Insert newline · Enter sends or steers active turn",
            ),
            (
                "Paste (>1,000c)",
                "Folds long paste into a block · Alt+E expands · Ctrl+V pastes",
            ),
            (
                "Esc / Ctrl+C",
                "Interrupt · Ctrl+D / /exit leaves and stops active jobs",
            ),
        ] {
            self.log_shortcut(&format!("{key:<15}"), description);
        }
        self.log("Character counts are measured; exact Gemini token usage is unavailable.");
    }

    fn log_status(&mut self, sink: &mut dyn EffectSink) {
        self.log_heading("Status");
        let model = self.model_label().to_owned();
        let thinking = self.thinking_label().to_owned();
        let theme_name = self.theme.name.to_owned();
        let workspace = self.options.workspace.display().to_string();
        let account = self.options.account.clone();
        let conversation = self
            .active
            .as_ref()
            .or(self.parent.as_ref())
            .map(String::as_str)
            .unwrap_or("new")
            .to_owned();
        let perms = self.options.effective_permissions();
        let store = crate::store::Store::open(&crate::cli::root()).ok();
        let pace = self
            .options
            .pace
            .unwrap_or_else(|| crate::gemini::load_human_pace(store.as_ref()));
        let mode_text = if !self.options.agent {
            "Chat (direct conversation, no tools)".to_owned()
        } else if self.options.yolo {
            "Agent (--yolo · all tools & sub-agents allowed)".to_owned()
        } else {
            format!("Agent (permissions: {})", perms.summary())
        };
        self.log_kv(
            "· ",
            Face::ACCENT,
            "Mode",
            &mode_text,
            if self.options.agent {
                Face::HIGHLIGHT_BOLD
            } else {
                Face::ACCENT
            },
        );
        self.log_kv("· ", Face::ACCENT, "Model", &model, Face::ACCENT_BOLD);
        self.log_kv("· ", Face::ACCENT, "Thinking", &thinking, Face::HIGHLIGHT);
        self.log_kv(
            "· ",
            Face::ACCENT,
            "Pace",
            pace.description(),
            Face::HIGHLIGHT,
        );
        self.log_kv("· ", Face::ACCENT, "Theme", &theme_name, Face::ACCENT);
        self.log_kv(
            "· ",
            Face::ACCENT,
            "Debug",
            &format!(
                "{} ({})",
                self.debug.level,
                DebugLogger::label(self.debug.level)
            ),
            if self.debug.level > 0 {
                Face::ACCENT_BOLD
            } else {
                Face::MUTED
            },
        );
        if let Some(path) = self.debug.path.clone() {
            self.log_kv(
                "· ",
                Face::ACCENT,
                "Debug log",
                &path.display().to_string(),
                Face::MUTED,
            );
        }
        self.log_kv("· ", Face::ACCENT, "Directory", &workspace, Face::TEXT);
        let provider_label = format!("{} ({})", self.provider_name(), self.provider_id());
        self.log_kv(
            "· ",
            Face::ACCENT,
            "Provider",
            &provider_label,
            Face::ACCENT,
        );
        self.log_kv("· ", Face::ACCENT, "Account", &account, Face::TEXT);
        // Keep "Conversation: <id>" contiguous for PTY expectations.
        self.log_spans(
            &format!("Conversation: {conversation}"),
            vec![
                ("· ".into(), Face::ACCENT),
                (format!("Conversation: {conversation}"), Face::HIGHLIGHT),
            ],
        );
        self.log_kv(
            "· ",
            Face::ACCENT,
            "Permissions",
            &perms.summary(),
            if perms.shell || perms.edit {
                Face::HIGHLIGHT_BOLD
            } else {
                Face::ACCENT
            },
        );
        let in_c = self.in_chars.to_string();
        let out_c = self.out_chars.to_string();
        self.log_spans(
            &format!(
                "This turn: {in_c} input / {out_c} output characters; exact tokens unavailable"
            ),
            vec![
                ("  This turn: ".into(), Face::MUTED),
                (in_c, Face::HIGHLIGHT),
                (" input / ".into(), Face::MUTED),
                (out_c, Face::HIGHLIGHT),
                (
                    " output characters; exact tokens unavailable".into(),
                    Face::MUTED,
                ),
            ],
        );
        let reused = self.status["usage"]["reused_turns"].to_string();
        let submitted = self.status["usage"]["submitted_chars"].to_string();
        self.log_spans(
            &format!("Service: {reused} reused turns, {submitted} submitted characters"),
            vec![
                ("  Service: ".into(), Face::MUTED),
                (reused, Face::HIGHLIGHT),
                (" reused turns, ".into(), Face::MUTED),
                (submitted, Face::HIGHLIGHT),
                (" submitted characters".into(), Face::MUTED),
            ],
        );
        self.log("Checking runtime health…");
        self.send(sink, Command::Status);
        self.send(
            sink,
            Command::WebHistoryStatus(self.options.account.clone(), true),
        );
    }

    pub(super) fn open_permissions(
        &mut self,
        perms: crate::domain::ToolPermissions,
        selected: usize,
    ) {
        let mark = |on: bool| if on { "[x]" } else { "[ ]" };
        let items = [
            (
                PermissionKind::Read,
                perms.read,
                "read      File search & read (find_files, grep_search, read_file)",
            ),
            (
                PermissionKind::Edit,
                perms.edit,
                "edit      File modify & create (edit_file, write_file)",
            ),
            (
                PermissionKind::Shell,
                perms.shell,
                "shell     Shell & processes (run_shell, start_process, …)",
            ),
            (
                PermissionKind::Browser,
                perms.browser,
                "browser   Web fetch & Chrome control (web_fetch, browser_control, …)",
            ),
            (
                PermissionKind::Subagent,
                perms.subagent,
                "subagent  Parallel worker sub-agents (spawn_agent)",
            ),
        ];
        let choices = items
            .into_iter()
            .map(|(kind, on, desc)| Choice {
                label: format!("{} {desc}", mark(on)),
                value: Selection::Permission { kind, perms },
            })
            .collect();
        self.menu = Some(Menu {
            title: "Permissions · ↑↓ move · Space/←→ toggle · Enter save · Esc cancel".into(),
            choices,
            selected: selected.min(4),
        });
    }

    pub(super) fn toggle_permission_menu_item(&mut self) -> bool {
        let Some(menu) = self.menu.as_ref() else {
            return false;
        };
        let selected = menu.selected;
        let Some(Choice {
            value: Selection::Permission { kind, mut perms },
            ..
        }) = menu.choices.get(selected)
        else {
            return false;
        };
        match kind {
            PermissionKind::Read => perms.read = !perms.read,
            PermissionKind::Edit => perms.edit = !perms.edit,
            PermissionKind::Shell => perms.shell = !perms.shell,
            PermissionKind::Browser => perms.browser = !perms.browser,
            PermissionKind::Subagent => perms.subagent = !perms.subagent,
        }
        self.open_permissions(perms, selected);
        true
    }

    pub(super) fn apply_permissions(&mut self, perms: crate::domain::ToolPermissions) {
        self.options.overrides.insert("permissions".into());
        self.options.overrides.insert("no_shell".into());
        self.options.permissions = perms;
        self.options.no_shell = !perms.shell;
        if perms != crate::domain::ToolPermissions::all() {
            self.options.yolo = false;
        }
        if let Some(data) = &self.preferences {
            if let Err(e) = data.set_permissions(perms) {
                self.log_error(&format!("Permission setting could not be saved: {e}"));
            }
        }
        self.log(&format!("Tool permissions saved: {}.", perms.summary()));
    }

    pub(super) fn set_agent_mode(&mut self, agent: bool, yolo: bool) {
        self.options.overrides.insert("agent".into());
        self.options.agent = agent;
        self.options.yolo = agent && yolo;
        if !agent {
            self.log("Mode set to Chat: direct conversation without tools for the next turn.");
        } else if yolo {
            self.options.overrides.insert("no_shell".into());
            self.options.no_shell = false;
            self.log(
                "Mode set to Agent (--yolo): all file, shell, browser, and sub-agent tools allowed for the next turn.",
            );
        } else {
            if let Some(data) = &self.preferences {
                if let Some(saved_perms) = data.permissions() {
                    self.options.permissions = saved_perms;
                    self.options.no_shell = !saved_perms.shell;
                } else if let Some(saved_no_shell) = data.no_shell() {
                    self.options.no_shell = saved_no_shell;
                    self.options.permissions.shell = !saved_no_shell;
                }
            }
            self.log(&format!(
                "Mode set to Agent (permissions: {}).",
                self.options.effective_permissions().summary()
            ));
        }
    }

    pub(super) fn submit(&mut self, sink: &mut dyn EffectSink) -> bool {
        let draft = self.editor.draft();
        let input = draft.text.trim();
        if input.is_empty() {
            return false;
        }
        let command_start = draft.text.len() - draft.text.trim_start().len();
        if input.starts_with('/') && !draft.is_block_at(command_start) {
            if !matches!(input, "/exit" | "/quit") {
                self.remember_draft(&draft);
            }
            let (command, arg) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
            let arg = arg.trim();
            self.editor.set(String::new());
            self.completion = None;
            self.clear_mention();
            match command {
                "/exit" | "/quit" => {
                    if self.pending.is_none() {
                        return true;
                    }
                    self.exit_requested = true;
                    self.log("Finishing submission before stopping…");
                }
                "/help" => self.log_help(),
                "/mode" => match arg {
                    "" => {
                        let current = if !self.options.agent {
                            "chat"
                        } else if self.options.yolo {
                            "agent (--yolo)"
                        } else {
                            "agent"
                        };
                        let selected = if !self.options.agent {
                            0
                        } else if self.options.yolo {
                            2
                        } else {
                            1
                        };
                        self.menu = Some(Menu {
                            title: format!("Mode · current: {current}"),
                            choices: vec![
                                Choice {
                                    label: "Chat · direct conversation (no tools, minimal tokens)"
                                        .into(),
                                    value: Selection::Mode {
                                        agent: false,
                                        yolo: false,
                                    },
                                },
                                Choice {
                                    label: "Agent · coding tools (respects /permissions)".into(),
                                    value: Selection::Mode {
                                        agent: true,
                                        yolo: false,
                                    },
                                },
                                Choice {
                                    label:
                                        "Agent (--yolo) · all file, shell & sub-agent tools allowed"
                                            .into(),
                                    value: Selection::Mode {
                                        agent: true,
                                        yolo: true,
                                    },
                                },
                            ],
                            selected,
                        });
                    }
                    "chat" | "normal" | "off" => self.set_agent_mode(false, false),
                    "agent" | "on" => self.set_agent_mode(true, false),
                    "yolo" => self.set_agent_mode(true, true),
                    _ => self.log("Usage: /mode [chat|agent|yolo]"),
                },
                "/copy" => {
                    if arg.is_empty() {
                        self.copy_last_response(sink);
                    } else {
                        self.log("Usage: /copy");
                    }
                }
                "/status" => self.log_status(sink),
                "/cache" => {
                    match crate::store::cache::Options::parse(
                        &arg.split_whitespace().collect::<Vec<_>>(),
                    ) {
                        Ok(options) => {
                            self.log(if options.prune && !options.dry_run {
                                "Cleaning eligible cache files…"
                            } else {
                                "Checking cache storage…"
                            });
                            self.send(sink, Command::Cache(options));
                        }
                        Err(error) => self.log_error(&format!("Usage: /{error}")),
                    }
                }
                "/usage" => {
                    if !arg.is_empty() {
                        self.log("Usage: /usage");
                    } else if self.clearing_web_history
                        || self.deleting_conversation.is_some()
                        || self.login_in_progress()
                    {
                        self.log("Finish sign-in or cleanup before checking web account usage.");
                    } else {
                        if self.active.is_none() && self.pending.is_none() {
                            self.phase = "loading".into();
                        }
                        self.log("Checking web account usage limits…");
                        self.send(sink, Command::WebUsage(self.options.account.clone()));
                    }
                }
                "/provider" => {
                    if self.busy() {
                        self.log(
                            "Interrupt the current task before switching provider or account.",
                        );
                    } else if self.status["accounts"].as_array().is_none_or(Vec::is_empty) {
                        self.phase = "loading".into();
                        let target = (!arg.is_empty()).then(|| arg.to_owned());
                        self.send(sink, Command::Providers(target));
                    } else if arg.is_empty() {
                        self.open_providers();
                    } else {
                        self.apply_provider_arg(arg, Some(sink));
                    }
                }
                "/model" => {
                    if arg.is_empty() || arg == "refresh" {
                        let refresh = arg == "refresh";
                        if refresh && self.busy() {
                            self.log("Finish the current task before refreshing model choices.");
                        } else {
                            if self.active.is_none() {
                                self.phase = "loading".into();
                            }
                            if refresh {
                                self.log(
                                    "Refreshing model names and thinking levels from the browser…",
                                );
                            }
                            self.send(sink, Command::Models(self.options.account.clone(), refresh));
                        }
                    } else {
                        self.options.overrides.insert("model".into());
                        self.options.overrides.insert("thinking".into());
                        self.options.thinking = None;
                        self.options.model = if arg == "default" {
                            None
                        } else {
                            Some(arg.into())
                        };
                        if self.active.is_none() {
                            self.active_model = None;
                        }
                        self.save_model_preference();
                        self.log("Model selection saved for the next turn.");
                    }
                }
                "/theme" => {
                    if arg.is_empty() {
                        let choices = theme::THEMES
                            .iter()
                            .map(|t| Choice {
                                label: t.name.into(),
                                value: Selection::Theme(*t),
                            })
                            .collect();
                        self.theme_preview_origin = Some(self.theme);
                        self.menu = Some(Menu {
                            title: "Theme".into(),
                            choices,
                            selected: theme::THEMES
                                .iter()
                                .position(|t| t.id == self.theme.id)
                                .unwrap_or(0),
                        });
                    } else if let Some(theme) = theme::find(arg) {
                        self.theme_preview_origin = None;
                        self.choose_theme(theme);
                    } else {
                        self.log("Unknown theme. Use /theme to select a palette.");
                    }
                }
                "/browse" => {
                    let parsed = crate::browser_control::terminal::parse(arg);
                    match parsed {
                        Ok(args) if args["action"] == "help" => {
                            self.log(crate::browser_control::HELP)
                        }
                        Ok(_) if self.busy() => self.log(
                            "Interrupt the current task before controlling its browser session.",
                        ),
                        Ok(args) => {
                            let session =
                                crate::browser_control::workspace_session(&self.options.workspace);
                            match crate::browser_control::request(
                                &args,
                                &self.options.account,
                                &session,
                            ) {
                                Ok(request) => self.send(sink, Command::BrowserControl(request)),
                                Err(error) => self.log(&error),
                            }
                        }
                        Err(error) => self.log(&error),
                    }
                }
                "/browser" => {
                    if arg == "clear-history" {
                        self.open_history_clear();
                    } else if !["", "status", "show", "hide", "refresh", "login"].contains(&arg) {
                        self.log("Usage: /browser [status|show|hide|refresh|login|clear-history]");
                    } else if ["show", "hide", "refresh", "login"].contains(&arg) && self.busy() {
                        self.log("Interrupt the current task before changing browser state.");
                    } else {
                        self.send(
                            sink,
                            Command::Browser(self.options.account.clone(), arg.into()),
                        );
                    }
                }
                "/resume" if !self.busy() => {
                    if arg == "clear-history" {
                        self.open_history_clear();
                        return false;
                    }
                    self.phase = "loading".into();
                    let command = if arg.is_empty() {
                        Command::List(self.options.workspace.clone(), self.options.account.clone())
                    } else {
                        Command::Load(arg.into(), self.options.clone())
                    };
                    self.send(sink, command);
                }
                "/new" | "/clear" if !self.busy() => {
                    self.parent = None;
                    self.files.clear();
                    self.in_chars = 0;
                    self.out_chars = 0;
                    self.active_model = None;
                    self.phase = "ready".into();
                    self.copy_response.clear();
                    self.subagents.clear();
                    if command == "/clear" {
                        self.transcript.clear();
                    }
                    self.log("New conversation. Saved conversations are available in /resume.");
                }
                "/permissions" => {
                    use crate::domain::ToolPermissions;
                    if arg.is_empty() {
                        let current = self.options.effective_permissions();
                        self.open_permissions(current, 0);
                    } else {
                        match arg {
                            "readonly" | "read-only" | "read" | "default" | "safe" => {
                                self.apply_permissions(ToolPermissions::read_only());
                            }
                            "edit" | "write" => {
                                self.apply_permissions(ToolPermissions {
                                    read: true,
                                    edit: true,
                                    ..ToolPermissions::read_only()
                                });
                            }
                            "all" | "full" => {
                                self.apply_permissions(ToolPermissions::all());
                            }
                            _ => {
                                let mut perms = self.options.effective_permissions();
                                let mut ok = true;
                                for token in arg.split_whitespace() {
                                    let (enable, name) = if let Some(n) = token.strip_prefix('+') {
                                        (true, n)
                                    } else if let Some(n) = token.strip_prefix('-') {
                                        (false, n)
                                    } else {
                                        (true, token)
                                    };
                                    match name {
                                        "read" => perms.read = enable,
                                        "edit" | "write" => perms.edit = enable,
                                        "shell" => perms.shell = enable,
                                        "browser" | "web" => perms.browser = enable,
                                        "subagent" | "subagents" => perms.subagent = enable,
                                        _ => {
                                            ok = false;
                                            break;
                                        }
                                    }
                                }
                                if ok {
                                    self.apply_permissions(perms);
                                } else {
                                    self.log("Usage: /permissions [readonly|edit|all | +read|-read +edit|-edit +shell|-shell +browser|-browser +subagent|-subagent]");
                                }
                            }
                        }
                    }
                }
                "/pace" => {
                    use crate::gemini::HumanPace;
                    let store = crate::store::Store::open(&crate::cli::root()).ok();
                    if arg.is_empty() {
                        let current = self
                            .options
                            .pace
                            .unwrap_or_else(|| crate::gemini::load_human_pace(store.as_ref()));
                        self.menu = Some(Menu {
                            title: format!("Humanization pace · current: {}", current.as_str()),
                            choices: vec![
                                Choice {
                                    label: HumanPace::Fast.description().into(),
                                    value: Selection::Pace(HumanPace::Fast),
                                },
                                Choice {
                                    label: HumanPace::Low.description().into(),
                                    value: Selection::Pace(HumanPace::Low),
                                },
                                Choice {
                                    label: HumanPace::High.description().into(),
                                    value: Selection::Pace(HumanPace::High),
                                },
                            ],
                            selected: match current {
                                HumanPace::Fast => 0,
                                HumanPace::Low => 1,
                                HumanPace::High => 2,
                            },
                        });
                    } else if let Some(pace) = HumanPace::parse(arg) {
                        self.options.pace = Some(pace);
                        crate::gemini::set_human_pace(store.as_ref(), pace);
                        self.send(sink, Command::Pace(pace));
                        self.log(&format!("Humanization pace set to {}", pace.description()));
                    } else {
                        self.log("Usage: /pace [fast|low|high]");
                    }
                }
                "/prompts" => match arg {
                    "" | "status" => {
                        let status = crate::agent::inspect_prompt_status(
                            None,
                            Some(&self.options.workspace),
                        );
                        self.log_heading("Agent Prompts & Instructions");
                        self.log_kv(
                            "· ",
                            Face::ACCENT,
                            "Prompts dir",
                            &status.prompts_dir.display().to_string(),
                            Face::TEXT,
                        );
                        self.log_kv(
                            "· ",
                            Face::ACCENT,
                            "system.md",
                            &format!(
                                "{} ({})",
                                status.system_file.display(),
                                if status.system_active {
                                    "active"
                                } else {
                                    "default"
                                }
                            ),
                            if status.system_active {
                                Face::HIGHLIGHT_BOLD
                            } else {
                                Face::MUTED
                            },
                        );
                        self.log_kv(
                            "· ",
                            Face::ACCENT,
                            "tools.toml",
                            &format!(
                                "{} ({} tool override(s))",
                                status.tools_toml_file.display(),
                                status.override_tool_count
                            ),
                            if status.override_tool_count > 0 {
                                Face::HIGHLIGHT_BOLD
                            } else {
                                Face::MUTED
                            },
                        );
                        self.log_kv(
                            "· ",
                            Face::ACCENT,
                            "subagent.md",
                            &format!(
                                "{} ({})",
                                status.subagent_file.display(),
                                if status.subagent_active {
                                    "active"
                                } else {
                                    "default"
                                }
                            ),
                            if status.subagent_active {
                                Face::HIGHLIGHT_BOLD
                            } else {
                                Face::MUTED
                            },
                        );
                        if status.instruction_files.is_empty() {
                            self.log("  No AGENTS.md instruction files currently loaded.");
                        } else {
                            for path in status.instruction_files {
                                self.log_kv(
                                    "· ",
                                    Face::ACCENT,
                                    "AGENTS.md",
                                    &path.display().to_string(),
                                    Face::HIGHLIGHT,
                                );
                            }
                        }
                        self.log("Use `/prompts init` to write default system.md, tools.toml, and subagent.md templates.");
                    }
                    "init" => {
                        let registry = crate::agent::ToolRegistry::with_builtin_coding_tools();
                        match crate::agent::init_prompt_files(None, &registry) {
                            Ok((dir, created)) => {
                                if created.is_empty() {
                                    self.log(&format!(
                                            "Prompt template files already exist in {}; nothing overwritten.",
                                            dir.display()
                                        ));
                                } else {
                                    self.log(&format!(
                                        "Created editable prompt files in {}: {}",
                                        dir.display(),
                                        created.join(", ")
                                    ));
                                }
                            }
                            Err(e) => {
                                self.log_error(&format!("Could not initialize prompt files: {e}"))
                            }
                        }
                    }
                    _ => self.log("Usage: /prompts [status|init]"),
                },
                "/debug" => {
                    if arg.is_empty() {
                        self.menu = Some(Menu {
                            title: format!(
                                "Debug level · current: {} ({})",
                                self.debug.level,
                                DebugLogger::label(self.debug.level)
                            ),
                            choices: vec![
                                Choice {
                                    label: "0 · off (normal compact output)".into(),
                                    value: Selection::Debug(0),
                                },
                                Choice {
                                    label: "1 · info (live phases, browser steps, tool timings)"
                                        .into(),
                                    value: Selection::Debug(1),
                                },
                                Choice {
                                    label: "2 · trace (full tool args, output snippets, IDs)"
                                        .into(),
                                    value: Selection::Debug(2),
                                },
                            ],
                            selected: usize::from(self.debug.level.min(2)),
                        });
                    } else if let Some(level) = DebugLogger::parse_level(arg) {
                        self.set_debug_level(level);
                    } else {
                        self.log("Usage: /debug [0|1|2|off|info|trace]");
                    }
                }
                "/resume" | "/new" | "/clear" => {
                    self.log("Interrupt with Esc before switching conversations.")
                }
                _ => self
                    .log_error("Unknown command. Type /help or press Tab to complete a command."),
            }
            return false;
        }
        if self.pending.is_some() || self.phase == "loading" {
            self.log("Waiting for the current submission. Your draft is kept.");
            return false;
        }
        if self.active.is_none() && self.human_required() {
            if self.login_in_progress() {
                self.log("Waiting for sign-in and model setup. Your draft is kept.");
            } else {
                let auth_required = self
                    .account()
                    .is_some_and(|account| account["state"] == "auth_required");
                self.log_human_action_guide(auth_required, None, true);
            }
            return false;
        }
        let prepared = match crate::input::prepare_with_literals(
            &draft.text,
            &self.options.workspace,
            &self.files,
            &draft.literal_ranges(),
        ) {
            Ok(p) => p,
            Err(e) => {
                self.error(&e.to_string());
                return false;
            }
        };
        self.log_prompt(&self.editor.display_text());
        for warning in &prepared.warnings {
            self.log_warning(warning);
        }
        self.remember_draft(&draft);
        self.scroll = 0;
        self.pending = Some((draft, std::mem::take(&mut self.files)));
        self.editor.set(String::new());
        self.completion = None;
        self.clear_mention();
        let command = if let Some(id) = &self.active {
            Command::Steer {
                id: id.clone(),
                message: prepared.text,
                files: prepared.files,
                temp_files: prepared.temp_files,
                account: self.options.account.clone(),
            }
        } else {
            self.phase = if prepared.files.is_empty() {
                "starting"
            } else {
                "uploading"
            }
            .into();
            self.started = Instant::now();
            Command::Start(
                self.options
                    .agent_request(prepared.text, self.parent.clone()),
                prepared.files,
                prepared.temp_files,
            )
        };
        self.send(sink, command);
        false
    }

    pub(super) fn confirm_conversation_delete(&mut self) {
        if self.busy() {
            return;
        }
        let Some(choice) = self.menu.as_ref().and_then(|m| m.choices.get(m.selected)) else {
            return;
        };
        let Selection::Resume(id) = &choice.value else {
            return;
        };
        let id = id.clone();
        let label = clip(&choice.label, 72);
        self.resume_notice = None;
        self.resume_menu = self.menu.take();
        self.menu = Some(Menu {
            title: "Delete conversation? · d confirm · Esc cancel".into(),
            choices: vec![
                Choice {
                    label: "Cancel".into(),
                    value: Selection::CancelDelete,
                },
                Choice {
                    label: format!("Delete: {label}"),
                    value: Selection::DeleteConversation(id),
                },
            ],
            selected: 0,
        });
    }

    pub(super) fn delete_conversation_key(&mut self, sink: &mut dyn EffectSink) {
        if self.resume_menu.is_some() {
            if let Some(menu) = self.menu.as_mut() {
                if let Some(index) = menu
                    .choices
                    .iter()
                    .position(|choice| matches!(choice.value, Selection::DeleteConversation(_)))
                {
                    menu.selected = index;
                    self.select(sink);
                }
            }
        } else {
            self.confirm_conversation_delete();
        }
    }

    pub(super) fn clear_resume_web_history(&mut self) {
        if self.resume_menu.is_none()
            && self.menu.as_ref().is_some_and(|menu| {
                matches!(
                    menu.choices.first().map(|choice| &choice.value),
                    Some(Selection::Resume(_))
                )
            })
        {
            self.open_history_clear();
        }
    }

    pub(super) fn move_menu(&mut self, up: bool) {
        let preview_theme = if let Some(menu) = &mut self.menu {
            menu.selected = if up {
                menu.selected.saturating_sub(1)
            } else {
                menu.selected
                    .saturating_add(1)
                    .min(menu.choices.len().saturating_sub(1))
            };
            match menu.choices.get(menu.selected).map(|c| &c.value) {
                Some(Selection::Theme(theme)) => Some(*theme),
                _ => None,
            }
        } else {
            None
        };
        if let Some(theme) = preview_theme {
            self.theme = theme;
        }
    }

    pub(super) fn close_menu(&mut self) {
        if let Some(original) = self.theme_preview_origin.take() {
            self.theme = original;
        }
        self.menu = self.resume_menu.take();
    }

    pub(super) fn select(&mut self, sink: &mut dyn EffectSink) {
        self.theme_preview_origin = None;
        let Some(mut menu) = self.menu.take() else {
            return;
        };
        if menu.choices.is_empty() {
            return;
        }
        match menu.choices.remove(menu.selected).value {
            Selection::Provider {
                account_id,
                provider_id,
                provider_name,
            } => {
                self.switch_provider(&account_id, &provider_id, &provider_name);
            }
            Selection::SetupProvider {
                provider_id,
                provider_name,
            } => {
                self.begin_provider_setup(&provider_id, &provider_name, sink);
            }
            Selection::Model(model) => {
                self.options.overrides.insert("model".into());
                self.options.overrides.insert("thinking".into());
                self.options.thinking = model.level().id.clone();
                self.options.model = model.id;
                if self.active.is_none() {
                    self.active_model = None;
                }
                self.save_model_preference();
                let model_name = self.model_label().to_owned();
                let thinking_name = self.thinking_label().to_owned();
                let plain = format!("Next turn: {model_name} · thinking {thinking_name}");
                self.log_spans(
                    &plain,
                    vec![
                        ("· ".into(), Face::ACCENT),
                        ("Next turn: ".into(), Face::MUTED),
                        (model_name, Face::ACCENT_BOLD),
                        (" · thinking ".into(), Face::MUTED),
                        (thinking_name, Face::HIGHLIGHT_BOLD),
                    ],
                );
            }
            Selection::Theme(theme) => self.choose_theme(theme),
            Selection::Mode { agent, yolo } => self.set_agent_mode(agent, yolo),
            Selection::Permission { perms, .. } => self.apply_permissions(perms),
            Selection::Shell(allow) => {
                let mut perms = self.options.effective_permissions();
                perms.shell = allow;
                self.apply_permissions(perms);
            }
            Selection::Pace(pace) => {
                let store = crate::store::Store::open(&crate::cli::root()).ok();
                self.options.pace = Some(pace);
                crate::gemini::set_human_pace(store.as_ref(), pace);
                self.send(sink, Command::Pace(pace));
                self.log(&format!("Humanization pace set to {}", pace.description()));
            }
            Selection::Debug(level) => self.set_debug_level(level),
            Selection::Resume(id) => {
                self.phase = "loading".into();
                self.send(sink, Command::Load(id, self.options.clone()));
            }
            Selection::CancelDelete => self.close_menu(),
            Selection::DeleteConversation(id) => {
                self.menu = self.resume_menu.take();
                self.phase = "loading".into();
                self.deleting_conversation = Some(id.clone());
                if let Err(error) =
                    sink.send_command(Command::DeleteConversation(id, self.options.clone()))
                {
                    self.update(Update::ConversationDeleteError(format!(
                        "could not queue deletion: {error}"
                    )));
                }
            }
        }
    }
}
