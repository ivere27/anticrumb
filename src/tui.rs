//! Keyboard-first terminal client with an application-owned service runtime.
mod cache;
mod clipboard;
mod commands;
mod debug_log;
mod editor;
mod input_loop;
mod layout;
mod markdown;
mod mentions;
mod model_picker;
mod platform;
pub(crate) mod preferences;
mod reducer;
mod render;
mod selection;
mod theme;
mod transcript;
mod usage;
mod worker;

pub(crate) use input_loop::run;
pub use input_loop::run_with_client;
use transcript::clip;

#[cfg(test)]
mod tests {
    use super::commands::{complete, Choice, Menu, Selection};
    use super::editor::{self, Editor};
    use super::markdown;
    use super::model_picker::ModelChoice;
    use super::preferences::Preferences;
    use super::reducer::{EffectSink, SelectionTarget, State, SubagentView};
    use super::theme;
    use super::transcript::{clip, Entry};
    use super::worker::{Command, Update};
    use crate::cli::PromptOptions;
    use crossterm::event::{self, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use serde_json::json;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn cache_runtime_commands_preview_prune_and_preserve_active_work_state() {
        let mut state = State::new(PromptOptions::default());
        let (mut tx, rx) = mpsc::sync_channel(8);
        for (input, prune, dry_run, days) in [
            ("/cache", false, false, 7),
            ("/cache prune --dry-run", true, true, 7),
            ("/cache prune 14", true, false, 14),
        ] {
            state.editor.set(input.into());
            state.submit(&mut tx);
            let Command::Cache(options) = rx.try_recv().unwrap() else {
                panic!("expected cache request");
            };
            assert_eq!(
                (options.prune, options.dry_run, options.days),
                (prune, dry_run, days)
            );
        }
        state.editor.set("/cache prune 0".into());
        state.submit(&mut tx);
        assert!(rx.try_recv().is_err());
        state.update(Update::Started("active-run".into()));
        let phase = state.phase.clone();
        state.editor.set("keep this draft".into());
        state.update(Update::Cache(
            crate::rpc::pb::CacheReport {
                stores: vec![
                    crate::rpc::pb::CacheStoreReport {
                        role: "workspace".into(),
                        stats: Some(crate::rpc::pb::CacheStats {
                            busy: true,
                            reclaimable_bytes: 4096,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    crate::rpc::pb::CacheStoreReport {
                        role: "browser".into(),
                        error: "connection unavailable".into(),
                        ..Default::default()
                    },
                ],
            },
            crate::store::cache::Options {
                prune: true,
                dry_run: true,
                ..Default::default()
            },
        ));
        state.update(Update::CacheError("storage unavailable".into()));
        assert_eq!(state.active.as_deref(), Some("active-run"));
        assert_eq!(state.phase, phase);
        assert_eq!(state.editor.text, "keep this draft");
        let logs = state.logs();
        assert!(logs.contains("Preview only"));
        assert!(logs.contains("attachment deletion deferred"));
        assert!(logs.contains("browser: unavailable / incomplete"));
    }

    #[test]
    fn copy_tracks_assistant_source_and_does_not_copy_status_or_tool_output() {
        let mut state = State::new(PromptOptions::default());
        let (mut tx, rx) = mpsc::sync_channel(4);
        state.editor.set("/copy".into());
        state.submit(&mut tx);
        assert!(state.logs().contains("No assistant response to copy yet."));
        assert!(rx.try_recv().is_err());
        let source = "**Keep Markdown**\n\n| A | B |\n| - | - |\n| \u{754c}\u{8a9e} | 42 |\n";
        state.update(Update::Finished(
            json!({"state":"completed","result":{"final_response":source}}),
        ));
        assert_eq!(state.copy_response, source);
        state.log("an unrelated status message");
        state.update(Update::Started("next-turn".into()));
        assert_eq!(state.copy_response, source);
        state.update(Update::Event(crate::domain::Event {
            sequence: 1,
            run_id: "next-turn".into(),
            kind: "model_response".into(),
            data: json!({"text":"**Working**\n[TOOL_CALL]\n{\"tool\":\"read_file\",\"args\":{}}\n[/TOOL_CALL]\n"}),
            at: 0,
        }));
        assert_eq!(state.copy_response, "**Working**\n");
        state.active = None;
        state.editor.set("/new".into());
        state.submit(&mut tx);
        assert!(state.copy_response.is_empty());
    }

    #[test]
    fn mouse_selection_uses_painted_scrolled_rows_and_excludes_composer_and_menus() {
        let mut state = State::new(PromptOptions::default());
        state.painted_transcript = markdown::lines("visible first\nsecond row", 79);
        state.painted_size = (80, 26);
        state.scroll = 12;
        let top = state.compute_layout(80, 26).unwrap().0.transcript.start;
        let mouse = |kind, column, row| event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 24));
        assert!(
            state.selection.is_none(),
            "composer does not start a selection"
        );
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, top));
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3, top + 1));
        state.log("new output while dragging");
        state.stream = "new streamed text".into();
        assert_eq!(
            state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, top + 1)),
            Some("sible first\nseco".into())
        );
        assert!(state.selection.is_none());
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, top));
        assert!(state
            .transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, top))
            .is_none());
        state.menu = Some(Menu {
            title: "Menu".into(),
            choices: Vec::new(),
            selected: 0,
        });
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, top));
        assert!(state.selection.is_none());
    }

    #[test]
    fn double_click_selects_text_block_and_drag_extends_by_blocks() {
        let mut state = State::new(PromptOptions::default());
        state.painted_transcript = markdown::lines("hello world_one two\nnext row", 79);
        state.painted_size = (80, 26);
        let top = state.compute_layout(80, 26).unwrap().0.transcript.start;
        let mouse = |kind, column, row| event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // First click + release copies nothing, arms double-click.
        assert!(state
            .transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 8, top))
            .is_none());
        assert!(state
            .transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 8, top))
            .is_none());

        // Second click on `world_one` immediately selects the entire `world_one` block.
        assert!(state
            .transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 8, top))
            .is_none());
        assert_eq!(
            state.selection.as_ref().and_then(|s| s.text()),
            Some("world_one".into())
        );

        // Dragging onto `two` extends the selection through the whole `two` block.
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 17, top));
        assert_eq!(
            state.selection.as_ref().and_then(|s| s.text()),
            Some("world_one two".into())
        );

        // Dragging backward onto `hello` extends to the start of `hello`.
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2, top));
        assert_eq!(
            state.selection.as_ref().and_then(|s| s.text()),
            Some("hello world_one".into())
        );

        // Releasing copies the block-aligned selection.
        assert_eq!(
            state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, top)),
            Some("hello world_one".into())
        );

        // Double-click followed immediately by release (no drag) also copies that block.
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, top));
        state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, top));
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, top));
        assert_eq!(
            state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, top)),
            Some("hello".into())
        );

        // Micro-drag/jitter during first click must not disarm double-click.
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 8, top));
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 9, top));
        state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 9, top));
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, top));
        assert_eq!(
            state.selection.as_ref().and_then(|s| s.text()),
            Some("world_one".into())
        );
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 17, top));
        assert_eq!(
            state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 17, top)),
            Some("world_one two".into())
        );
    }

    #[test]
    fn clear_all_history_requires_exact_confirmation_and_freezes_account() {
        let mut state = State::new(PromptOptions {
            account: "chosen-account".into(),
            ..Default::default()
        });
        let (mut tx, rx) = mpsc::sync_channel(4);
        let key = |code| event::KeyEvent::new(code, KeyModifiers::NONE);
        state.open_history_clear();
        state.history_clear_key(key(KeyCode::Enter), &mut tx);
        assert!(state.history_clear_confirmation.is_none());
        assert!(rx.try_recv().is_err());
        state.open_history_clear();
        for ch in "delete".chars() {
            state.history_clear_key(key(KeyCode::Char(ch)), &mut tx);
        }
        state.history_clear_key(key(KeyCode::Enter), &mut tx);
        assert!(state.history_clear_confirmation.is_some());
        assert!(rx.try_recv().is_err());
        state.history_clear_key(key(KeyCode::Esc), &mut tx);
        assert!(state.history_clear_confirmation.is_none());
        state.open_history_clear();
        state.options.account = "different-account".into();
        for ch in "DELETE".chars() {
            state.history_clear_key(key(KeyCode::Char(ch)), &mut tx);
        }
        let repeated_enter = event::KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            event::KeyEventKind::Repeat,
        );
        state.history_clear_key(repeated_enter, &mut tx);
        assert!(rx.try_recv().is_err());
        state.history_clear_key(key(KeyCode::Enter), &mut tx);
        assert!(
            matches!(rx.try_recv().unwrap(), Command::ClearWebHistory(account) if account == "chosen-account")
        );
        assert!(state.busy());
        state.update(Update::WebHistoryError("fixture rejected".into(), true));
        assert!(!state.busy());
        assert!(state.logs().contains("fixture rejected"));
        state.active = Some("active-task".into());
        state.open_history_clear();
        assert!(state.history_clear_confirmation.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resume_bulk_web_delete_works_without_local_entries_and_menu_cancel_preserves_selection() {
        let mut state = State::new(PromptOptions::default());
        let (mut tx, rx) = mpsc::sync_channel(4);
        state.editor.set("/resume clear-history".into());
        state.submit(&mut tx);
        assert!(state.history_clear_confirmation.is_some());
        assert!(
            rx.try_recv().is_err(),
            "opening confirmation cannot delete history"
        );
        state.history_clear_key(
            event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut tx,
        );
        state.update(Update::Conversations(vec![
            json!({"id":"one", "request":{"task":"Saved"}}),
        ]));
        state.clear_resume_web_history();
        assert!(state.history_clear_confirmation.is_some());
        state.history_clear_key(
            event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut tx,
        );
        assert_eq!(state.menu.as_ref().unwrap().choices.len(), 1);
        assert!(rx.try_recv().is_err());
        state.clear_resume_web_history();
        for ch in "DELETE".chars() {
            state.history_clear_key(
                event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut tx,
            );
        }
        state.history_clear_key(
            event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut tx,
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            Command::ClearWebHistory(_)
        ));
        assert!(state.menu.is_none());
    }

    #[test]
    fn history_cleanup_polls_announce_progress_once_and_keep_historical_results_quiet() {
        let mut state = State::new(PromptOptions::default());
        let status = |phase: &str, deleted: u32, updated: i64| json!({"account_id":"default", "state":phase, "deleted":deleted, "error":"", "started_at":1, "updated_at":updated});
        state.notice_web_history(status("completed", 4, 1), false);
        assert!(state.logs().is_empty());
        state.notice_web_history(status("completed", 4, 2), false);
        assert!(state.logs().is_empty());
        state.notice_web_history(status("pending", 0, 3), true);
        assert!(state.logs().contains("pending"));
        assert!(state.status_text(120).contains("History:0 deleted"));
        state.notice_web_history(status("running", 2, 4), false);
        assert!(state.logs().contains("running · 2 chats deleted"));
        let logs = state.logs();
        state.notice_web_history(status("running", 2, 5), false);
        assert_eq!(state.logs(), logs);
        state.notice_web_history(status("completed", 4, 6), false);
        assert!(state.logs().contains("completed · 4 chats deleted"));
        assert!(!state.status_text(120).contains("History:"));
    }

    #[test]
    fn cleanup_notifications_use_a_baseline_and_show_pending_and_failures() {
        let mut state = State::new(PromptOptions::default());
        let status = |pending: u32, completed: u32, failed: u32, error: &str| {
            json!({
                "chat_cleanup": {
                    "pending": pending, "running": 0, "completed": completed,
                    "failed": failed, "unmapped": 0, "last_error": error
                }
            })
        };
        state.update(Update::Status(status(0, 7, 0, "")));
        assert!(!state.logs().contains("Web chat cleanup"));
        state.update(Update::Status(status(1, 7, 0, "")));
        assert!(state.logs().contains("1 queued"));
        assert!(state.status_text(120).contains("Cleanup:1"));
        let prior_logs = state.logs();
        state.update(Update::Status(status(1, 7, 0, "")));
        assert_eq!(state.logs(), prior_logs);
        state.update(Update::Status(status(0, 8, 0, "")));
        assert!(state.logs().contains("8 completed"));
        assert!(!state.status_text(120).contains("Cleanup:"));
        state.update(Update::Status(status(0, 8, 1, "Sign in required")));
        assert!(state.status_text(120).contains("Cleanup:!1"));
        assert!(state
            .resume_notice
            .as_deref()
            .unwrap()
            .contains("Sign in required"));
        let details = state.chat_cleanup_details().unwrap();
        assert!(details.contains("1 failed"));
        assert!(details.contains("Latest cleanup issue: Sign in required"));
        state.update(Update::Status(status(1, 8, 1, "Sign in required")));
        assert!(state
            .resume_notice
            .as_deref()
            .unwrap()
            .starts_with("Web chat cleanup: 1 queued"));
    }

    #[test]
    fn wheel_and_page_scrolling_use_wrapped_stream_rows_and_clamp_after_resize() {
        let mut state = State::new(PromptOptions::default());
        state.log("history");
        state.stream = "abcdefg ".repeat(60);
        let viewport = usize::from(state.compute_layout(24, 10).unwrap().0.transcript.len);
        let maximum = state.transcript_lines(24).len() - viewport;
        state.scroll_wheel(true, 24, 10);
        assert_eq!(state.scroll, 3);
        state.scroll_transcript(true, usize::MAX, 24, 10);
        assert_eq!(state.scroll, maximum);
        state.scroll = usize::MAX;
        state.scroll_wheel(false, 24, 10);
        assert_eq!(state.scroll, maximum - 3);
        state.scroll_transcript(false, usize::MAX, 24, 10);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn wheel_scrolls_open_menu_without_changing_draft_or_transcript_offset() {
        let mut state = State::new(PromptOptions::default());
        state.editor.set("draft".into());
        state.scroll = 4;
        state.menu = Some(Menu {
            title: "fixture".into(),
            choices: vec![
                Choice {
                    label: "first".into(),
                    value: Selection::Shell(false),
                },
                Choice {
                    label: "second".into(),
                    value: Selection::Shell(true),
                },
            ],
            selected: 0,
        });
        state.scroll_wheel(false, 80, 24);
        state.scroll_wheel(false, 80, 24);
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        state.scroll_wheel(true, 80, 24);
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
        assert_eq!(state.scroll, 4);
        assert_eq!(state.editor.text, "draft");
    }

    #[test]
    fn login_completion_loads_choices_without_opening_a_menu_or_submitting_the_draft() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        let (mut tx, rx) = mpsc::sync_channel(4);
        state.editor.set("Keep this unfinished task".into());
        state.update(Update::Status(json!({"accounts":[{
            "id":"default", "state":"awaiting_human", "models":[],
            "login":{"state":"waiting","error":""}
        }]})));
        assert!(state
            .logs()
            .contains("Model choices will load automatically"));
        assert!(!state.submit(&mut tx));
        assert!(rx.try_recv().is_err());
        let ready = json!({"accounts":[{
            "id":"default", "state":"ready", "models":[{"mode_id":"fresh-model"}],
            "login":{"state":"ready","error":""}
        }]});
        state.update(Update::Status(ready.clone()));
        assert_eq!(state.catalog["models"][0]["mode_id"], "fresh-model");
        assert!(state.logs().contains("Model choices are ready"));
        assert_eq!(state.editor.text, "Keep this unfinished task");
        assert!(state.menu.is_none());
        assert!(
            rx.try_recv().is_err(),
            "login completion must not submit a task"
        );
        let logged = state.logs();
        state.update(Update::Status(ready));
        assert_eq!(
            state.logs(),
            logged,
            "polling must not repeat completion notices"
        );
    }

    #[test]
    fn login_failure_keeps_the_previous_choices_and_reports_recovery_once() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        let status = |account_state, login| {
            json!({"accounts":[{
                "id":"default", "state":account_state, "models":[{"mode_id":"saved-model"}],
                "login":login
            }]})
        };
        state.update(Update::Status(status(
            "awaiting_human",
            json!({"state":"waiting","error":""}),
        )));
        state.editor.set("unfinished draft".into());
        let failed = status(
            "adapter_error",
            json!({"state":"failed","error":"Automatic model refresh failed (UiContractChanged at models): UI contract changed: empty thinking menu. Retry with /model refresh."}),
        );
        state.update(Update::Status(failed.clone()));
        assert!(state.logs().contains("empty thinking menu"));
        assert!(state.logs().contains("Retry with /model refresh"));
        assert_eq!(state.logs().matches("/model refresh").count(), 1);
        assert!(!state.logs().contains(".."));
        assert!(!state.logs().contains("Account is available again"));
        assert_eq!(state.catalog["models"][0]["mode_id"], "saved-model");
        assert_eq!(state.editor.text, "unfinished draft");
        let logged = state.logs();
        state.update(Update::Status(failed));
        assert_eq!(state.logs(), logged);
    }

    #[test]
    fn daemon_connection_status_reports_active_remote_and_preserves_context_when_disconnected() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        let status = json!({
            "server_id":"workspace_fixture",
            "accounts":[{"id":"default","state":"ready"}],
            "browser_connection": {
                "transport":"grpc", "endpoint":"https://browser.example:50051",
                "server_id":"browser_fixture", "connected":true, "error":"",
                "token":"synthetic-secret-must-not-be-displayed"
            }
        });
        state.update(Update::Status(status.clone()));
        assert!(state
            .logs()
            .contains("Daemon: remote https://browser.example:50051 · connected"));
        let remote_footer = state.status_text(100);
        assert!(remote_footer.ends_with("D:remote(browser.example:50051)"));
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(remote_footer.as_str()),
            100
        );
        let remote_spans = state.status_spans(100);
        assert_eq!(remote_spans.last().unwrap().1, markdown::Face::MUTED);
        let (mut tx, rx) = mpsc::sync_channel(4);
        state.editor.set("/status".into());
        state.submit(&mut tx);
        assert!(matches!(rx.try_recv().unwrap(), Command::Status));
        state.update(Update::StatusReport(Some(status.clone())));
        assert!(state.logs().contains("Daemon server: browser_fixture"));
        assert!(state.logs().contains("Workspace server: workspace_fixture"));
        assert!(!state.logs().contains("synthetic-secret"));
        assert!(
            matches!(rx.try_recv().unwrap(), Command::WebHistoryStatus(account, true) if account == "default")
        );
        assert!(rx.try_recv().is_err());

        state.editor.set("unfinished work".into());
        state.update(Update::StatusDisconnected);
        let offline_footer = state.status_text(100);
        assert!(offline_footer.ends_with("D:remote/offline(browser.example:50051)"));
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(offline_footer.as_str()),
            100
        );
        assert_eq!(
            state.status_spans(100).last().unwrap().1,
            markdown::Face::DANGER_BOLD
        );
        let details = state.daemon_details();
        assert!(details.contains("https://browser.example:50051 · disconnected"));
        assert!(details.contains("Daemon server: browser_fixture"));
        assert!(details.contains("anticrumb connect status"));
        assert_eq!(state.editor.text, "unfinished work");
        let logged = state.logs();
        state.update(Update::StatusDisconnected);
        assert_eq!(
            state.logs(),
            logged,
            "status polling must not repeat the same notice"
        );
        state.update(Update::Status(status));
        assert_eq!(state.daemon_label(), "remote");
        assert!(!state
            .daemon_details()
            .contains("Workspace status is unavailable"));
    }

    #[test]
    fn footer_right_aligns_daemon_ip_port_and_turns_red_when_disconnected() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            workspace: "/tmp/project".into(),
            ..Default::default()
        });
        state.update(Update::Status(json!({
            "accounts": [{"id": "default", "state": "ready"}],
            "browser_connection": {
                "transport": "grpc",
                "endpoint": "https://192.168.1.10:50051",
                "connected": true
            }
        })));
        let connected = state.status_text(110);
        assert!(connected.ends_with("  D:remote(192.168.1.10:50051)"));
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(connected.as_str()),
            110
        );
        assert_eq!(
            state.status_spans(110).last().unwrap().1,
            markdown::Face::MUTED
        );

        // Remote daemon offline while workspace daemon is still reachable -> red at bottom-right.
        state.update(Update::Status(json!({
            "accounts": [{"id": "default", "state": "ready"}],
            "browser_connection": {
                "transport": "grpc",
                "endpoint": "https://192.168.1.10:50051",
                "connected": false
            }
        })));
        let remote_down = state.status_text(110);
        assert!(remote_down.ends_with("  D:remote/offline(192.168.1.10:50051)"));
        assert_eq!(
            state.status_spans(110).last().unwrap().1,
            markdown::Face::DANGER_BOLD
        );

        let mut out = Vec::new();
        state.render_frame(&mut out, 110, 26).expect("render_frame");
        let rendered = String::from_utf8_lossy(&out);
        let footer_pos = rendered.find("\x1b[26;1H").expect("footer row");
        let footer_ansi = &rendered[footer_pos..];
        let danger_fg = format!(
            "{}",
            crossterm::style::SetForegroundColor(state.theme.danger)
        );
        assert!(
            footer_ansi.contains(&danger_fg),
            "disconnected daemon badge must use theme danger color"
        );
        assert!(footer_ansi.contains("D:remote/offline(192.168.1.10:50051)"));

        // IPv6 endpoint and local disconnect also align right and use danger color.
        state.update(Update::Status(json!({
            "browser_connection": {
                "transport": "grpc",
                "endpoint": "https://[::1]:50051",
                "connected": true
            }
        })));
        assert!(state.status_text(110).ends_with("  D:remote([::1]:50051)"));

        state.update(Update::Status(json!({
            "browser_connection": {
                "transport": "local",
                "endpoint": "",
                "connected": true
            }
        })));
        assert!(state.status_text(110).ends_with("  D:local"));
        assert_eq!(
            state.status_spans(110).last().unwrap().1,
            markdown::Face::MUTED
        );
        state.update(Update::StatusDisconnected);
        assert!(state.status_text(110).ends_with("  D:local/offline"));
        assert_eq!(
            state.status_spans(110).last().unwrap().1,
            markdown::Face::DANGER_BOLD
        );

        // Long quota notice stays untruncated on very narrow terminals (e.g. 48 cols / safe_w=47).
        state.update(Update::WebUsage(json!({
            "account_id": "default",
            "limits": [
                {"label": "Current usage", "used_percent": 25.0, "reset": "Resets at 5:41 AM"},
                {"label": "Weekly limit", "used_percent": 0.0, "reset": "Resets Nov 20 at 3:41 PM"}
            ]
        })));
        assert!(state
            .status_text(47)
            .starts_with("Left:≈75% @5:41am Wk:≈100% @Nov 20 3:41pm"));
        // When right-aligned daemon badge is omitted on narrow terminals, remaining space renders left items.
        let narrow_52 = state.status_text(52);
        assert!(narrow_52.starts_with("Left:≈75% @5:41am Wk:≈100% @Nov 20 3:41pm · M:"));
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(narrow_52.as_str()),
            52
        );
    }

    #[test]
    fn daemon_startup_log_includes_four_tls_fingerprint_emojis_while_footer_stays_plain() {
        let mut state = State::new(PromptOptions::default());
        let pin = crate::domain::digest(b"abc");
        state.remote_tls_pin = Some(pin);
        state.remote_tls_endpoint = Some("https://100.121.6.125:50051".into());
        state.update(Update::Status(json!({
            "accounts": [{"id": "default", "state": "ready"}],
            "browser_connection": {
                "transport": "grpc",
                "endpoint": "https://100.121.6.125:50051/",
                "connected": true
            }
        })));
        assert!(state
            .logs()
            .contains("Daemon: remote https://100.121.6.125:50051/ (🦍🐆🦇🐪) · connected"));
        let footer = state.status_text(110);
        assert!(footer.ends_with("  D:remote(100.121.6.125:50051)"));
        assert!(!footer.contains("🦍"));

        state.update(Update::StatusDisconnected);
        assert!(state
            .logs()
            .contains("Daemon: remote https://100.121.6.125:50051/ (🦍🐆🦇🐪) · disconnected"));

        // Missing reported endpoint must not match saved TLS pin.
        state.update(Update::Status(json!({
            "browser_connection": {
                "transport": "grpc",
                "connected": true
            }
        })));
        assert_eq!(state.daemon_tls_emoji(), None);
    }

    #[test]
    fn local_runtime_is_distinguished_from_hosts_without_connection_metadata() {
        let mut state = State::new(PromptOptions::default());
        assert_eq!(state.daemon_label(), "checking");
        {
            let transport = "local";
            state.update(Update::Status(json!({"browser_connection": {
                "transport":transport,"endpoint":"","server_id":"local_browser",
                "connected":true,"error":""
            }})));
            assert_eq!(state.daemon_label(), "local");
            assert_eq!(state.daemon_summary(), "Runtime: local");
            assert!(state
                .daemon_details()
                .contains(&format!("Daemon transport: {transport}")));
        }
        state.update(Update::Status(json!({"server_id":"older_workspace"})));
        assert_eq!(state.daemon_label(), "unknown");
        assert!(!state.daemon_summary().contains("connected"));
        state.update(Update::StatusDisconnected);
        assert_eq!(state.daemon_label(), "offline");
    }

    #[test]
    fn runtime_health_separates_ipc_from_browser_and_marks_old_data() {
        let mut state = State::new(PromptOptions::default());
        let status = json!({
            "runtime": {"pid":4321,"version":"fixture","automatic":true,
                "transport":"unix","endpoint":"/tmp/fixture.sock","uptime_ms":65000,
                "connected_clients":2,"stopping":false,"token":"never-print-this"},
            "active_agents":3,"queued":1,"running":2,
            "browser_connection":{"transport":"grpc","endpoint":"https://browser.example:50051",
                "connected":false,"error":"Browser daemon connection failed"}
        });
        state.update(Update::StatusReport(Some(status.clone())));
        let details = state.runtime_details();
        assert!(details.contains("Local daemon: responding"));
        assert!(details.contains("PID: 4321"));
        assert!(details.contains("automatic; exits after last client"));
        assert!(details.contains("Uptime: 0h 01m 05s"));
        assert!(details.contains("Connected clients: 2 (includes this TUI)"));
        assert!(details.contains("Workspace jobs: 3 agents, 1 queued turns, 2 running turns"));
        assert_eq!(state.daemon_label(), "remote/offline");
        assert!(!state.logs().contains("never-print-this"));

        state.last_status_at = Some(Instant::now() - Duration::from_secs(8));
        assert!(state
            .runtime_details()
            .contains("Local daemon: status is stale"));
        assert!(state.runtime_details().contains("last known values"));
        state.update(Update::StatusReport(None));
        assert!(state
            .runtime_details()
            .contains("Local daemon: disconnected"));
        assert!(state.runtime_details().contains("PID: 4321"));
        state.update(Update::StatusReport(Some(status)));
        assert!(state.runtime_details().contains("Local daemon: responding"));
        assert!(!state.runtime_details().contains("last known values"));
    }

    #[test]
    fn human_verification_keeps_draft_and_requires_explicit_resubmission() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        let (mut tx, rx) = mpsc::sync_channel(4);
        state.editor.set("Finish the task".into());
        state.update(Update::Status(
            json!({"accounts":[{"id":"default","state":"awaiting_human"}]}),
        ));
        assert!(!state.submit(&mut tx));
        assert_eq!(state.editor.text, "Finish the task");
        assert!(rx.try_recv().is_err());
        state.update(Update::Status(
            json!({"accounts":[{"id":"default","state":"ready"}]}),
        ));
        assert!(
            rx.try_recv().is_err(),
            "verification must not replay the draft"
        );
        assert!(state.logs().contains("resume explicitly"));
    }

    #[test]
    fn input_history_preserves_draft_and_ignores_down_at_the_end() {
        let mut state = State::new(PromptOptions::default());
        state.remember("first task");
        state.remember("/model");
        state.editor.set("unfinished draft".into());
        state.next_input();
        assert_eq!(state.editor.text, "unfinished draft");
        state.previous_input();
        assert_eq!(state.editor.text, "/model");
        state.previous_input();
        assert_eq!(state.editor.text, "first task");
        state.previous_input();
        assert_eq!(state.editor.text, "first task");
        state.next_input();
        assert_eq!(state.editor.text, "/model");
        state.next_input();
        assert_eq!(state.editor.text, "unfinished draft");
    }

    #[test]
    fn paste_submission_history_and_failed_submission_keep_literal_full_payload() {
        let dir = tempfile::tempdir().unwrap();
        let options = PromptOptions {
            workspace: dir.path().into(),
            account: "default".into(),
            ..Default::default()
        };
        let mut state = State::new(options.clone());
        state.preferences = Some(Preferences::open(dir.path()).unwrap());
        let payload = format!("\n/exit @missing @@literal\n{}\n", "\u{754c}".repeat(1001));
        state.paste(&payload);
        let expected = state.editor.draft();
        let (mut tx, rx) = mpsc::sync_channel(4);
        assert!(
            !state.submit(&mut tx),
            "a hidden slash command is prompt text"
        );
        let Command::Start(request, files, _) = rx.try_recv().unwrap() else {
            panic!("expected prompt submission");
        };
        assert_eq!(request.task, payload);
        assert!(files.is_empty());
        assert!(state.logs().contains("[Pasted Content"));
        assert!(!state.logs().contains("@missing"));
        state.error("fixture error");
        assert_eq!(state.editor.draft(), expected);
        assert!(state.editor.display_text().contains("[Pasted Content"));

        let mut restarted = State::new(options);
        restarted.preferences = Some(Preferences::open(dir.path()).unwrap());
        restarted.load_history();
        restarted.previous_input();
        assert_eq!(restarted.editor.draft(), expected);
        restarted.editor.expand_paste();
        assert!(
            !restarted.submit(&mut tx),
            "expanded paste keeps literal slash semantics"
        );
        let Command::Start(request, _, _) = rx.try_recv().unwrap() else {
            panic!("expected prompt submission");
        };
        assert_eq!(request.task, payload);
    }

    #[test]
    fn paste_limit_reports_error_and_keeps_draft_for_editing() {
        let mut state = State::new(PromptOptions::default());
        state.paste(&"a".repeat(1001));
        let expected = state.editor.draft();
        state.paste(&"b".repeat(editor::MAX_INPUT_BYTES));
        assert_eq!(state.editor.draft(), expected);
        assert!(state.logs().contains("48,000-byte input limit"));
        assert!(state.logs().contains("draft is unchanged"));
    }

    #[test]
    fn unicode_cursor_paste_and_wrap_are_safe() {
        let mut e = Editor::default();
        e.insert("\u{754c}\u{8a9e}\r\nabc");
        e.left();
        e.backspace();
        e.insert("界");
        assert_eq!(e.text, "\u{754c}\u{8a9e}\na界c");
        e.cursor = 0;
        e.delete();
        assert_eq!(e.text, "\u{8a9e}\na界c");
        let (rows, pos) = e.layout(3);
        assert_eq!(rows, vec!["\u{8a9e}", "a界", "c"]);
        assert_eq!(pos, (0, 0));
        e.insert("\u{1b}[31m");
        assert!(!e.text.contains('\u{1b}'));
    }

    #[test]
    fn completion_quotes_paths_and_ignores_emails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a b.txt"), "x").unwrap();
        let mut e = Editor::default();
        e.set("read @a".into());
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec!["@\"a b.txt\" "]
        );
        e.set("mail x@y".into());
        assert!(complete(&e, dir.path()).is_none());
        e.set("/mo".into());
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec!["/mode ", "/model "]
        );
        e.set("@git:".into());
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec!["@git:diff ", "@git:staged "]
        );
        e.set("@\"git:st".into());
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec!["@\"git:staged\" "]
        );
    }

    #[test]
    fn completion_lists_recent_git_commits_chronologically_and_handles_slash_refs() {
        let dir = tempfile::tempdir().unwrap();
        let run_out = |args: &[&str]| -> String {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_out(&["init"]);
        run_out(&["config", "user.name", "test"]);
        run_out(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("f.txt"), "1\n").unwrap();
        run_out(&["add", "f.txt"]);
        run_out(&["commit", "-m", "first"]);
        let sha1 = run_out(&["rev-parse", "--short", "HEAD"]);
        std::fs::write(dir.path().join("f.txt"), "2\n").unwrap();
        run_out(&["add", "f.txt"]);
        run_out(&["commit", "-m", "second"]);
        let sha2 = run_out(&["rev-parse", "--short", "HEAD"]);

        let mut e = Editor::default();
        e.set("@git:".into());
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec![
                "@git:diff ".to_owned(),
                "@git:staged ".to_owned(),
                format!("@git:{sha2} "),
                format!("@git:{sha1} "),
            ]
        );

        e.set(format!("@\"git:{}", &sha1[..3]));
        let quoted_vals = complete(&e, dir.path()).unwrap().values;
        assert!(quoted_vals.contains(&format!("@\"git:{sha1}\" ")));

        e.set("@git:D".into());
        assert!(complete(&e, dir.path())
            .unwrap()
            .values
            .contains(&"@git:diff ".to_owned()));

        e.set("@git:first".into());
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec![format!("@git:{sha1} ")]
        );

        // Push sha1 beyond the recent 20-commit window and verify Tab completion still resolves it.
        for i in 3..=24 {
            std::fs::write(dir.path().join("f.txt"), format!("{i}\n")).unwrap();
            run_out(&["add", "f.txt"]);
            run_out(&["commit", "-m", &format!("commit {i}")]);
        }
        e.set(format!("@git:{sha1}"));
        assert_eq!(
            complete(&e, dir.path()).unwrap().values,
            vec![format!("@git:{sha1} ")]
        );

        e.set("@git:feat/login".into());
        assert!(complete(&e, dir.path()).unwrap().values.is_empty());
    }

    #[test]
    fn failed_command_delivery_cleans_up_temp_files() {
        let patch_dir =
            std::env::temp_dir().join(format!("anticrumb-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&patch_dir).unwrap();
        let patch_file = patch_dir.join("test.patch");
        std::fs::write(&patch_file, "diff").unwrap();

        let (mut tx, rx) = mpsc::sync_channel::<Command>(0);
        drop(rx); // Disconnected

        let cmd = Command::Steer {
            id: "test-id".into(),
            message: "test".into(),
            files: vec![],
            temp_files: vec![patch_file.clone()],
            account: "default".into(),
        };
        assert!(patch_file.exists());
        let res = tx.send_command(cmd);
        assert!(res.is_err());
        assert!(!patch_file.exists());
        assert!(!patch_dir.exists());
    }

    #[test]
    fn web_usage_footer_updates_survives_polling_and_stays_visible_in_small_terminals() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        let status = json!({"browser_connection":{"server_id":"browser-a","transport":"local","connected":true}});
        state.update(Update::Status(status.clone()));
        state.update(Update::WebUsage(json!({
            "account_id":"default","server_id":"browser-a","limits":[
                {"label":"Current usage","used_percent":25.0},
                {"label":"Weekly limit","used_percent":0.0}
            ]
        })));
        assert!(state.status_text(120).contains("Left:≈75% Wk:≈100%"));
        state.update(Update::Status(status));
        assert!(clip(&state.status_text(48), 48).contains("Left:≈75% Wk:≈100%"));
        state.update(Update::WebUsageError("usage page unavailable".into()));
        let stale = clip(&state.status_text(48), 48);
        assert!(stale.contains("Left:≈75% Wk:≈100% (stale)"));
        assert!(state.logs().contains("usage page unavailable"));
        state.update(Update::WebUsage(json!({
            "account_id":"default","server_id":"browser-a","limits":[
                {"label":"Current usage","used_percent":100.0}
            ]
        })));
        assert!(state.status_text(48).starts_with("Left:≈0%"));
        assert!(!state.status_text(48).contains("stale"));
    }

    #[test]
    fn web_usage_footer_does_not_cross_account_or_browser_identity() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        state.update(Update::WebUsage(json!({
            "account_id":"default","server_id":"browser-a","limits":[
                {"label":"Current usage","used_percent":25.0}
            ]
        })));
        assert!(state.status_text(48).contains("Left:≈75%"));
        state.options.account = "other".into();
        assert!(!state.status_text(48).contains("Left:"));
        state.options.account = "default".into();
        state.update(Update::Status(
            json!({"browser_connection":{"server_id":"browser-b"}}),
        ));
        assert!(!state.status_text(48).contains("Left:"));
    }

    #[test]
    fn footer_tracks_live_model_workspace_and_usage() {
        let options = PromptOptions {
            account: "default".into(),
            workspace: "/tmp/work".into(),
            ..Default::default()
        };
        let mut state = State::new(options);
        state.options.model = Some("chosen".into());
        assert!(state.status_text(100).contains("M:chosen"));
        state.active_model = Some("actual".into());
        state.in_chars = 123;
        state.out_chars = 45;
        let footer = state.status_text(100);
        assert!(footer.contains("M:actual"));
        assert!(footer.contains("W:work"));
        assert!(footer.contains("U:123/45c T:?"));
        assert!(!footer.contains("M:chosen"));
    }

    #[test]
    fn model_selection_persists_across_tui_restarts_and_respects_cli_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = PromptOptions {
            account: "default".into(),
            workspace: dir.path().into(),
            ..Default::default()
        };
        let mut state = State::new(options.clone());
        state.preferences = Some(Preferences::open(dir.path()).unwrap());
        state.menu = Some(Menu {
            title: "Model".into(),
            choices: vec![Choice {
                label: "Pro".into(),
                value: Selection::Model(ModelChoice::new(
                    Some("pro-id".into()),
                    &json!([{"level":"high","label":"High","selected":true}]),
                    None,
                )),
            }],
            selected: 0,
        });
        let (mut tx, _) = mpsc::sync_channel(4);
        state.select(&mut tx);
        assert_eq!(state.options.model.as_deref(), Some("pro-id"));
        assert_eq!(state.options.thinking.as_deref(), Some("high"));

        let mut restarted = State::new(options.clone());
        restarted.preferences = Some(Preferences::open(dir.path()).unwrap());
        restarted.load_model_preference();
        assert_eq!(restarted.options.model.as_deref(), Some("pro-id"));
        assert_eq!(restarted.options.thinking.as_deref(), Some("high"));

        options.model = Some("cli-model".into());
        options.overrides.insert("model".into());
        let mut overridden = State::new(options);
        overridden.preferences = Some(Preferences::open(dir.path()).unwrap());
        overridden.load_model_preference();
        assert_eq!(overridden.options.model.as_deref(), Some("cli-model"));
        assert!(overridden.options.thinking.is_none());
    }

    #[test]
    fn system_and_command_messages_render_with_theme_faces() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        state.theme = theme::find("nord").unwrap();
        let (mut tx, _) = mpsc::sync_channel(4);
        state.editor.set("/help".into());
        state.submit(&mut tx);
        state.editor.set("/status".into());
        state.submit(&mut tx);
        state.error("file_name_with_underscores failed");

        let rendered = state.transcript_lines(100);
        let mut bytes = Vec::new();
        for line in &rendered {
            markdown::write_line(&mut bytes, line, state.theme).unwrap();
            bytes.push(b'\n');
        }
        let output = String::from_utf8(bytes).unwrap();
        // Nord background (46,52,64), accent (136,192,208), highlight (163,190,140),
        // danger (191,97,106), muted (216,222,233), text (236,239,244)
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetBackgroundColor(crossterm::style::Color::Rgb {
                r: 46,
                g: 52,
                b: 64
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 136,
                g: 192,
                b: 208
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 163,
                g: 190,
                b: 140
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 191,
                g: 97,
                b: 106
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 216,
                g: 222,
                b: 233
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 236,
                g: 239,
                b: 244
            })
        )));
        assert!(output.contains("Commands"));
        assert!(output.contains("/help"));
        assert!(output.contains("Conversation: new"));
        assert!(output.contains("file_name_with_underscores failed"));

        // Verify a single /status and status_spans line uses multiple theme colors while preserving exact text
        state.update(Update::Status(json!({
            "accounts": [{"id": "default", "state": "ready"}]
        })));
        state.editor.set("open @README.md".into());
        state.submit(&mut tx);
        let status_parts = state.status_spans(110);
        let joined_status: String = status_parts.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined_status, state.status_text(110));
        let status_refs: Vec<(&str, markdown::Face)> =
            status_parts.iter().map(|(t, f)| (t.as_str(), *f)).collect();
        let status_line = &markdown::spans(&status_refs, 110)[0];
        let mut status_bytes = Vec::new();
        markdown::write_line(&mut status_bytes, status_line, state.theme).unwrap();
        let status_output = String::from_utf8(status_bytes).unwrap();
        assert!(status_output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 136,
                g: 192,
                b: 208
            })
        )));
        assert!(status_output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 163,
                g: 190,
                b: 140
            })
        )));
        assert!(status_output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 216,
                g: 222,
                b: 233
            })
        )));
    }

    #[test]
    fn human_action_guide_highlights_commands_and_steps_with_theme_colors() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        state.theme = theme::find("nord").unwrap();
        state.update(Update::Status(json!({
            "accounts":[{"id":"default","state":"awaiting_human","models":[]}]
        })));
        state.editor.set("resume prompt".into());
        let (mut tx, _) = mpsc::sync_channel(4);
        state.submit(&mut tx);
        state.log_human_action_guide(true, Some(false), false);

        let rendered = state.transcript_lines(120);
        let mut bytes = Vec::new();
        for line in &rendered {
            markdown::write_line(&mut bytes, line, state.theme).unwrap();
            bytes.push(b'\n');
        }
        let output = String::from_utf8(bytes).unwrap();
        // Danger red headline, highlight green badges/steps, accent cyan commands
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 191,
                g: 97,
                b: 106
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 163,
                g: 190,
                b: 140
            })
        )));
        assert!(output.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 136,
                g: 192,
                b: 208
            })
        )));
        assert!(output.contains("Human verification (CAPTCHA) required"));
        assert!(output.contains("Draft kept"));
        assert!(output.contains("/browser show"));
        assert!(output.contains("/browser refresh"));
        assert!(output.contains("/browser hide"));
        assert!(output.contains("/browser login"));
        assert!(output.contains("Google sign-in required"));
    }

    #[test]
    fn subagent_dynamic_status_lines_appear_update_and_disappear_on_completion() {
        let mut state = State::new(PromptOptions::default());
        state.update(Update::Started("master-run".into()));
        assert_eq!(state.subagent_rows(26), 0);
        assert!(state.subagent_status_lines(100).is_empty());

        // Spawn two concurrent worker sub-agents.
        state.update(Update::Event(crate::domain::Event {
            sequence: 1,
            run_id: "master-run".into(),
            kind: "subagent_started".into(),
            data: json!({"id": "sub-1", "label": "Trace auth flow", "task": "Inspect src/auth.rs"}),
            at: 0,
        }));
        state.update(Update::Event(crate::domain::Event {
            sequence: 2,
            run_id: "master-run".into(),
            kind: "subagent_started".into(),
            data: json!({"id": "sub-2", "label": "Run RPC contract", "task": "Inspect tests/rpc_contract.rs"}),
            at: 0,
        }));
        assert_eq!(state.subagent_rows(26), 2);
        let lines = state.subagent_status_lines(100);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("worker #1 [Trace auth flow] · starting"));
        assert!(lines[1].contains("worker #2 [Run RPC contract] · starting"));

        // Progress updates individual worker line in place.
        state.update(Update::Event(crate::domain::Event {
            sequence: 3,
            run_id: "master-run".into(),
            kind: "subagent_progress".into(),
            data: json!({"id": "sub-1", "label": "Trace auth flow", "phase": "step 2", "detail": "read_file src/auth.rs"}),
            at: 0,
        }));
        let lines = state.subagent_status_lines(100);
        assert!(lines[0].contains("worker #1 [Trace auth flow] · step 2 · read_file src/auth.rs"));

        // When worker #1 finishes, its line is removed immediately and only 1 line remains.
        state.update(Update::Event(crate::domain::Event {
            sequence: 4,
            run_id: "master-run".into(),
            kind: "subagent_finished".into(),
            data: json!({"id": "sub-1", "label": "Trace auth flow", "ok": true, "elapsed_ms": 1400}),
            at: 0,
        }));
        assert_eq!(state.subagent_rows(26), 1);
        let lines = state.subagent_status_lines(100);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("[Run RPC contract]"));

        // When worker #2 finishes, all dynamic sub-agent lines disappear.
        state.update(Update::Event(crate::domain::Event {
            sequence: 5,
            run_id: "master-run".into(),
            kind: "subagent_finished".into(),
            data: json!({"id": "sub-2", "label": "Run RPC contract", "ok": true, "elapsed_ms": 2100}),
            at: 0,
        }));
        assert_eq!(state.subagent_rows(26), 0);
        assert!(state.subagent_status_lines(100).is_empty());
    }

    #[test]
    fn debug_command_toggles_levels_persists_and_logs_live_activity() {
        let dir = tempfile::tempdir().unwrap();
        let (mut tx, _) = mpsc::sync_channel(4);
        let mut state = State::new(PromptOptions {
            workspace: dir.path().into(),
            account: "default".into(),
            ..Default::default()
        });
        state.preferences = Some(Preferences::open(dir.path()).unwrap());
        state.debug.dir = Some(dir.path().to_path_buf());
        assert_eq!(state.debug.level, 0);
        assert!(!state.status_text(120).contains("DBG:"));
        assert!(state.debug.path.is_none());

        // Opening `/debug` with no arguments opens level selector menu.
        state.editor.set("/debug".into());
        assert!(!state.submit(&mut tx));
        assert_eq!(state.menu.as_ref().unwrap().choices.len(), 3);
        state.menu.as_mut().unwrap().selected = 1;
        state.select(&mut tx);
        assert_eq!(state.debug.level, 1);
        assert!(state.status_text(120).contains("DBG:1(info)"));
        assert_eq!(state.preferences.as_ref().unwrap().debug_level(), 1);
        let log_path = state
            .debug
            .path
            .clone()
            .expect("temp debug log file created when debug > 0");
        assert!(log_path.starts_with(dir.path()));
        assert!(log_path.exists());
        assert!(state.logs().contains("Debug log:"));

        // `/debug 2` / `/debug trace` enables level 2, `/debug invalid` shows usage.
        state.editor.set("/debug invalid".into());
        assert!(!state.submit(&mut tx));
        assert!(state.logs().contains("Usage: /debug"));
        state.editor.set("/debug trace".into());
        assert!(!state.submit(&mut tx));
        assert_eq!(state.debug.level, 2);
        assert!(state.status_text(120).contains("DBG:2(trace)"));

        // Live browser stages & tool completion events are surfaced clearly.
        state.update(Update::Started("job-dbg".into()));
        state.update(Update::Event(crate::domain::Event {
            sequence: 1,
            run_id: "job-dbg".into(),
            kind: "browser_stage".into(),
            data: json!({"stage": "waiting_stream", "detail": "sent 420c to Gemini Web (sess-1)"}),
            at: 0,
        }));
        assert_eq!(state.phase, "waiting");
        assert!(state
            .logs()
            .contains("[waiting_stream] sent 420c to Gemini Web"));

        state.update(Update::Event(crate::domain::Event {
            sequence: 2,
            run_id: "job-dbg".into(),
            kind: "model_chunk".into(),
            data: json!({"text": "hello"}),
            at: 0,
        }));
        assert_eq!(state.phase, "stream 5c");
        assert!(state.logs().contains("receiving response stream"));

        state.update(Update::Event(crate::domain::Event {
            sequence: 3,
            run_id: "job-dbg".into(),
            kind: "request_payload".into(),
            data: json!({
                "request": {"account_id": "default", "mode": "chat", "model": "gemini-3-pro", "thinking": "high"},
                "prompt": "[User]\nInspect src/main.rs\nLine 2 of full prompt"
            }),
            at: 0,
        }));
        assert!(state.logs().contains("[request_meta]"));
        assert!(state.logs().contains("\"model\": \"gemini-3-pro\""));
        assert!(state.logs().contains("[request_prompt]"));
        assert!(state.logs().contains("Line 2 of full prompt"));

        state.update(Update::Event(crate::domain::Event {
            sequence: 4,
            run_id: "job-dbg".into(),
            kind: "tool_started".into(),
            data: json!({"tool": "read_file", "args": {"path": "src/main.rs", "start_line": 1}}),
            at: 0,
        }));
        assert_eq!(state.phase, "tool:read_file");
        assert!(state.logs().contains("[tool_args] read_file"));
        assert!(state.logs().contains("\"start_line\": 1"));

        let long_output = format!("head {} tail", "x".repeat(300));
        state.update(Update::Event(crate::domain::Event {
            sequence: 5,
            run_id: "job-dbg".into(),
            kind: "tool_finished".into(),
            data: json!({"tool": "read_file", "ok": true, "elapsed_ms": 18, "output": long_output}),
            at: 0,
        }));
        assert!(state.logs().contains("[tool_done] read_file · ok · 18ms"));
        assert!(state.logs().contains("tail"));

        // Turning `/debug off` hides DBG badge again, saves/closes the temp log, and persists level 0.
        state.editor.set("/debug off".into());
        assert!(!state.submit(&mut tx));
        assert_eq!(state.debug.level, 0);
        assert!(!state.status_text(120).contains("DBG:"));
        assert_eq!(state.preferences.as_ref().unwrap().debug_level(), 0);
        assert!(state.debug.path.is_none());
        assert!(state.logs().contains("Debug log saved:"));

        let saved = std::fs::read_to_string(&log_path).unwrap();
        assert!(saved.contains("=== anticrumb debug log ==="));
        assert!(saved.contains("[waiting_stream] sent 420c to Gemini Web"));
        assert!(saved.contains("[request_meta]"));
        assert!(saved.contains("Line 2 of full prompt"));
        assert!(saved.contains("[tool_args]"));
        assert!(saved.contains("[tool_done]"));
        assert!(saved.contains("tail"));
        assert!(saved.contains("[debug_off] debug log closed"));
    }

    #[test]
    fn thinking_stage_updates_phase_and_transient_preview_without_debug_mode() {
        let mut state = State::new(PromptOptions::default());
        assert_eq!(state.debug.level, 0);

        state.update(Update::Started("job-think".into()));
        let mut initial_frame = Vec::new();
        state.render_frame(&mut initial_frame, 100, 24).unwrap();
        let initial_rendered = String::from_utf8(initial_frame).unwrap();
        assert!(initial_rendered.contains("working... · Esc: interrupt"));
        assert!(!state.status_text(120).contains("running"));

        state.update(Update::Event(crate::domain::Event {
            sequence: 1,
            run_id: "job-think".into(),
            kind: "browser_stage".into(),
            data: json!({
                "stage": "thinking",
                "detail": "Gemini Web thinking (42s): Analyzing build.rs",
                "summary": "Analyzing build.rs",
                "elapsed_secs": 42
            }),
            at: 0,
        }));
        assert_eq!(state.phase, "thinking (42s): Analyzing build.rs");
        assert_eq!(
            state.thinking_preview,
            "Gemini Web thinking (42s): Analyzing build.rs"
        );
        // Bottom status bar does not duplicate live thinking text.
        assert!(!state.status_text(120).contains("thinking"));
        // Transcript itself stays clean (no duplicate "○ Gemini Web thinking" row).
        let preview_lines: String = state
            .transcript_lines(100)
            .into_iter()
            .flat_map(|line| line.into_iter().map(|s| s.text))
            .collect();
        assert!(!preview_lines.contains("Gemini Web thinking"));

        state.update(Update::Event(crate::domain::Event {
            sequence: 2,
            run_id: "job-think".into(),
            kind: "model_chunk".into(),
            data: json!({"text": "Answer starts"}),
            at: 0,
        }));
        assert!(state.thinking_preview.is_empty());
        let stream_lines: String = state
            .transcript_lines(100)
            .into_iter()
            .flat_map(|line| line.into_iter().map(|s| s.text))
            .collect();
        assert!(!stream_lines.contains("Gemini Web thinking (42s)"));
        assert!(stream_lines.contains("Answer starts"));
    }

    #[test]
    fn live_thinking_line_appears_above_prompt_in_rendered_frame() {
        let mut state = State::new(PromptOptions::default());
        state.update(Update::Started("job-live-think".into()));
        state.update(Update::Event(crate::domain::Event {
            sequence: 1,
            run_id: "job-live-think".into(),
            kind: "browser_stage".into(),
            data: json!({
                "stage": "thinking",
                "detail": "Gemini Web thinking (7s): Inspecting Cargo dependencies",
                "summary": "Inspecting Cargo dependencies",
                "elapsed_secs": 7
            }),
            at: 0,
        }));
        let mut frame = Vec::new();
        state.render_frame(&mut frame, 100, 24).unwrap();
        let rendered = String::from_utf8(frame).unwrap();
        let matches = rendered
            .matches("thinking (7s): Inspecting Cargo dependencies")
            .count();
        assert_eq!(
            matches, 1,
            "expected thinking text exactly once (above prompt), count={matches}"
        );
        assert!(rendered.contains("thinking (7s): Inspecting Cargo dependencies · Esc: interrupt"));
    }

    #[test]
    fn selection_render_uses_in_place_updates_without_full_screen_clear() {
        let mut state = State::new(PromptOptions::default());
        state.log("alpha beta gamma\nsecond line");

        let mut normal_frame = Vec::new();
        state.render_frame(&mut normal_frame, 80, 26).unwrap();
        let normal_str = String::from_utf8(normal_frame).unwrap();
        assert!(normal_str.contains("\x1b[1;1H\x1b[J"));
        assert!(!normal_str.contains("\x1b[2J"));

        state.transcript_mouse(event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        state.transcript_mouse(event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(state.selection.is_some());

        let mut selection_frame = Vec::new();
        state.render_frame(&mut selection_frame, 80, 26).unwrap();
        let sel_str = String::from_utf8(selection_frame).unwrap();
        assert!(
            !sel_str.contains("\x1b[J") && !sel_str.contains("\x1b[2J"),
            "active selection frame must not wipe entire screen"
        );
        assert!(sel_str.contains("\x1b[K"));
    }

    #[test]
    fn prompt_entry_highlights_bare_double_and_single_quoted_mentions() {
        let entry = Entry::Prompt(
            "inspect @bare.rs and @\"double spaced.rs\" and @'single spaced.rs'".into(),
        );
        let lines = entry.render(120);
        let spans: Vec<_> = lines.into_iter().flatten().collect();
        for expected in ["@bare.rs", "@\"double spaced.rs\"", "@'single spaced.rs'"] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.text == expected && span.face.accent),
                "expected accent span for {expected}, got {:?}",
                spans
            );
        }
    }

    #[test]
    fn theme_menu_previews_on_move_and_restores_on_cancel_or_saves_on_enter() {
        let dir = tempfile::tempdir().unwrap();
        let (mut tx, _) = mpsc::sync_channel(4);
        let mut state = State::new(PromptOptions {
            workspace: dir.path().into(),
            account: "default".into(),
            ..Default::default()
        });
        state.preferences = Some(Preferences::open(dir.path()).unwrap());
        assert_eq!(state.theme.id, "terminal");

        // Opening `/theme` and moving down (or wheel) previews immediately without saving or logging.
        state.editor.set("/theme".into());
        assert!(!state.submit(&mut tx));
        assert!(state.menu.is_some());

        state.move_menu(false);
        assert_eq!(state.theme.id, "busan-night");
        state.scroll_wheel(false, 100, 26);
        assert_eq!(state.theme.id, "monokai");
        assert_eq!(state.preferences.as_ref().unwrap().theme(), "terminal");
        assert!(!state.logs().contains("Theme:"));

        // Esc restores original theme without writing preferences.
        state.close_menu();
        assert!(state.menu.is_none());
        assert_eq!(state.theme.id, "terminal");
        assert_eq!(state.preferences.as_ref().unwrap().theme(), "terminal");

        // Re-opening, previewing, and pressing Enter persists and logs the chosen theme.
        state.editor.set("/theme".into());
        assert!(!state.submit(&mut tx));
        state.move_menu(false);
        state.move_menu(false);
        assert_eq!(state.theme.id, "monokai");
        state.select(&mut tx);
        assert_eq!(state.theme.id, "monokai");
        assert_eq!(state.preferences.as_ref().unwrap().theme(), "monokai");
        assert!(state.logs().contains("Theme: Monokai"));
    }

    #[test]
    fn theme_menu_renders_split_preview_on_wide_terminals() {
        let (mut tx, _) = mpsc::sync_channel(4);
        let mut state = State::new(PromptOptions::default());
        state.editor.set("/theme".into());
        assert!(!state.submit(&mut tx));
        state.move_menu(false);
        assert_eq!(state.theme.id, "busan-night");

        let mut wide = Vec::new();
        state.render_frame(&mut wide, 110, 26).unwrap();
        let wide_str = String::from_utf8(wide).unwrap();
        assert!(wide_str.contains("› Busan Night"));
        assert!(wide_str.contains("│"));
        assert!(wide_str.contains("Preview ·"));
        assert!(wide_str.contains("(busan-night)"));
        assert!(wide_str.contains("summarize"));
        assert!(wide_str.contains("is_active()"));
        // Busan Night uses the terminal background with its highlight, danger, and code colors.
        assert!(wide_str.contains(&format!(
            "{}",
            crossterm::style::SetBackgroundColor(crossterm::style::Color::Reset)
        )));
        assert!(wide_str.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 158,
                g: 214,
                b: 111
            })
        )));
        assert!(wide_str.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 245,
                g: 140,
                b: 155
            })
        )));
        assert!(wide_str.contains(&format!(
            "{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 94,
                g: 219,
                b: 196
            })
        )));

        // Narrow terminal omits the side preview and keeps the single-column picker intact.
        let mut narrow = Vec::new();
        state.render_frame(&mut narrow, 48, 24).unwrap();
        let narrow_str = String::from_utf8(narrow).unwrap();
        assert!(narrow_str.contains("› Busan Night"));
        assert!(!narrow_str.contains("Preview ·"));
        assert!(!narrow_str.contains("fn summarize"));
    }

    #[test]
    fn startup_banner_displays_version_commit_head_and_logo() {
        let mut state = State::new(PromptOptions {
            workspace: std::path::PathBuf::from("/tmp/fixture-proj"),
            account: "default".into(),
            ..Default::default()
        });
        state.log_startup_banner();

        let expected_version = env!("CARGO_PKG_VERSION");
        assert!(!expected_version.is_empty());
        let commit = crate::cli::commit_head().expect("git HEAD short hash in repo");
        let sha = commit.strip_suffix("-dirty").unwrap_or(&commit);
        assert_eq!(sha.len(), 7);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));

        let mut frame = Vec::new();
        state.render_frame(&mut frame, 100, 24).unwrap();
        let rendered = String::from_utf8(frame).unwrap();
        assert!(rendered.contains("▄██████▄"));
        assert!(rendered.contains("▀████▀"));
        assert!(rendered.contains(expected_version));
        assert!(rendered.contains(&format!("({commit})")));
        assert!(rendered.contains("Gemini Web · default"));
        assert!(rendered.contains("/tmp/fixture-proj"));
    }

    #[test]
    fn provider_command_switches_account_resets_conversation_and_blocks_while_busy() {
        let dir = tempfile::tempdir().unwrap();
        let (mut tx, rx) = mpsc::sync_channel(4);
        let mut state = State::new(PromptOptions {
            workspace: dir.path().into(),
            account: "default".into(),
            ..Default::default()
        });
        state.preferences = Some(Preferences::open(dir.path()).unwrap());
        state.parent = Some("conv-gemini".into());
        state.active_model = Some("Argon High".into());

        // Without cached status, `/provider` queues a status fetch.
        state.editor.set("/provider".into());
        assert!(!state.submit(&mut tx));
        assert!(matches!(rx.try_recv().unwrap(), Command::Providers(None)));
        state.update(Update::Providers(
            json!({
                "providers": [
                    {"id": "gemini-web", "name": "Gemini Web"},
                    {"id": "claude-web", "name": "Claude Web"},
                    {"id": "chatgpt-web", "name": "ChatGPT Web"}
                ],
                "accounts": [
                    {"id": "default", "provider": "gemini-web", "state": "ready", "selected_model": "Argon High", "models": [{"mode_id": "argon"}]},
                    {"id": "claude", "provider": "claude-web", "state": "ready", "selected_model": "Sonnet 5.5", "models": [{"mode_id": "Sonnet 5.5"}]}
                ]
            }),
            None,
        ));
        assert!(!state.busy());
        let menu = state.menu.as_mut().expect("provider menu open");
        assert_eq!(menu.choices.len(), 3);
        assert!(menu.choices[2].label.contains("setup & sign in"));
        assert_eq!(menu.selected, 0);
        menu.selected = 1;
        state.select(&mut tx);

        assert_eq!(state.options.account, "claude");
        assert_eq!(state.options.provider.as_deref(), Some("claude-web"));
        assert_eq!(state.provider_name(), "Claude Web");
        assert!(state.parent.is_none());
        assert!(state.active_model.is_none());
        assert_eq!(state.catalog["models"][0]["mode_id"], "Sonnet 5.5");
        assert!(state.status_text(120).contains("claude-web/claude:ready"));

        // Direct `/provider chatgpt-web` on an unconfigured provider triggers automatic account + login setup.
        state.editor.set("/provider chatgpt-web".into());
        assert!(!state.submit(&mut tx));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Command::SetupProvider { provider_id, .. } if provider_id == "chatgpt-web"
        ));
        state.update(Update::ProviderSetup {
            status: json!({
                "providers": [
                    {"id": "gemini-web", "name": "Gemini Web"},
                    {"id": "claude-web", "name": "Claude Web"},
                    {"id": "chatgpt-web", "name": "ChatGPT Web"}
                ],
                "accounts": [
                    {"id": "default", "provider": "gemini-web", "state": "ready", "selected_model": "Argon High", "models": [{"mode_id": "argon"}]},
                    {"id": "claude", "provider": "claude-web", "state": "ready", "selected_model": "Sonnet 5.5", "models": [{"mode_id": "Sonnet 5.5"}]},
                    {"id": "chatgpt", "provider": "chatgpt-web", "state": "auth_required", "login": {"state": "waiting", "error": ""}}
                ]
            }),
            account_id: "chatgpt".into(),
            provider_id: "chatgpt-web".into(),
            provider_name: "ChatGPT Web".into(),
            login_error: None,
        });
        assert_eq!(state.options.account, "chatgpt");
        assert_eq!(state.provider_id(), "chatgpt-web");

        // Direct `/provider default` switches back by account ID.
        state.editor.set("/provider default".into());
        assert!(!state.submit(&mut tx));
        assert_eq!(state.options.account, "default");
        assert_eq!(state.provider_id(), "gemini-web");

        // Switching while a run is active is rejected.
        state.active = Some("running-job".into());
        state.editor.set("/provider claude-web".into());
        assert!(!state.submit(&mut tx));
        assert_eq!(state.options.account, "default");
        assert!(state.logs().contains("Interrupt the current task"));
    }

    #[test]
    fn provider_command_rejects_switching_to_account_with_unregistered_provider() {
        let dir = tempfile::tempdir().unwrap();
        let (mut tx, _rx) = mpsc::sync_channel(4);
        let mut state = State::new(PromptOptions {
            workspace: dir.path().into(),
            account: "default".into(),
            ..Default::default()
        });
        state.parent = Some("conv-gemini".into());
        state.update(Update::Status(json!({
            "providers": [
                {"id": "gemini-web", "name": "Gemini Web"}
            ],
            "accounts": [
                {"id": "default", "provider": "gemini-web", "state": "ready"},
                {"id": "claude", "provider": "claude-web", "state": "ready"}
            ]
        })));

        // Menu marks unregistered provider accounts cleanly and refuses switching.
        state.editor.set("/provider".into());
        assert!(!state.submit(&mut tx));
        let menu = state.menu.as_mut().expect("provider menu open");
        assert!(menu.choices[1].label.contains("unregistered"));
        menu.selected = 1;
        state.select(&mut tx);
        assert_eq!(state.options.account, "default");
        assert_eq!(state.parent.as_deref(), Some("conv-gemini"));
        assert!(state.logs().contains("is not registered"));
        assert!(state.logs().contains("anticrumb providers add"));

        // Direct `/provider claude` or `/provider claude-web` also blocks immediately.
        state.editor.set("/provider claude".into());
        assert!(!state.submit(&mut tx));
        assert_eq!(state.options.account, "default");
    }

    #[test]
    fn usage_command_runs_during_active_turn_without_clearing_phase_or_pending() {
        let dir = tempfile::tempdir().unwrap();
        let (mut tx, rx) = mpsc::sync_channel(4);
        let mut state = State::new(PromptOptions {
            workspace: dir.path().into(),
            account: "claude".into(),
            ..Default::default()
        });
        state.active = Some("agent_turn_1".into());
        state.phase = "generating".into();
        state.pending = Some((editor::Draft::plain("queued follow up".into()), vec![]));

        state.editor.set("/usage".into());
        assert!(!state.submit(&mut tx));
        assert!(
            matches!(rx.try_recv().unwrap(), Command::WebUsage(account) if account == "claude")
        );
        assert_eq!(state.phase, "generating");
        assert!(state.logs().contains("Checking web account usage limits"));

        // Usage failure during an active turn must keep active phase & pending draft intact.
        state.update(Update::WebUsageError("temporary scrape error".into()));
        assert_eq!(state.phase, "generating");
        assert!(state.pending.is_some());
        assert!(state.web_usage_stale);
        assert!(state.logs().contains("! temporary scrape error"));

        // Successful usage update during an active turn keeps active phase & pending draft intact.
        state.update(Update::WebUsage(json!({
            "account_id": "claude",
            "checked_at": 1000,
            "windows": [
                {"label": "Session", "percent_used": 20.0, "resets_at": "15:00"}
            ]
        })));
        assert_eq!(state.phase, "generating");
        assert!(state.pending.is_some());
        assert!(!state.web_usage_stale);
        assert!(state.web_usage.is_some());

        // Still blocks during bulk history cleanup or active sign-in.
        state.clearing_web_history = true;
        state.editor.set("/usage".into());
        assert!(!state.submit(&mut tx));
        assert!(rx.try_recv().is_err());
        assert!(state
            .logs()
            .contains("Finish sign-in or cleanup before checking web account usage."));
    }

    #[test]
    fn safe_row_width_prevents_terminal_autowrap_scroll() {
        // Bottom row (row + 1 >= height) must reserve the final cell (width - 1)
        // to prevent autowrap scroll-up on Windows Console and others.
        assert_eq!(State::safe_row_width(25, 80, 26), 79);
        assert_eq!(State::safe_row_width(5, 100, 6), 99);

        // Non-bottom rows can occupy the full width.
        assert_eq!(State::safe_row_width(0, 80, 26), 80);
        assert_eq!(State::safe_row_width(24, 80, 26), 80);
        assert_eq!(State::safe_row_width(4, 100, 6), 100);
    }

    #[test]
    fn render_frame_footer_renders_without_bottom_right_cell_overflow() {
        let mut state = State::new(PromptOptions::default());
        let mut out = Vec::new();
        state.render_frame(&mut out, 80, 26).expect("render_frame");
        let rendered = String::from_utf8_lossy(&out);

        // 1. ClearType::UntilNewLine (ESC[K or ESC[0K) must be emitted to clear
        //    the trailing cell(s) cleanly without causing autowrap scrolling.
        assert!(
            rendered.contains("\x1b[K") || rendered.contains("\x1b[0K"),
            "expected ClearType::UntilNewLine escape sequence in rendered output"
        );

        // 2. Locate footer output starting at MoveTo(0, 25), which is 1-based ESC[26;1H.
        let footer_pos = rendered
            .find("\x1b[26;1H")
            .expect("footer cursor position ESC[26;1H");
        let footer_output = &rendered[footer_pos..];

        // Ensure footer contains the Clear(UntilNewLine) sequence to pad the row end.
        assert!(
            footer_output.contains("\x1b[K") || footer_output.contains("\x1b[0K"),
            "footer must use ClearType::UntilNewLine"
        );

        // Strip ANSI escape sequences from footer output to verify visible character count.
        // The bottom row (safe_w = 79) must never print into column 80.
        let mut in_escape = false;
        let mut printable = String::new();
        for ch in footer_output.chars() {
            if ch == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if ch.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else {
                printable.push(ch);
            }
        }
        assert!(
            unicode_width::UnicodeWidthStr::width(printable.as_str()) < 80,
            "footer printable width must be strictly less than 80 columns, got: {}",
            unicode_width::UnicodeWidthStr::width(printable.as_str())
        );
    }

    #[test]
    fn drag_selection_autoscrolls_transcript_past_edges_and_survives_live_output() {
        let mouse = |kind, column, row| event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let mut state = State::new(PromptOptions::default());
        for i in 1..=35 {
            state.log(&format!("row-{i:02}-alpha beta"));
        }
        state.scroll = 15;
        let mut out = Vec::new();
        state.render_frame(&mut out, 80, 16).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        let vp = usize::from(layout.transcript.len);
        assert!(vp >= 10);

        // Anchor on the first visible line and drag below the transcript (into footer row).
        let first_visible = state.painted_transcript_start;
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            layout.transcript.start,
        ));
        assert_eq!(state.selection_target, SelectionTarget::Transcript);

        // Dragging >= 2 rows past transcript end scrolls down by 2 lines immediately.
        let scroll_before = state.scroll;
        state.transcript_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            10,
            layout.footer.start,
        ));
        assert_eq!(state.scroll, scroll_before - 2);

        // Live output/streaming arriving mid-drag does not alter the snapshotted selection.
        state.log("row-36-live-arrival");
        state.stream = "streaming mid-drag".into();
        state.render_frame(&mut out, 80, 16).expect("render_frame");

        // Wheel down while dragging scrolls further and extends selection cleanly.
        state.scroll_wheel_at(false, 10, layout.footer.start, 80, 16);
        assert_eq!(state.scroll, scroll_before - 5);

        // Timer tick autoscroll after >50ms interval continues scrolling down.
        state.selection_drag.as_mut().unwrap().last_autoscroll =
            Instant::now() - Duration::from_millis(80);
        assert!(state.tick_selection_autoscroll());
        assert_eq!(state.scroll, scroll_before - 7);

        let copied = state
            .transcript_mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                10,
                layout.footer.start,
            ))
            .expect("copied multi-scroll selection");
        let copied_lines: Vec<&str> = copied.lines().collect();
        assert_eq!(copied_lines.len(), vp + (scroll_before - state.scroll));
        assert!(copied.contains(&format!("row-{:02}-alpha", first_visible + 1)));
        assert!(!copied.contains("row-36-live-arrival"));

        // Default layout (show_header = false): horizontal drag along row 0 does not scroll;
        // dwelling or repeating identical Drag(col, 0) scrolls up across earlier history.
        state.scroll = 4;
        state.render_frame(&mut out, 80, 16).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        assert_eq!(layout.transcript.start, 0);
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 3));
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5, 0));
        assert_eq!(state.scroll, 4, "arriving on row 0 must not jump scroll");
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 8, 0));
        assert_eq!(
            state.scroll, 4,
            "horizontal drag across row 0 must not scroll"
        );
        // Repeating the exact same top-edge cell scrolls up 1 line immediately.
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 8, 0));
        assert_eq!(state.scroll, 5);
        // Dwell at row 0 also triggers upward autoscroll on timer tick.
        if let Some(drag) = state.selection_drag.as_mut() {
            drag.entered_top_at = Some(Instant::now() - Duration::from_millis(220));
            drag.last_autoscroll = Instant::now() - Duration::from_millis(80);
        }
        assert!(state.tick_selection_autoscroll());
        assert_eq!(state.scroll, 6);
        let top_copied = state
            .transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 0))
            .expect("upward scrolled selection");
        assert_eq!(top_copied.lines().count(), 4 + (6 - 4));

        // When show_header = true (transcript.start == 1), dragging onto row 0 scrolls up immediately.
        state.layout.show_header = true;
        state.scroll = 4;
        state.render_frame(&mut out, 80, 16).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        assert_eq!(layout.transcript.start, 1);
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 3));
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0));
        assert_eq!(state.scroll, 5);
        state.selection_drag.as_mut().unwrap().last_autoscroll =
            Instant::now() - Duration::from_millis(80);
        assert!(state.tick_selection_autoscroll());
        assert_eq!(state.scroll, 6);
        state.transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 0));
    }

    #[test]
    fn lower_ui_regions_are_independently_selectable_without_prompt_prefix() {
        let mouse = |kind, column, row| event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let mut state = State::new(PromptOptions::default());
        state.log("Transcript line one");
        state.editor.set("prompt alpha\nprompt beta".into());
        state.subagents.push(SubagentView {
            id: "sub-1".into(),
            label: "worker".into(),
            phase: "working".into(),
            detail: "reading files".into(),
            started: Instant::now(),
        });
        let mut out = Vec::new();
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        assert_eq!(layout.composer.len, 2);
        assert_eq!(layout.subagents.len, 1);
        assert_eq!(layout.hint.len, 1);
        assert_eq!(layout.footer.len, 1);

        // 1. Composer multi-line selection strips "> " / "  " gutter and clamps on vertical drift.
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            layout.composer.start,
        ));
        assert_eq!(state.selection_target, SelectionTarget::Composer);
        // Drag down past composer into footer: clamps strictly inside composer band.
        state.transcript_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            50,
            layout.footer.start,
        ));
        let mut sel_frame = Vec::new();
        state
            .render_frame(&mut sel_frame, 100, 20)
            .expect("render_frame during composer selection");
        let sel_rendered = String::from_utf8_lossy(&sel_frame);
        let expect_col_3 = format!("\x1b[{};3H", layout.composer.start + 1);
        assert!(
            sel_rendered.contains(&expect_col_3),
            "composer highlight must start after the 2-cell gutter"
        );
        let copied_composer = state
            .transcript_mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                50,
                layout.footer.start,
            ))
            .expect("copied composer text");
        assert_eq!(copied_composer, "prompt alpha\nprompt beta");

        // Double-click inside composer selects the word under the cursor without gutter offset bugs.
        // Column 2 is 'p' of "prompt", column 9 is 'a' of "alpha".
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            9,
            layout.composer.start,
        ));
        state.transcript_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            9,
            layout.composer.start,
        ));
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            9,
            layout.composer.start,
        ));
        assert_eq!(
            state
                .transcript_mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    9,
                    layout.composer.start,
                ))
                .as_deref(),
            Some("alpha")
        );

        // 2. Footer (status bar) selection copies status text and clamps if dragged upward.
        let expected_footer: String = state.painted_footer[0]
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            layout.footer.start,
        ));
        assert_eq!(state.selection_target, SelectionTarget::Footer);
        state.transcript_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            12,
            layout.footer.start,
        ));
        let copied_footer = state
            .transcript_mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                12,
                layout.footer.start,
            ))
            .expect("copied footer slice");
        let expected_slice: String = expected_footer.chars().take(13).collect();
        assert_eq!(copied_footer, expected_slice);

        // 3. Hint selection preserves hint text during drag rather than overwriting with "Drag to select".
        state.clipboard_notice = None;
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        let expected_hint: String = state.painted_hint[0]
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            1,
            layout.hint.start,
        ));
        assert_eq!(state.selection_target, SelectionTarget::Hint);
        state.transcript_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            9,
            layout.hint.start,
        ));
        let mut hint_frame = Vec::new();
        state
            .render_frame(&mut hint_frame, 100, 20)
            .expect("render_frame during hint selection");
        assert!(!String::from_utf8_lossy(&hint_frame).contains("Drag to select"));
        let copied_hint = state
            .transcript_mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                9,
                layout.hint.start,
            ))
            .expect("copied hint text");
        let expected_hint_slice: String = expected_hint.chars().skip(1).take(9).collect();
        assert_eq!(copied_hint, expected_hint_slice);

        // 4. Subagents banner is also independently selectable.
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            layout.subagents.start,
        ));
        assert_eq!(state.selection_target, SelectionTarget::Subagents);
        state.transcript_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            40,
            layout.subagents.start,
        ));
        let copied_subagent = state
            .transcript_mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                40,
                layout.subagents.start,
            ))
            .expect("copied subagent row");
        assert!(copied_subagent.contains("worker"));

        // 5. Empty composer ignores clicks so accidental clicks on empty prompt don't select.
        state.editor.set(String::new());
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            layout.composer.start,
        ));
        assert!(state.selection.is_none());

        // 6. Live transcript updates while a non-transcript region is selected still sync painted_all_transcript.
        state.editor.set("keep composer".into());
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            2,
            layout.composer.start,
        ));
        assert_eq!(state.selection_target, SelectionTarget::Composer);
        for i in 1..=30 {
            state.log(&format!("live-after-composer-{i:02}"));
        }
        state
            .render_frame(&mut out, 100, 20)
            .expect("render_frame with live transcript during composer selection");
        assert_eq!(
            state.painted_all_transcript.len(),
            state.painted_transcript_start + state.painted_transcript.len()
        );
        // Release composer selection and immediately click last transcript row without an intervening render.
        state.transcript_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            2,
            layout.composer.start,
        ));
        let last_row = layout.transcript.end() - 1;
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, last_row));
        assert_eq!(state.selection_target, SelectionTarget::Transcript);
        state.transcript_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 30, last_row));
        let copied_live = state
            .transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 30, last_row))
            .expect("copied newly arrived transcript row");
        assert!(copied_live.contains("live-after-composer-30"));
    }

    #[test]
    fn scroll_bottom_button_appears_centered_when_scrolled_and_jumps_to_bottom_on_click() {
        let mouse = |kind, column, row| event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let mut state = State::new(PromptOptions::default());
        let mut out = Vec::new();

        // Short transcript cannot scroll even if `scroll > 0`; button must stay hidden.
        state.log("Short line");
        state.scroll = 5;
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        assert_eq!(state.painted_effective_scroll, 0);
        assert!(state.scroll_bottom_button.is_none());

        // Add enough lines to overflow viewport, then scroll up.
        for i in 1..=35 {
            state.log(&format!("scroll-row-{i:02}"));
        }
        state.scroll = 8;
        state.clipboard_notice = Some("Copied 12 characters to clipboard.".into());
        out.clear();
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        let layout = state.painted_layout.unwrap();
        assert_eq!(state.painted_effective_scroll, 8);
        let (btn_row, btn_start, btn_end) = state
            .scroll_bottom_button
            .expect("jump-to-bottom button must appear when scrolled up");
        assert_eq!(btn_row, layout.hint.start);
        assert_eq!(layout.hint.start + 1, layout.composer.start);
        let mid = (btn_start + btn_end) / 2;
        assert!(
            (45..=55).contains(&mid),
            "button should sit above center of 100-col prompt, got {btn_start}..{btn_end}"
        );
        let rendered = String::from_utf8_lossy(&out);
        assert!(rendered.contains("[ ↓ Jump to bottom ]"));
        assert!(rendered.contains("Copied 12 characters to clipboard."));

        // Clicking outside the button does not reset scroll.
        state.transcript_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            btn_start - 1,
            btn_row,
        ));
        state.transcript_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            btn_start - 1,
            btn_row,
        ));
        assert_eq!(state.scroll, 8);

        // Clicking inside the button jumps immediately to bottom (`scroll = 0`) without selecting.
        state.transcript_mouse(mouse(MouseEventKind::Down(MouseButton::Left), mid, btn_row));
        assert_eq!(state.scroll, 0);
        assert!(state.selection.is_none());
        assert!(state
            .transcript_mouse(mouse(MouseEventKind::Up(MouseButton::Left), mid, btn_row))
            .is_none());

        out.clear();
        state.render_frame(&mut out, 100, 20).expect("render_frame");
        assert_eq!(state.painted_effective_scroll, 0);
        assert!(state.scroll_bottom_button.is_none());
        assert!(!String::from_utf8_lossy(&out).contains("[ ↓ Jump to bottom ]"));
    }

    #[test]
    fn async_mention_popup_renders_below_composer_and_supports_navigation_selection_and_dismiss() {
        use super::mentions::{MentionKind, MentionMatch};

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("a b.txt"), "spaced").unwrap();

        let mut state = State::new(PromptOptions {
            workspace: dir.path().into(),
            ..Default::default()
        });
        let (mut tx, rx) = mpsc::sync_channel(8);

        state.editor.set("open @dd".into());
        state.refresh_mention(&mut tx);
        assert!(state.mention_loading());
        let Command::SearchMentions { id, query, .. } = rx.try_recv().unwrap() else {
            panic!("expected Command::SearchMentions");
        };
        assert_eq!(query, "dd");

        // While searching, the loading spinner and cancel hint render below the composer
        // with the full fixed 8-row completion height so the prompt row never shifts.
        let mut out = Vec::new();
        state.render_frame(&mut out, 100, 26).unwrap();
        let loading_layout = state.painted_layout.unwrap();
        assert_eq!(loading_layout.completion.len, 8);
        assert!(loading_layout.completion.start >= loading_layout.composer.end());
        let loading_screen = String::from_utf8_lossy(&out);
        assert!(loading_screen.contains("Fetching matches for \"dd\"..."));
        assert!(loading_screen.contains("esc Cancel"));

        // Stale response with old/wrong ID is ignored.
        state.update(Update::MentionMatches {
            id: id + 99,
            query: "dd".into(),
            matches: vec![],
            total: 0,
        });
        assert!(state.mention_loading());

        // Deliver 7 matches -> 5 visible rows + overflow indicator ("↓ 2 more") + footer hint.
        let matches: Vec<MentionMatch> = (0..7)
            .map(|i| MentionMatch {
                display: if i == 1 {
                    "src/".into()
                } else {
                    format!("file_{i}_dd.rs")
                },
                kind: if i == 1 {
                    MentionKind::Dir
                } else {
                    MentionKind::File
                },
                detail: dir
                    .path()
                    .join(format!("file_{i}_dd.rs"))
                    .display()
                    .to_string(),
                score: 100 - i as i64,
            })
            .collect();
        state.update(Update::MentionMatches {
            id,
            query: "dd".into(),
            matches,
            total: 7,
        });
        assert!(!state.mention_loading());

        out.clear();
        state.render_frame(&mut out, 100, 26).unwrap();
        let results_layout = state.painted_layout.unwrap();
        assert_eq!(results_layout.completion.len, 8);
        assert_eq!(results_layout.composer, loading_layout.composer);
        assert!(results_layout.completion.start >= results_layout.composer.end());
        let rendered = String::from_utf8_lossy(&out);
        assert!(rendered.contains("file_0_dd.rs"));
        assert!(rendered.contains("↓ 2 more"));
        assert!(rendered.contains("↑/↓ Navigate · enter Select · tab Complete · esc Cancel"));

        // Navigate down to `src/` (Dir) and select -> inserts `@src/` and triggers search inside `src/`.
        state.move_mention(false);
        assert_eq!(state.mention.as_ref().unwrap().selected, 1);
        assert!(state.select_mention(&mut tx));
        assert_eq!(state.editor.text, "open @src/");
        let Command::SearchMentions {
            query: sub_query, ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected directory drill-down SearchMentions");
        };
        assert_eq!(sub_query, "src/");

        // Esc dismisses and stays dismissed for this token offset.
        state.dismiss_mention();
        assert!(state.mention.is_none());
        state.refresh_mention(&mut tx);
        assert!(state.mention.is_none());
        assert!(rx.try_recv().is_err());

        // Burst Tab while loading falls back synchronously so `@a\t` works deterministically.
        state.editor.set("read @a".into());
        state.refresh_mention(&mut tx);
        let _ = rx.try_recv();
        assert!(state.mention_loading());
        assert!(state.tab_complete_mention(&mut tx));
        assert_eq!(state.editor.text, "read @\"a b.txt\" ");
        assert!(state.mention.is_none());

        // Wheel inside completion band moves selection without changing transcript scroll;
        // clicking a match row inserts it.
        state.editor.set("@".into());
        state.refresh_mention(&mut tx);
        let Command::SearchMentions { id: id3, .. } = rx.try_recv().unwrap() else {
            panic!("expected SearchMentions");
        };
        state.update(Update::MentionMatches {
            id: id3,
            query: "".into(),
            matches: vec![
                MentionMatch {
                    display: "first.rs".into(),
                    kind: MentionKind::File,
                    detail: dir.path().join("first.rs").display().to_string(),
                    score: 100,
                },
                MentionMatch {
                    display: "second.rs".into(),
                    kind: MentionKind::File,
                    detail: dir.path().join("second.rs").display().to_string(),
                    score: 90,
                },
            ],
            total: 2,
        });
        state.render_frame(&mut out, 100, 26).unwrap();
        let layout3 = state.painted_layout.unwrap();
        assert_eq!(layout3.completion.len, 8);
        assert_eq!(layout3.composer, results_layout.composer);
        state.scroll_wheel_at(false, 10, layout3.completion.start + 1, 100, 26);
        assert_eq!(state.mention.as_ref().unwrap().selected, 1);
        assert_eq!(state.scroll, 0);

        // Clicking an empty padded row inside the fixed completion band does not select an item.
        let empty_click = event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: layout3.completion.start + 4,
            modifiers: KeyModifiers::NONE,
        };
        assert!(state.completion_mouse(empty_click, &mut tx));
        assert_eq!(state.editor.text, "@");
        assert!(state.mention.is_some());

        let click = event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: layout3.completion.start + 2,
            modifiers: KeyModifiers::NONE,
        };
        assert!(state.completion_mouse(click, &mut tx));
        assert_eq!(state.editor.text, "@second.rs ");
        assert!(state.mention.is_none());

        // Git commits without a subject keep empty `detail` for search and display "Git commit" on render.
        state.editor.set("@git:".into());
        state.refresh_mention(&mut tx);
        let Command::SearchMentions { id: id4, .. } = rx.try_recv().unwrap() else {
            panic!("expected SearchMentions");
        };
        state.update(Update::MentionMatches {
            id: id4,
            query: "git:".into(),
            matches: vec![MentionMatch {
                display: "git:abc1234".into(),
                kind: MentionKind::Git,
                detail: String::new(),
                score: 100_000,
            }],
            total: 1,
        });
        out.clear();
        state.render_frame(&mut out, 100, 26).unwrap();
        let git_rendered = String::from_utf8_lossy(&out);
        assert!(git_rendered.contains("git:abc1234"));
        assert!(git_rendered.contains("Git commit"));
    }

    #[test]
    fn chat_mode_is_default_and_mode_command_switches_to_agent_yolo_with_full_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(PromptOptions {
            workspace: dir.path().to_path_buf(),
            account: "default".into(),
            ..Default::default()
        });
        state.preferences = Some(Preferences::open(dir.path()).unwrap());
        state.load_model_preference();
        assert!(!state.options.agent);
        assert!(!state.options.yolo);
        state.log_startup_banner();
        assert!(state.logs().contains("Mode: Chat"));
        assert!(state.status_text(120).contains(" · CHAT · "));

        // 1. Default permissions are read-only (`read` only). Open `/permissions`, toggle `edit` ON, and save.
        assert_eq!(
            state.options.effective_permissions(),
            crate::domain::ToolPermissions::read_only()
        );
        let (mut tx, rx) = mpsc::sync_channel(4);
        state.editor.set("/permissions".into());
        state.submit(&mut tx);
        assert_eq!(state.menu.as_ref().unwrap().choices.len(), 5);
        assert!(state.menu.as_ref().unwrap().choices[0]
            .label
            .starts_with("[x] read"));
        assert!(state.menu.as_ref().unwrap().choices[1]
            .label
            .starts_with("[ ] edit"));
        state.move_menu(false); // move to `edit` (index 1)
        assert!(state.toggle_permission_menu_item()); // toggle `edit` ON
        assert!(state.menu.as_ref().unwrap().choices[1]
            .label
            .starts_with("[x] edit"));
        state.select(&mut tx); // Enter saves permissions (read + edit, shell still disabled)
        assert!(state.options.no_shell);
        assert_eq!(
            state.options.effective_permissions(),
            crate::domain::ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: false,
                subagent: false,
            }
        );

        // 2. Restart in `anticrumb --agent` mode: saved /permissions (read + edit, shell disabled) is respected!
        let mut agent_state = State::new(PromptOptions {
            workspace: dir.path().to_path_buf(),
            account: "default".into(),
            agent: true,
            yolo: false,
            ..Default::default()
        });
        agent_state.preferences = Some(Preferences::open(dir.path()).unwrap());
        agent_state.load_model_preference();
        assert!(agent_state.options.agent);
        assert!(!agent_state.options.yolo);
        assert!(
            agent_state.options.no_shell,
            "anticrumb --agent must respect saved /permissions (no_shell = true)"
        );
        assert_eq!(
            agent_state.options.effective_permissions(),
            crate::domain::ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: false,
                subagent: false,
            }
        );
        assert!(agent_state.status_text(120).contains(" · AGENT · "));
        agent_state.editor.set("inspect files without shell".into());
        agent_state.submit(&mut tx);
        let Command::Start(agent_req, _, _) = rx.try_recv().unwrap() else {
            panic!("expected Start command");
        };
        assert!(!agent_req.chat_only);
        assert!(
            agent_req.no_shell,
            "run_shell must be disabled under --agent when /permissions disabled shell"
        );
        assert_eq!(
            agent_req.permissions,
            Some(crate::domain::ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: false,
                subagent: false,
            })
        );

        // 3. Open interactive /mode menu and select Agent (--yolo) -> all permissions enabled.
        state.editor.set("/mode".into());
        state.submit(&mut tx);
        let menu = state.menu.as_mut().expect("/mode menu opened");
        assert_eq!(menu.selected, 0);
        menu.selected = 2; // Agent (--yolo)
        state.select(&mut tx);
        assert!(state.options.agent);
        assert!(state.options.yolo);
        assert!(!state.options.no_shell, "--yolo enables all permissions");
        assert_eq!(
            state.options.effective_permissions(),
            crate::domain::ToolPermissions::all()
        );
        assert!(state.status_text(120).contains(" · YOLO · "));

        state.editor.set("run agent task".into());
        state.submit(&mut tx);
        let Command::Start(req2, _, _) = rx.try_recv().unwrap() else {
            panic!("expected Start command");
        };
        assert!(!req2.chat_only);
        assert!(!req2.no_shell);
        assert_eq!(
            req2.permissions,
            Some(crate::domain::ToolPermissions::all())
        );
        state.active = None;
        state.pending = None;
        state.phase = "ready".into();

        // 4. Direct `/mode agent` switches from YOLO to standard Agent mode and restores saved /permissions (read + edit).
        state.editor.set("/mode agent".into());
        state.submit(&mut tx);
        assert!(state.options.agent);
        assert!(!state.options.yolo);
        assert!(state.options.no_shell);
        assert_eq!(
            state.options.effective_permissions().summary(),
            "read, edit"
        );
        assert!(state.status_text(120).contains(" · AGENT · "));

        // 5. Shorthand `/permissions readonly` resets back to safe read-only default.
        state.editor.set("/permissions readonly".into());
        state.submit(&mut tx);
        assert_eq!(
            state.options.effective_permissions(),
            crate::domain::ToolPermissions::read_only()
        );

        // 6. Direct `/mode chat` toggles back to Chat mode.
        state.editor.set("/mode chat".into());
        state.submit(&mut tx);
        assert!(!state.options.agent);
        assert!(!state.options.yolo);
        assert!(state.status_text(120).contains(" · CHAT · "));
    }

    #[test]
    fn status_update_skips_redraw_when_only_runtime_uptime_changes() {
        let mut state = State::new(PromptOptions {
            account: "default".into(),
            ..Default::default()
        });
        let base_status = |uptime_ms: u64, account_state: &str| {
            serde_json::json!({
                "runtime": {
                    "uptime_ms": uptime_ms,
                    "running_jobs": 0
                },
                "browser_connection": {
                    "connected": true,
                    "transport": "local"
                },
                "accounts": [{
                    "id": "default",
                    "provider": "gemini-web",
                    "state": account_state,
                    "models": []
                }]
            })
        };

        assert!(state.update(Update::Status(base_status(1000, "ready"))));
        assert!(!state.update(Update::Status(base_status(2000, "ready"))));
        assert!(state.update(Update::Status(base_status(3000, "busy"))));

        assert!(state.update(Update::StatusDisconnected));
        assert!(!state.update(Update::StatusDisconnected));
        assert!(state.update(Update::Status(base_status(4000, "busy"))));
    }

    #[test]
    fn missing_at_mention_warns_and_submits_prompt_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "hello").unwrap();
        let mut state = State::new(PromptOptions {
            workspace: dir.path().to_path_buf(),
            account: "default".into(),
            ..Default::default()
        });
        let (mut tx, rx) = mpsc::sync_channel(4);

        // 1. Standalone unresolvable @mention logs a warning and still submits literal text.
        state.editor.set("@fdsjifdsoidsf fdsijdsf".into());
        assert!(!state.submit(&mut tx));
        assert!(state.editor.text.is_empty());
        let logs = state.logs();
        assert!(logs.contains("Warning:"));
        assert!(logs.contains("cannot attach"));
        assert!(logs.contains("@fdsjifdsoidsf"));
        assert!(!logs.contains("! cannot attach"));
        let Command::Start(req, files, temp_files) = rx.try_recv().unwrap() else {
            panic!("expected Command::Start");
        };
        assert_eq!(req.task, "@fdsjifdsoidsf fdsijdsf");
        assert!(files.is_empty());
        assert!(temp_files.is_empty());

        // 2. Mixed valid file + missing @mention attaches valid file, warns on missing, preserves text.
        state.active = None;
        state.pending = None;
        state.phase = "ready".into();
        state
            .editor
            .set("Check @real.txt with @fdsjifdsoidsf".into());
        assert!(!state.submit(&mut tx));
        let Command::Start(req2, files2, _) = rx.try_recv().unwrap() else {
            panic!("expected Command::Start");
        };
        assert_eq!(
            req2.task,
            "Check [Attached file: real.txt] with @fdsjifdsoidsf"
        );
        assert_eq!(files2.len(), 1);
        assert_eq!(files2[0].file_name().unwrap(), "real.txt");

        // 3. Invalid or unresolvable @git: mentions warn and keep literal prompt text too.
        state.active = None;
        state.pending = None;
        state.phase = "ready".into();
        state.editor.set("@git:nonexistent hello".into());
        assert!(!state.submit(&mut tx));
        let git_logs = state.logs();
        assert!(
            git_logs.contains("not a git repository") || git_logs.contains("unknown git commit"),
            "unexpected git warning logs: {git_logs}"
        );
        assert!(git_logs.contains("keeping @git:nonexistent as prompt text"));
        let Command::Start(req_git, files_git, temp_git) = rx.try_recv().unwrap() else {
            panic!("expected Command::Start");
        };
        assert_eq!(req_git.task, "@git:nonexistent hello");
        assert!(files_git.is_empty());
        assert!(temp_git.is_empty());

        // 4. Steering an active turn also warns cleanly and keeps literal text.
        state.active = Some("active-run".into());
        state.pending = None;
        state.phase = "running".into();
        state.editor.set("@missing_steer keep going".into());
        assert!(!state.submit(&mut tx));
        assert!(state.logs().contains("@missing_steer"));
        let Command::Steer {
            message,
            files: steer_files,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected Command::Steer");
        };
        assert_eq!(message, "@missing_steer keep going");
        assert!(steer_files.is_empty());
    }
}
