//! Terminal raw-mode lifecycle, effect sink, and interactive event loop.
use super::{
    clipboard,
    commands::{complete, Choice, Selection},
    layout::FrameLayout,
    mentions::MentionSearchHandle,
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
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Terminal {
    _console_guard: platform::ConsoleGuard,
}

impl Terminal {
    fn enter() -> Result<Self> {
        let console_guard = platform::ConsoleGuard::init();
        terminal::enable_raw_mode().map_err(ServiceError::storage)?;
        let guard = Self {
            _console_guard: console_guard,
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

    /// Called from the main loop's `INPUT_MODE_REFRESH` interval.
    fn refresh_input_modes(&mut self) -> Result<()> {
        // An emulator reset can disable mouse reporting without notifying the
        // child process. Restore input modes while idle as well as during work.
        execute!(
            io::stdout(),
            event::EnableBracketedPaste,
            event::EnableMouseCapture
        )
        .map_err(ServiceError::storage)
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
    commands: &'a tokio::sync::mpsc::Sender<Command>,
    mentions: &'a MentionSearchHandle,
}

impl EffectSink for TerminalSink<'_> {
    fn send_command(&mut self, command: Command) -> std::result::Result<(), String> {
        if let Command::SearchMentions {
            id,
            workspace,
            query,
            recent,
        } = command
        {
            self.mentions.schedule(id, workspace, query, recent);
            return Ok(());
        }
        self.commands.try_send(command).map_err(|e| {
            let cmd = match &e {
                tokio::sync::mpsc::error::TrySendError::Full(c)
                | tokio::sync::mpsc::error::TrySendError::Closed(c) => c,
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

/// Re-enable mouse/paste reporting this often (emulator resets drop them silently).
const INPUT_MODE_REFRESH: Duration = Duration::from_millis(500);
/// Upper bound on how long the input thread blocks before re-checking shutdown.
const INPUT_POLL: Duration = Duration::from_millis(200);
/// Minimum gap between repaints caused by background updates (~30 fps).
/// Keyboard/mouse input always repaints immediately.
const UPDATE_FRAME: Duration = Duration::from_millis(33);
/// Spinner frame period while busy.
const ANIMATION_FRAME: Duration = Duration::from_millis(120);
/// Selection autoscroll period while dragging.
const DRAG_FRAME: Duration = Duration::from_millis(50);
/// How long shutdown waits for an in-flight worker command before aborting it.
const WORKER_GRACE: Duration = Duration::from_secs(1);

fn is_ignored_input(input: &Input) -> bool {
    matches!(
        input,
        Input::Mouse(mouse)
            if !matches!(
                mouse.kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
            )
    )
}

const FRAME_BUF_INIT_CAP: usize = 16 * 1024;
const FRAME_BUF_SHRINK_THRESHOLD: usize = 256 * 1024;

fn render_stdout(state: &mut State, buf: &mut Vec<u8>) -> Result<()> {
    let (width, height) = terminal::size().map_err(ServiceError::storage)?;
    buf.clear();
    state.render_frame(buf, width, height)?;
    if buf.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    out.write_all(buf).map_err(ServiceError::storage)?;
    out.flush().map_err(ServiceError::storage)?;
    if buf.capacity() > FRAME_BUF_SHRINK_THRESHOLD {
        buf.clear();
        buf.shrink_to(FRAME_BUF_INIT_CAP);
    }
    Ok(())
}

pub(super) fn handle_input(
    state: &mut State,
    input: Input,
    sink: &mut dyn EffectSink,
) -> Result<bool> {
    match input {
        Input::Key(key) if key.kind != event::KeyEventKind::Release => {
            let had_selection = state.selection.is_some();
            state.clear_selection();
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
                    KeyCode::Char(' ')
                        if key.modifiers.is_empty() && key.kind == event::KeyEventKind::Press =>
                    {
                        if !state.toggle_permission_menu_item() {
                            state.shift_inspector_menu_item(true);
                        }
                    }
                    KeyCode::Left | KeyCode::Right => {
                        if !state.toggle_permission_menu_item()
                            && !state.shift_inspector_menu_item(key.code == KeyCode::Right)
                        {
                            let menu = state.menu.as_mut().unwrap();
                            if let Some(Choice {
                                value: Selection::Model(model),
                                ..
                            }) = menu.choices.get_mut(menu.selected)
                            {
                                model.shift(key.code == KeyCode::Right);
                            }
                        }
                    }
                    KeyCode::Enter => state.select(sink),
                    _ => {}
                }
                return Ok(false);
            }
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            // Priority 1: Ctrl+O fullscreen Inspector overlay modal routing.
            if state.inspector_zoom {
                match key.code {
                    KeyCode::Esc => {
                        state.inspector_zoom = false;
                        state.inspector_zoom_scroll = 0;
                    }
                    KeyCode::Char('o' | 'O') if ctrl && !alt => {
                        state.inspector_zoom = false;
                        state.inspector_zoom_scroll = 0;
                    }
                    KeyCode::Left if !ctrl && !alt => {
                        state.select_prev_next_tool(false);
                    }
                    KeyCode::Right if !ctrl && !alt => {
                        state.select_prev_next_tool(true);
                    }
                    KeyCode::Up => state.scroll_zoom_overlay(true, 1, None),
                    KeyCode::Down => state.scroll_zoom_overlay(false, 1, None),
                    KeyCode::PageUp => state.scroll_zoom_overlay(true, 10, None),
                    KeyCode::PageDown => state.scroll_zoom_overlay(false, 10, None),
                    KeyCode::Home => state.scroll_zoom_overlay(true, 0, Some(true)),
                    KeyCode::End => state.scroll_zoom_overlay(false, 0, Some(false)),
                    KeyCode::Char('f' | 'F' | 'l' | 'L') if !ctrl && !alt => {
                        state.toggle_inspector_follow();
                    }
                    KeyCode::Char('l' | 'L') if alt && !ctrl => {
                        state.toggle_inspector_follow();
                    }
                    KeyCode::Char(ch @ ('y' | 'Y' | 'n' | 'N'))
                        if !ctrl && !alt && !state.tool_approvals.is_empty() =>
                    {
                        if key.kind == event::KeyEventKind::Press {
                            state.respond_tool_approval(matches!(ch, 'y' | 'Y'), sink);
                        }
                    }
                    _ => {}
                }
                return Ok(false);
            }
            // Ctrl+O opens fullscreen Inspector overlay (works even with draft text in composer).
            if ctrl && !alt && matches!(key.code, KeyCode::Char('o' | 'O')) {
                state.toggle_inspector_zoom();
                return Ok(false);
            }
            if !state.tool_approvals.is_empty()
                && state.editor.text.is_empty()
                && state.history_clear_confirmation.is_none()
                && state.completion.is_none()
                && state.mention.is_none()
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                if let KeyCode::Char(ch @ ('y' | 'Y' | 'n' | 'N')) = key.code {
                    if key.kind == event::KeyEventKind::Press {
                        state.respond_tool_approval(matches!(ch, 'y' | 'Y'), sink);
                    }
                    return Ok(false);
                }
            }
            // Priority 2: Tool Inspector Alt+* shortcuts MUST precede catch-all Char/Up/Down/PgUp/PgDn.
            if alt && !ctrl {
                match key.code {
                    KeyCode::Char('i' | 'I') => {
                        state.toggle_inspector_mode();
                        return Ok(false);
                    }
                    KeyCode::Char('l' | 'L') => {
                        state.toggle_inspector_follow();
                        return Ok(false);
                    }
                    KeyCode::Up => {
                        state.scroll_focused_inspector(true, 3);
                        return Ok(false);
                    }
                    KeyCode::Down => {
                        state.scroll_focused_inspector(false, 3);
                        return Ok(false);
                    }
                    KeyCode::PageUp => {
                        state.scroll_focused_inspector(true, 10);
                        return Ok(false);
                    }
                    KeyCode::PageDown => {
                        state.scroll_focused_inspector(false, 10);
                        return Ok(false);
                    }
                    _ => {}
                }
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
                        state.clear_mention();
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
                    if state.mention.is_some() {
                        state.dismiss_mention();
                        return Ok(false);
                    }
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
                    if state.select_mention(sink) {
                        state.completion = None;
                        return Ok(false);
                    }
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
                KeyCode::Left => {
                    if key.modifiers.is_empty()
                        && state.editor.text.is_empty()
                        && state.mention.is_none()
                        && state.completion.is_none()
                        && !state.tool_blocks.is_empty()
                    {
                        state.select_prev_next_tool(false);
                    } else {
                        state.editor.left();
                    }
                }
                KeyCode::Right => {
                    if key.modifiers.is_empty()
                        && state.editor.text.is_empty()
                        && state.mention.is_none()
                        && state.completion.is_none()
                        && !state.tool_blocks.is_empty()
                    {
                        state.select_prev_next_tool(true);
                    } else {
                        state.editor.right();
                    }
                }
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
                    if !state.tab_complete_mention(sink) {
                        if let Some(c) = complete(&state.editor, &state.options.workspace) {
                            if let Some(value) = c.values.first() {
                                state.editor.replace(c.start..c.end, value);
                            }
                        }
                    }
                }
                KeyCode::Up | KeyCode::Down => {
                    if state.mention.is_some() && key.modifiers.is_empty() {
                        state.move_mention(key.code == KeyCode::Up);
                        return Ok(false);
                    }
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
            state.refresh_mention(sink);
            state.completion = if state.mention.is_some() {
                None
            } else {
                complete(&state.editor, &state.options.workspace).filter(|c| !c.values.is_empty())
            };
        }
        Input::Paste(text) => {
            state.clear_selection();
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
                state.scroll_wheel_at(
                    mouse.kind == MouseEventKind::ScrollUp,
                    mouse.column,
                    mouse.row,
                    width.into(),
                    height.into(),
                );
            } else if !state.completion_mouse(mouse, sink) {
                if let Some(text) = state.transcript_mouse(mouse) {
                    state.copy_text(&text, true, sink);
                }
            }
        }
        Input::Resize(_, _) => {
            state.clear_selection();
            state.last_click = None;
            state.clear_painted_regions();
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
        runtime: tokio::runtime::Handle,
        task: Option<tokio::task::JoinHandle<()>>,
    }
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            let Some(mut task) = self.task.take() else {
                let _ = self.client.close();
                return;
            };
            if tokio::runtime::Handle::try_current().is_ok() {
                // Cannot block inside a runtime; the queue guard still cleans up.
                let _ = self.client.close();
                task.abort();
                return;
            }
            // The command channel is already closed, so an idle worker exits at
            // once; give an in-flight command (upload, steer, cancel) a short
            // grace period with the RPC still open before tearing it down.
            let finished = self
                .runtime
                .block_on(async { tokio::time::timeout(WORKER_GRACE, &mut task).await })
                .is_ok();
            let _ = self.client.close();
            if !finished {
                task.abort();
                let _ = self.runtime.block_on(task);
            }
        }
    }
    let runtime = client.runtime.clone();
    // Declared before channels: every early return disconnects them before awaiting.
    let mut worker_guard = WorkerGuard {
        client: client.clone(),
        runtime: runtime.clone(),
        task: None,
    };
    let (commands, rx) = tokio::sync::mpsc::channel(32);
    let (tx, mut updates) = tokio::sync::mpsc::channel(128);
    let mention_search = MentionSearchHandle::spawn(tx.clone());
    worker_guard.task = Some(runtime.spawn(worker(client, rx, tx)));
    let mut terminal = Terminal::enter()?;
    let mut state = State::new(options);
    if let crate::rpc::client::Connection::Grpc {
        endpoint, tls_pin, ..
    } = crate::rpc::workspace_client::selected_connection()
    {
        state.remote_tls_pin = tls_pin;
        state.remote_tls_endpoint = Some(crate::rpc::client::endpoint_description(&endpoint));
    }
    let prefs = Preferences::open(&crate::cli::root())?;
    // Resolve saved model IDs to display names before the immutable startup banner.
    state.catalog =
        crate::store::Store::open(&crate::cli::root())?.model_catalog(&state.options.account)?;
    state.theme = theme::find(&prefs.theme()).unwrap_or(theme::THEMES[0]);
    state.debug.level = prefs.debug_level();
    state.layout.inspector = prefs.inspector_config();
    if state.layout.inspector.mode != super::layout::InspectorMode::Off {
        state.inspector_last_active_mode = state.layout.inspector.mode;
    }
    state.preferences = Some(prefs);
    state.load_model_preference();
    state.load_history();
    let mut sink = TerminalSink {
        commands: &commands,
        mentions: &mention_search,
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

    let stop_input = Arc::new(AtomicBool::new(false));
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<io::Result<Input>>(64);
    let stop_reader = Arc::clone(&stop_input);
    // crossterm's level-triggered reader (`use-dev-tty`) drains paste bursts
    // without another keypress (#1142). A bounded poll (instead of a bare
    // blocking read) lets shutdown stop and join this thread, so it never keeps
    // crossterm's global event reader or swallows input after the TUI exits.
    let reader = std::thread::spawn(move || {
        while !stop_reader.load(Ordering::Relaxed) {
            match event::poll(INPUT_POLL) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(err) => {
                    let _ = input_tx.blocking_send(Err(err));
                    break;
                }
            }
            match event::read() {
                Ok(input) => {
                    if stop_reader.load(Ordering::Relaxed)
                        || input_tx.blocking_send(Ok(input)).is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = input_tx.blocking_send(Err(err));
                    break;
                }
            }
        }
    });

    let loop_result: Result<()> = runtime.block_on(async {
        // `dirty`: something changed; `urgent`: user input, repaint immediately.
        let mut dirty = true;
        let mut urgent = true;
        let mut last_render = Instant::now();
        let mut frame_buf = Vec::with_capacity(FRAME_BUF_INIT_CAP);
        let mut mode_refresh = tokio::time::interval_at(
            tokio::time::Instant::now() + INPUT_MODE_REFRESH,
            INPUT_MODE_REFRESH,
        );
        mode_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            while let Ok(update) = updates.try_recv() {
                if initial && matches!(&update, Update::Error(_)) {
                    initial = false;
                }
                if state.update(update) {
                    dirty = true;
                }
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
                dirty = true;
                urgent = true;
            }
            // Coalesce bursts of background updates (streamed events) into at
            // most one repaint per UPDATE_FRAME; input repaints right away.
            if dirty && (urgent || last_render.elapsed() >= UPDATE_FRAME) {
                render_stdout(&mut state, &mut frame_buf)?;
                dirty = false;
                urgent = false;
                last_render = Instant::now();
            }

            let dragging = state.selection_drag.is_some();
            let animating = state.needs_animation() || state.mention_loading();
            let deadline = [
                dirty.then(|| last_render + UPDATE_FRAME),
                animating.then(|| last_render + ANIMATION_FRAME),
                dragging.then(|| Instant::now() + DRAG_FRAME),
            ]
            .into_iter()
            .flatten()
            .min()
            .map(tokio::time::Instant::from_std);

            tokio::select! {
                _ = mode_refresh.tick() => {
                    terminal.refresh_input_modes()?;
                }
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if dragging && state.tick_selection_autoscroll() {
                        dirty = true;
                        urgent = true;
                    }
                    if animating && last_render.elapsed() >= ANIMATION_FRAME {
                        dirty = true;
                    }
                }
                maybe_update = updates.recv() => {
                    let Some(update) = maybe_update else {
                        break;
                    };
                    if initial && matches!(&update, Update::Error(_)) {
                        initial = false;
                    }
                    if state.update(update) {
                        dirty = true;
                    }
                }
                maybe_input = input_rx.recv() => {
                    let Some(first) = maybe_input else {
                        break;
                    };
                    let mut should_exit = false;
                    let mut handle_one = |res: io::Result<Input>| -> Result<bool> {
                        let input = res.map_err(ServiceError::storage)?;
                        if is_ignored_input(&input) {
                            return Ok(false);
                        }
                        dirty = true;
                        urgent = true;
                        handle_input(&mut state, input, &mut sink)
                    };
                    if handle_one(first)? {
                        should_exit = true;
                    } else {
                        // Drain ALL queued burst input (paste / fast typing / mouse
                        // drags) before repainting. try_recv returns only what is
                        // already buffered, so this cannot loop forever.
                        while let Ok(next) = input_rx.try_recv() {
                            if handle_one(next)? {
                                should_exit = true;
                                break;
                            }
                        }
                    }
                    if should_exit {
                        break;
                    }
                }
            }
        }
        Ok(())
    });

    stop_input.store(true, Ordering::Relaxed);
    drop(input_rx);
    // Bounded by INPUT_POLL; must finish before raw mode is disabled.
    let _ = reader.join();
    drop(terminal);
    drop(mention_search);
    drop(commands);
    drop(updates);
    drop(worker_guard);
    loop_result?;
    if let Some(id) = state.conversation_id.clone() {
        eprintln!("{}", exit_conversation_message(&id, &state));
    }
    Ok(())
}

pub(super) fn exit_conversation_message(id: &str, state: &State) -> String {
    let temporary = state
        .conversation_temporary
        .unwrap_or(state.options.temporary);
    if state.status["browser_connection"]["transport"] == "grpc" {
        let temp_note = if temporary {
            " (temporary mode: not stored on provider server)"
        } else {
            ""
        };
        format!(
            "Session saved{temp_note}. Reconnect with `anticrumb connect`, then use `/resume {id}`."
        )
    } else if temporary {
        format!(
            "Local conversation saved (temporary mode: not stored on provider server). Continue with: anticrumb resume {id}"
        )
    } else {
        format!("Session saved. Continue with: anticrumb resume {id}")
    }
}
