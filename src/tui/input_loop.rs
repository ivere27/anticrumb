//! Terminal raw-mode lifecycle, effect sink, and interactive event loop.
use super::{
    clipboard,
    commands::{complete, Choice, Selection},
    layout::FrameLayout,
    platform,
    preferences::Preferences,
    reducer::{EffectSink, State},
    theme,
    worker::{worker, Command, Update},
};
use crate::{
    cli::{Client, PromptOptions},
    domain::*,
};
use crossterm::{
    cursor,
    event::{self, Event as Input, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    execute, style, terminal,
};
use std::{
    io::{self, IsTerminal, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Terminal {
    _console_guard: platform::ConsoleGuard,
    last_input_mode_refresh: Instant,
}

impl Terminal {
    fn enter() -> Result<Self> {
        let console_guard = platform::ConsoleGuard::init();
        terminal::enable_raw_mode().map_err(ServiceError::storage)?;
        let guard = Self {
            _console_guard: console_guard,
            last_input_mode_refresh: Instant::now(),
        };
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            terminal::DisableLineWrap,
            event::EnableBracketedPaste,
            event::EnableMouseCapture,
            cursor::Show
        )
        .map_err(ServiceError::storage)?;
        Ok(guard)
    }

    fn refresh_input_modes(&mut self) -> Result<()> {
        if self.last_input_mode_refresh.elapsed() < Duration::from_millis(500) {
            return Ok(());
        }
        // An emulator reset can disable mouse reporting without notifying the
        // child process. Restore input modes while idle as well as during work.
        execute!(
            io::stdout(),
            event::EnableBracketedPaste,
            event::EnableMouseCapture
        )
        .map_err(ServiceError::storage)?;
        self.last_input_mode_refresh = Instant::now();
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            terminal::EnableLineWrap,
            style::ResetColor,
            event::DisableMouseCapture,
            event::DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

struct TerminalSink<'a> {
    commands: &'a mpsc::SyncSender<Command>,
}

impl EffectSink for TerminalSink<'_> {
    fn send_command(&mut self, command: Command) -> std::result::Result<(), String> {
        self.commands.try_send(command).map_err(|e| {
            let cmd = match &e {
                mpsc::TrySendError::Full(c) | mpsc::TrySendError::Disconnected(c) => c,
            };
            crate::input::cleanup_temp_files(cmd.temp_files());
            e.to_string()
        })
    }

    fn write_terminal(&mut self, sequence: &str) -> std::result::Result<(), String> {
        let mut out = io::stdout().lock();
        out.write_all(sequence.as_bytes())
            .and_then(|_| out.flush())
            .map_err(|e| e.to_string())
    }
}

fn render_stdout(state: &mut State) -> Result<()> {
    let (width, height) = terminal::size().map_err(ServiceError::storage)?;
    let mut buf = Vec::with_capacity(8192);
    state.render_frame(&mut buf, width, height)?;
    if buf.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    out.write_all(&buf).map_err(ServiceError::storage)?;
    out.flush().map_err(ServiceError::storage)
}

fn handle_input(state: &mut State, input: Input, sink: &mut dyn EffectSink) -> Result<bool> {
    match input {
        Input::Key(key) if key.kind != event::KeyEventKind::Release => {
            let had_selection = state.selection.take().is_some();
            state.last_click = None;
            state.clipboard_notice = None;
            if had_selection && key.code == KeyCode::Esc {
                return Ok(false);
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if state.history_clear_confirmation.is_some() {
                state.history_clear_key(key, sink);
                return Ok(false);
            }
            if state.menu.is_some() {
                if state.deleting_conversation.is_some() {
                    return Ok(false);
                }
                match key.code {
                    KeyCode::Esc => state.close_menu(),
                    KeyCode::Delete => state.confirm_conversation_delete(),
                    KeyCode::Char('d')
                        if key.modifiers.is_empty() && key.kind == event::KeyEventKind::Press =>
                    {
                        state.delete_conversation_key(sink);
                    }
                    KeyCode::Char('D')
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && key.kind == event::KeyEventKind::Press =>
                    {
                        state.clear_resume_web_history();
                    }
                    KeyCode::Up => state.move_menu(true),
                    KeyCode::Down | KeyCode::Tab => state.move_menu(false),
                    KeyCode::Left | KeyCode::Right => {
                        let menu = state.menu.as_mut().unwrap();
                        if let Some(Choice {
                            value: Selection::Model(model),
                            ..
                        }) = menu.choices.get_mut(menu.selected)
                        {
                            model.shift(key.code == KeyCode::Right);
                        }
                    }
                    KeyCode::Enter => state.select(sink),
                    _ => {}
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('c') if ctrl => {
                    if let Some(id) = &state.active {
                        state.send(sink, Command::Cancel(id.clone()));
                        state.phase = "cancelling".into();
                    } else if state.pending.is_some() {
                        state.log("Submission in progress. Ctrl+D exits once queued.");
                    } else if state.editor.text.is_empty()
                        && state
                            .last_ctrl_c
                            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
                    {
                        return Ok(true);
                    } else {
                        state.saved_draft = state.editor.draft();
                        state.editor.set(String::new());
                        state.files.clear();
                        state.last_ctrl_c = Some(Instant::now());
                        state.log("Press Ctrl+C again to exit.");
                    }
                }
                KeyCode::Char('d') if ctrl => {
                    if state.editor.text.is_empty() {
                        if state.pending.is_some() {
                            state.exit_requested = true;
                            state.log("Finishing submission before stopping…");
                        } else {
                            return Ok(true);
                        }
                    } else {
                        state.editor.delete();
                    }
                }
                KeyCode::Char('a') if ctrl => state.editor.home(),
                KeyCode::Char('e') if ctrl => state.editor.end(),
                KeyCode::Char('u') if ctrl => state.editor.kill_before(),
                KeyCode::Char('k') if ctrl => state.editor.kill_after(),
                KeyCode::Char('w') if ctrl => state.editor.delete_word(),
                KeyCode::Char('v') if ctrl => match clipboard::read_text() {
                    Ok(text) => state.paste(&text),
                    Err(error) => state.log(&error),
                },
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::ALT) => {
                    state.editor.expand_paste();
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                    state.editor.word_left();
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                    state.editor.word_right();
                }
                KeyCode::Esc => {
                    if state.completion.is_some() {
                        state.completion = None;
                        return Ok(false);
                    }
                    if let Some(id) = &state.active {
                        state.send(sink, Command::Cancel(id.clone()));
                        state.phase = "cancelling".into();
                    }
                }
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                {
                    state.editor.insert("\n");
                }
                KeyCode::Char('j') if ctrl => {
                    state.editor.insert("\n");
                }
                KeyCode::Enter => {
                    if state.submit(sink) {
                        return Ok(true);
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    state.editor.insert(&c.to_string());
                }
                KeyCode::Left
                    if key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.editor.word_left();
                }
                KeyCode::Right
                    if key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.editor.word_right();
                }
                KeyCode::Left => state.editor.left(),
                KeyCode::Right => state.editor.right(),
                KeyCode::Home => state.editor.home(),
                KeyCode::End => state.editor.end(),
                KeyCode::Backspace
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                {
                    state.editor.delete_block_backward();
                }
                KeyCode::Backspace => state.editor.backspace(),
                KeyCode::Delete => state.editor.delete(),
                KeyCode::Tab => {
                    if let Some(c) = complete(&state.editor, &state.options.workspace) {
                        if let Some(value) = c.values.first() {
                            state.editor.replace(c.start..c.end, value);
                        }
                    }
                }
                KeyCode::Up | KeyCode::Down => {
                    let (width, _) = terminal::size().map_err(ServiceError::storage)?;
                    let down = key.code == KeyCode::Down;
                    if !state
                        .editor
                        .vertical(down, FrameLayout::editor_width(usize::from(width)))
                    {
                        if down {
                            state.next_input();
                        } else {
                            state.previous_input();
                        }
                    }
                }
                KeyCode::PageUp | KeyCode::PageDown => {
                    let (width, height) = terminal::size().map_err(ServiceError::storage)?;
                    state.scroll_transcript(
                        key.code == KeyCode::PageUp,
                        10,
                        width.into(),
                        height.into(),
                    );
                }
                _ => {}
            }
            state.completion =
                complete(&state.editor, &state.options.workspace).filter(|c| !c.values.is_empty());
        }
        Input::Paste(text) => {
            state.selection = None;
            state.last_click = None;
            state.clipboard_notice = None;
            if let Some(confirmation) = state.history_clear_confirmation.as_mut() {
                confirmation.typed.extend(
                    text.chars()
                        .filter(|ch| !ch.is_control())
                        .take(32_usize.saturating_sub(confirmation.typed.chars().count())),
                );
            } else {
                state.paste(&text);
            }
        }
        Input::Mouse(mouse) => {
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                let (width, height) = terminal::size().map_err(ServiceError::storage)?;
                state.scroll_wheel(
                    mouse.kind == MouseEventKind::ScrollUp,
                    width.into(),
                    height.into(),
                );
            } else if let Some(text) = state.transcript_mouse(mouse) {
                state.copy_text(&text, true, sink);
            }
        }
        Input::Resize(_, _) => {
            state.selection = None;
            state.last_click = None;
            state.painted_transcript.clear();
            state.painted_layout = None;
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn run(options: PromptOptions) -> Result<()> {
    options.apply_worker_override();
    let client = Client::connect()?;
    run_with_client(options, client)
}

/// Run the terminal with an explicitly supplied service client.
pub fn run_with_client(mut options: PromptOptions, client: Client) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(ServiceError::invalid(
            "interactive mode needs a terminal; use exec or --print",
        ));
    }
    let auto_created = options.resolve_or_create_account(&client, true)?;
    struct WorkerGuard {
        client: Client,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            let _ = self.client.close();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    // Declared before channels: every early return disconnects them before joining.
    let mut worker_guard = WorkerGuard {
        client: client.clone(),
        thread: None,
    };
    let (commands, rx) = mpsc::sync_channel(32);
    let (tx, updates) = mpsc::sync_channel(128);
    worker_guard.thread = Some(std::thread::spawn(move || worker(client, rx, tx)));
    let mut terminal = Terminal::enter()?;
    let mut state = State::new(options);
    let prefs = Preferences::open(&crate::cli::root())?;
    // Resolve saved model IDs to display names before the immutable startup banner.
    state.catalog =
        crate::store::Store::open(&crate::cli::root())?.model_catalog(&state.options.account)?;
    state.theme = theme::find(&prefs.theme()).unwrap_or(theme::THEMES[0]);
    state.debug.level = prefs.debug_level();
    state.preferences = Some(prefs);
    state.load_model_preference();
    state.load_history();
    let mut sink = TerminalSink {
        commands: &commands,
    };
    state.send(
        &mut sink,
        Command::WebHistoryStatus(state.options.account.clone(), false),
    );
    state.log_startup_banner();
    state.log("Describe a task to begin. Follow-up messages stay in this conversation.\n");
    state.init_debug_session();
    if auto_created {
        state.log(&format!(
            "Created account '{}' for {} — opening Chrome sign-in…",
            state.options.account,
            state.provider_name()
        ));
        state.send(
            &mut sink,
            Command::Browser(state.options.account.clone(), "login".into()),
        );
    }
    let mut initial = !state.editor.text.is_empty();
    if let Some(id) = state.options.resume.clone() {
        state.phase = "loading".into();
        let command = if id.is_empty() {
            Command::List(
                state.options.workspace.clone(),
                state.options.account.clone(),
            )
        } else {
            Command::Load(id, state.options.clone())
        };
        state.send(&mut sink, command);
    } else if initial {
        state.submit(&mut sink);
        initial = false;
    }
    let mut dirty = true;
    let mut last_render = Instant::now();
    loop {
        terminal.refresh_input_modes()?;
        while let Ok(update) = updates.try_recv() {
            if initial && matches!(&update, Update::Error(_)) {
                initial = false;
            }
            state.update(update);
            dirty = true;
        }
        if state.exit_requested && state.pending.is_none() {
            break;
        }
        if initial
            && state.phase != "loading"
            && state.menu.is_none()
            && state.history_clear_confirmation.is_none()
        {
            state.submit(&mut sink);
            initial = false;
        }
        // Drain burst input before repainting, keeping paste/fast typing responsive on slow terminals.
        // The Unix tty reader skips queued input for a zero timeout (#839).
        let input_pending = event::poll(Duration::from_millis(1)).map_err(ServiceError::storage)?;
        if !input_pending
            && (dirty || (state.busy() && last_render.elapsed() > Duration::from_millis(120)))
        {
            render_stdout(&mut state)?;
            dirty = false;
            last_render = Instant::now();
        }
        if !event::poll(Duration::from_millis(60)).map_err(ServiceError::storage)? {
            continue;
        }
        let input = event::read().map_err(ServiceError::storage)?;
        if matches!(&input, Input::Mouse(mouse) if !matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)))
        {
            continue;
        }
        dirty = true;
        if handle_input(&mut state, input, &mut sink)? {
            break;
        }
    }
    drop(terminal);
    drop(commands);
    drop(updates);
    drop(worker_guard);
    if let Some(id) = state.active.or(state.parent) {
        if state.status["browser_connection"]["transport"] == "grpc" {
            eprintln!(
                "Session saved. Reconnect with `anticrumb connect`, then use `/resume {id}`."
            );
        } else {
            eprintln!("Session saved. Continue with: anticrumb resume {id}");
        }
    }
    Ok(())
}
