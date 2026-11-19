use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;

fn temp_ws(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let ws = std::env::temp_dir().join(format!(
        "anticrumb_agent_{tag}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&ws);
    fs::create_dir_all(&ws).unwrap();
    ws
}

fn tool_block(tool: &str, args: Value) -> String {
    format!(
        "[TOOL_CALL]\n{}\n[/TOOL_CALL]",
        json!({"tool": tool, "args": args})
    )
}

type Script = Box<dyn FnMut(usize, &str) -> Result<String, String> + Send>;

/// Scripted backend recording every prompt it receives.
struct MockBackend {
    script: Script,
    calls: usize,
    prompts: Arc<Mutex<Vec<String>>>,
    resets: Arc<AtomicUsize>,
}

impl MockBackend {
    fn replies(replies: Vec<String>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let mut q: VecDeque<String> = replies.into();
        Self::scripted(Box::new(move |_, _| {
            Ok(q.pop_front().unwrap_or_else(|| "All done!".to_string()))
        }))
    }

    fn scripted(script: Script) -> (Self, Arc<Mutex<Vec<String>>>) {
        let prompts = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                script,
                calls: 0,
                prompts: Arc::clone(&prompts),
                resets: Arc::new(AtomicUsize::new(0)),
            },
            prompts,
        )
    }
}

impl AgentBackend for MockBackend {
    fn reset_session(&mut self) -> Result<(), String> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn retry_safe(&self) -> bool {
        true
    }
    fn ask_step(
        &mut self,
        prompt: &str,
        _is_first: bool,
        on_chunk: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        let idx = self.calls;
        self.calls += 1;
        let r = (self.script)(idx, prompt)?;
        on_chunk(&r);
        Ok(r)
    }
}

fn cfg(ws: &Path) -> AgentConfig {
    AgentConfig {
        workspace_root: ws.to_path_buf(),
        max_steps: 6,
        step_retries: 1,
        prompts_dir: Some(ws.join(".test-no-prompts")),
        ..Default::default()
    }
}

fn call(tool: &str, args: Value) -> ToolCallRequest {
    ToolCallRequest {
        tool: tool.into(),
        args,
    }
}

// ----------------------------------------------------------------------------- tools

#[test]
fn atomic_file_tools_report_success_and_changes() {
    let ws = tempfile::tempdir().unwrap();
    let context = ToolContext::new(ws.path().to_path_buf());
    let registry = ToolRegistry::with_builtin_coding_tools();
    let created = registry.execute_call(
        &call(
            "write_file",
            json!({"path":"nested/result.txt","content":"original"}),
        ),
        &context,
    );
    assert!(created.ok, "{}", created.output);
    assert_eq!(
        fs::read_to_string(ws.path().join("nested/result.txt")).unwrap(),
        "original"
    );
    assert_eq!(context.changed_files(), vec!["nested/result.txt"]);

    let edited = registry.execute_call(
        &call(
            "edit_file",
            json!({"path":"nested/result.txt","old_text":"original","new_text":"updated"}),
        ),
        &context,
    );
    assert!(edited.ok, "{}", edited.output);
    assert_eq!(
        fs::read_to_string(ws.path().join("nested/result.txt")).unwrap(),
        "updated"
    );
    assert_eq!(context.changed_files(), vec!["nested/result.txt"]);
    assert_eq!(fs::read_dir(ws.path().join("nested")).unwrap().count(), 1);
}

#[test]
fn coding_tools_find_grep_read_edit_write_and_shell() {
    let ws = temp_ws("tools");
    let ctx = ToolContext::new(ws.clone());
    let reg = ToolRegistry::with_builtin_coding_tools();

    let w = reg.execute_call(
        &call("write_file", json!({"path": "src/hello.rs", "content": "pub fn greet() -> &'static str {\n    \"hello\"\n}\n"})),
        &ctx,
    );
    assert!(w.ok, "{}", w.output);

    let f = reg.execute_call(&call("find_files", json!({"pattern": "*.rs"})), &ctx);
    assert!(f.ok && f.output.contains("src/hello.rs"));

    let g = reg.execute_call(
        &call("grep_search", json!({"query": "greet", "dir": "src"})),
        &ctx,
    );
    assert!(g.ok && g.output.contains("src/hello.rs:1:"), "{}", g.output);

    let e = reg.execute_call(
        &call(
            "edit_file",
            json!({"path": "src/hello.rs", "old_text": "\"hello\"", "new_text": "\"hello agent\""}),
        ),
        &ctx,
    );
    assert!(e.ok, "{}", e.output);

    let r = reg.execute_call(
        &call(
            "read_file",
            json!({"path": "src/hello.rs", "start_line": 1, "end_line": 3}),
        ),
        &ctx,
    );
    assert!(r.ok && r.output.contains("\"hello agent\""));

    let b = reg.execute_call(
        &call(
            "run_shell",
            json!({"command": if cfg!(windows) { "Write-Output 'shell-ok'; [Console]::Error.WriteLine('err'); exit 3" } else { "echo shell-ok; echo err >&2; exit 3" }}),
        ),
        &ctx,
    );
    assert!(b.ok, "{}", b.output);
    assert!(
        b.output.contains("exit_code: 3")
            && b.output.contains("shell-ok")
            && b.output.contains("err")
    );

    assert_eq!(ctx.changed_files(), vec!["src/hello.rs".to_string()]);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_requires_unique_match_and_rejects_unknown_tool() {
    let ws = temp_ws("edit");
    let ctx = ToolContext::new(ws.clone());
    let reg = ToolRegistry::default();
    fs::write(ws.join("a.txt"), "x x").unwrap();
    let e = reg.execute_call(
        &call(
            "edit_file",
            json!({"path": "a.txt", "old_text": "x", "new_text": "y"}),
        ),
        &ctx,
    );
    assert!(!e.ok && e.output.contains("2 locations"));
    let e = reg.execute_call(
        &call(
            "edit_file",
            json!({"path": "a.txt", "old_text": "x", "new_text": "y", "replace_all": true}),
        ),
        &ctx,
    );
    assert!(e.ok);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "y y");
    let u = reg.execute_call(&call("nope", json!({})), &ctx);
    assert!(!u.ok && u.output.contains("unknown tool"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn workspace_escape_is_blocked_lexically_and_via_symlink() {
    let ws = temp_ws("escape");
    let outside = temp_ws("outside");
    fs::write(outside.join("secret.txt"), "top secret").unwrap();
    let ctx = ToolContext::new(ws.clone());

    assert!(ctx.resolve_workspace_path("../etc/passwd").is_err());
    assert!(ctx.resolve_workspace_path("/etc/passwd").is_err());
    assert!(ctx.resolve_workspace_path("a/b/new.txt").is_ok());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
        let err = ctx.resolve_workspace_path("link/secret.txt").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        // not-yet-existing file below an escaping symlink
        assert!(ctx.resolve_workspace_path("link/new.txt").is_err());
        let reg = ToolRegistry::default();
        let r = reg.execute_call(&call("read_file", json!({"path": "link/secret.txt"})), &ctx);
        assert!(!r.ok);
        let w = reg.execute_call(
            &call(
                "write_file",
                json!({"path": "link/pwn.txt", "content": "x"}),
            ),
            &ctx,
        );
        assert!(!w.ok);
        assert!(!outside.join("pwn.txt").exists());
    }
    let _ = fs::remove_dir_all(&ws);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn shell_timeout_returns_partial_output_and_kills_tree() {
    let ws = temp_ws("timeout");
    let ctx = ToolContext::new(ws.clone());
    let reg = ToolRegistry::default();
    let t = Instant::now();
    let out = reg.execute_call(
        &call(
            "run_shell",
            json!({"command": if cfg!(windows) { "Write-Output started; Start-Sleep -Seconds 30" } else { "echo started; sleep 30" }, "timeout_secs": 1}),
        ),
        &ctx,
    );
    assert!(t.elapsed() < Duration::from_secs(10));
    assert!(!out.ok);
    assert!(
        out.output.contains("timed out") && out.output.contains("started"),
        "{}",
        out.output
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn shell_is_cancellable() {
    let ws = temp_ws("cancelshell");
    let ctx = ToolContext::new(ws.clone());
    let cancel = ctx.cancel.clone();
    let reg = ToolRegistry::default();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        cancel.cancel();
    });
    let t = Instant::now();
    let out = reg.execute_call(
        &call(
            "run_shell",
            json!({"command": if cfg!(windows) { "Start-Sleep -Seconds 30" } else { "sleep 30" }}),
        ),
        &ctx,
    );
    assert!(t.elapsed() < Duration::from_secs(10));
    assert!(
        !out.ok && out.output.contains("cancelled"),
        "{}",
        out.output
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn background_process_lifecycle() {
    let ws = temp_ws("bg");
    let ctx = ToolContext::new(ws.clone());
    let reg = ToolRegistry::default();

    let s = reg.execute_call(
        &call(
            "start_process",
            json!({"command": if cfg!(windows) { "Write-Output first; Start-Sleep -Milliseconds 1200; Write-Output second; Start-Sleep -Seconds 60" } else { "echo first; sleep 0.8; echo second; sleep 30" }}),
        ),
        &ctx,
    );
    assert!(s.ok, "{}", s.output);
    let o1 = reg.execute_call(
        &call(
            "process_output",
            json!({"id": 1, "wait_secs": if cfg!(windows) { 25 } else { 5 }}),
        ),
        &ctx,
    );
    // the command line in the header mentions "first"; only the stdout section matters
    let o1_stdout = o1.output.split("[stdout]").nth(1).unwrap_or_default();
    assert!(
        o1.ok && o1_stdout.contains("first") && o1.output.contains("(running)"),
        "{}",
        o1.output
    );
    let o2 = reg.execute_call(
        &call(
            "process_output",
            json!({"id": 1, "wait_secs": if cfg!(windows) { 10 } else { 5 }}),
        ),
        &ctx,
    );
    let o2_stdout = o2.output.split("[stdout]").nth(1).unwrap_or_default();
    assert!(
        o2.ok && o2_stdout.contains("second") && !o2_stdout.contains("first"),
        "{}",
        o2.output
    );
    let k = reg.execute_call(&call("stop_process", json!({"id": 1})), &ctx);
    assert!(k.ok, "{}", k.output);
    assert!(ctx.background.list()[0].contains("exited"));
    let bad = reg.execute_call(&call("process_output", json!({"id": 99})), &ctx);
    assert!(!bad.ok && bad.output.contains("unknown background"));
    let _ = fs::remove_dir_all(&ws);
}

struct SleepTool(&'static str, bool);

impl AgentTool for SleepTool {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "sleeps"
    }
    fn parameters_schema(&self) -> Value {
        json!({})
    }
    fn is_read_only(&self) -> bool {
        self.1
    }
    fn execute(&self, _args: &Value, _ctx: &ToolContext) -> Result<String, String> {
        std::thread::sleep(Duration::from_millis(300));
        Ok(format!("{} done", self.0))
    }
}

#[test]
fn read_only_calls_run_in_parallel_and_keep_order() {
    let ws = temp_ws("par");
    let ctx = ToolContext::new(ws.clone());
    let mut reg = ToolRegistry::new();
    for n in ["r1", "r2", "r3"] {
        reg.register(Box::new(SleepTool(n, true)));
    }
    reg.register(Box::new(SleepTool("w1", false)));
    reg.register(Box::new(SleepTool("w2", false)));

    let started = Mutex::new(Vec::new());
    let calls: Vec<_> = ["r1", "r2", "r3"]
        .iter()
        .map(|n| call(n, json!({})))
        .collect();
    let t = Instant::now();
    let outs = reg.execute_batch(
        &calls,
        &ctx,
        &|i, _| started.lock().unwrap().push(i),
        &|_, _, _| {},
    );
    assert!(
        t.elapsed() < Duration::from_millis(800),
        "took {:?}",
        t.elapsed()
    );
    assert_eq!(
        outs.iter().map(|o| o.output.as_str()).collect::<Vec<_>>(),
        ["r1 done", "r2 done", "r3 done"]
    );
    assert_eq!(started.lock().unwrap().len(), 3);

    // mutating calls stay sequential
    let calls: Vec<_> = ["w1", "w2"].iter().map(|n| call(n, json!({}))).collect();
    let t = Instant::now();
    reg.execute_batch(&calls, &ctx, &|_, _| {}, &|_, _, _| {});
    assert!(t.elapsed() >= Duration::from_millis(600));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn oversized_output_keeps_head_and_tail() {
    let s = format!("HEAD{}TAIL", "x".repeat(50_000));
    let t = tools::truncate_tool_output(&s, 1_000);
    assert!(t.starts_with("HEAD") && t.ends_with("TAIL") && t.contains("truncated"));
    assert!(t.chars().count() < 1_100);
}

// ----------------------------------------------------------------------------- engine

#[test]
fn react_loop_multi_step_with_files_changed_and_events() {
    let ws = temp_ws("loop");
    let (mock, prompts) = MockBackend::replies(vec![
        format!(
            "Create a file.\n{}",
            tool_block("write_file", json!({"path": "note.txt", "content": "v1"}))
        ),
        format!(
            "Update it.\n{}",
            tool_block(
                "edit_file",
                json!({"path": "note.txt", "old_text": "v1", "new_text": "v2"})
            )
        ),
        "Task completed: note.txt is now v2.".to_string(),
    ]);
    let events = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let ev2 = Arc::clone(&events);
    let control = RunControl::with_events(EventSink::new(move |e| ev2.lock().unwrap().push(e)));

    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let res = agent.run("Create and update note.txt", &control).unwrap();
    assert_eq!(res.status, AgentStatus::Completed);
    assert_eq!(res.steps.len(), 3);
    assert_eq!(res.final_response, "Task completed: note.txt is now v2.");
    assert_eq!(res.files_changed, vec!["note.txt".to_string()]);
    assert_eq!(fs::read_to_string(ws.join("note.txt")).unwrap(), "v2");

    // persistent chat: later prompts carry only the new observations, not the system prompt
    let p = prompts.lock().unwrap();
    assert!(p[0].contains("[TOOLS]"));
    assert!(p[1].starts_with("[TOOL_RESULTS]") && !p[1].contains("[TOOLS]"));

    let evs = events.lock().unwrap();
    assert!(matches!(evs.first(), Some(AgentEvent::Started { .. })));
    assert!(matches!(evs.last(), Some(AgentEvent::Finished { .. })));
    let tool_starts = evs
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolStarted { .. }))
        .count();
    let tool_ends = evs
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolFinished { ok: true, .. }))
        .count();
    assert_eq!((tool_starts, tool_ends), (2, 2));
    assert!(evs
        .iter()
        .any(|e| matches!(e, AgentEvent::ModelChunk { .. })));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn persistent_chat_keeps_session_past_the_old_context_budget() {
    let ws = tempfile::tempdir().unwrap();
    fs::write(ws.path().join("large.txt"), "x".repeat(15_000)).unwrap();
    let mut replies: Vec<_> = (0..5)
        .map(|_| tool_block("read_file", json!({"path": "large.txt"})))
        .collect();
    replies.push("done".into());
    let (mock, prompts) = MockBackend::replies(replies);
    let resets = mock.resets.clone();
    let config = cfg(ws.path());
    let budget = config.context_budget_chars;
    let mut agent = CodingAgent::new(mock, config);
    let result = agent.run("Read the file", &RunControl::default()).unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(resets.load(Ordering::SeqCst), 0);
    let prompts = prompts.lock().unwrap();
    assert!(prompts.iter().map(|p| p.chars().count()).sum::<usize>() > budget);
    assert!(prompts[1..].iter().all(|p| {
        p.starts_with("[TOOL_RESULTS]") && !p.contains("[TOOLS]") && !p.contains("elided")
    }));
}

#[test]
fn persistent_chat_passes_large_initial_and_steering_prompts_to_backend() {
    let ws = tempfile::tempdir().unwrap();
    let control = RunControl::default();
    let inbox = control.inbox.clone();
    let steering = "Additional instructions\n".repeat(20_000);
    let expected_steering = steering.clone();
    let (mock, prompts) = MockBackend::scripted(Box::new(move |step, _| {
        if step == 0 {
            inbox.push(steering.clone());
        }
        Ok("done".into())
    }));
    let resets = mock.resets.clone();
    let mut agent = CodingAgent::new(mock, cfg(ws.path()));
    let task = "Initial task\n".repeat(10_000);
    let result = agent.run(&task, &control).unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(resets.load(Ordering::SeqCst), 0);
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].ends_with(&task));
    assert_eq!(prompts[1], format!("[USER_MESSAGE]\n{expected_steering}"));
}

#[test]
fn temp_chat_resends_compacted_transcript() {
    let ws = temp_ws("temp");
    fs::write(ws.join("big.txt"), "z".repeat(30_000)).unwrap();
    let (mock, prompts) = MockBackend::replies(vec![
        tool_block("read_file", json!({"path": "big.txt"})),
        tool_block("read_file", json!({"path": "big.txt", "start_line": 1})),
        tool_block("find_files", json!({})),
        "done".into(),
    ]);
    let mut c = cfg(&ws);
    c.temp_chat = true;
    c.context_budget_chars = 20_000;
    let mut agent = CodingAgent::new(mock, c);
    agent.run("t", &RunControl::default()).unwrap();
    let p = prompts.lock().unwrap();
    assert!(p[3].contains("[TOOLS]"), "task prompt must stay");
    assert!(
        p[3].contains("older tool results elided"),
        "old observations should be elided"
    );
    assert!(p[3].chars().count() < 30_000);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn malformed_tool_block_is_fed_back_instead_of_ending_the_run() {
    let ws = temp_ws("malformed");
    let (mock, prompts) = MockBackend::replies(vec![
        "[TOOL_CALL]\n{\"tool\": \"write_file\", \"args\": {oops}}\n[/TOOL_CALL]".to_string(),
        tool_block("write_file", json!({"path": "ok.txt", "content": "fine"})),
        "finished".to_string(),
    ]);
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let res = agent.run("x", &RunControl::default()).unwrap();
    assert_eq!(res.status, AgentStatus::Completed);
    assert_eq!(res.steps.len(), 3);
    assert!(prompts.lock().unwrap()[1].starts_with("[TOOL_ERROR]"));
    assert_eq!(fs::read_to_string(ws.join("ok.txt")).unwrap(), "fine");

    // endless garbage aborts
    let (mock, _) = MockBackend::scripted(Box::new(|_, _| {
        Ok("[TOOL_CALL]\nnot json\n[/TOOL_CALL]".to_string())
    }));
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let err = agent.run("x", &RunControl::default()).unwrap_err();
    assert!(err.contains("malformed"), "{err}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn step_budget_and_repeat_warning() {
    let ws = temp_ws("budget");
    let (mock, prompts) = MockBackend::scripted(Box::new(|_, _| {
        Ok(tool_block("find_files", json!({"pattern": "*.nothing"})))
    }));
    let mut c = cfg(&ws);
    c.max_steps = 4;
    let mut agent = CodingAgent::new(mock, c);
    let res = agent.run("loop", &RunControl::default()).unwrap();
    assert_eq!(res.status, AgentStatus::MaxSteps);
    assert_eq!(res.steps.len(), 4);
    assert!(prompts.lock().unwrap()[3].contains("repeating the same tool call"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn per_step_tool_call_limit_is_enforced() {
    let ws = temp_ws("limit");
    let many = (0..5)
        .map(|_| json!({"tool": "find_files", "args": {}}))
        .collect::<Vec<_>>();
    let (mock, prompts) = MockBackend::replies(vec![
        format!("[TOOL_CALL]\n{}\n[/TOOL_CALL]", json!({"tool_calls": many})),
        "ok".into(),
    ]);
    let mut c = cfg(&ws);
    c.max_tool_calls_per_step = 2;
    let mut agent = CodingAgent::new(mock, c);
    let res = agent.run("x", &RunControl::default()).unwrap();
    assert_eq!(res.steps[0].tool_calls.len(), 2);
    assert!(prompts.lock().unwrap()[1].contains("3 more were skipped"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn shell_tools_hidden_when_disabled() {
    let ws = temp_ws("noshell");
    let (mock, prompts) = MockBackend::replies(vec![
        tool_block("run_shell", json!({"command": "echo hi"})),
        "ok".into(),
    ]);
    let mut c = cfg(&ws);
    c.allow_shell = false;
    let mut agent = CodingAgent::new(mock, c);
    let res = agent.run("x", &RunControl::default()).unwrap();
    let p = prompts.lock().unwrap();
    assert!(!p[0].contains("start_process") && !p[0].contains("- run_shell"));
    assert!(!res.steps[0].observations[0].ok);
    assert!(res.steps[0].observations[0].output.contains("unknown tool"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn transient_backend_errors_are_retried_then_surface() {
    let ws = temp_ws("retry");
    let (mock, _) = MockBackend::scripted(Box::new(|i, _| {
        if i == 0 {
            Err("tab crashed".into())
        } else {
            Ok("recovered".into())
        }
    }));
    let events = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let ev2 = Arc::clone(&events);
    let control = RunControl::with_events(EventSink::new(move |e| ev2.lock().unwrap().push(e)));
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let res = agent.run("x", &control).unwrap();
    assert_eq!(res.final_response, "recovered");
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, AgentEvent::Retrying { attempt: 1, .. })));

    let (mock, _) = MockBackend::scripted(Box::new(|_, _| Err("always broken".into())));
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let err = agent.run("x", &RunControl::default()).unwrap_err();
    assert!(
        err.contains("always broken") && err.contains("2 attempt"),
        "{err}"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn steering_messages_reach_the_model_and_extend_a_finished_run() {
    let ws = temp_ws("steer");
    let control = RunControl::default();

    // queued before the run starts -> part of the first prompt
    control.inbox.push("prefer tabs over spaces");
    let inbox = control.inbox.clone();
    let (mock, prompts) = MockBackend::scripted(Box::new(move |i, _| {
        if i == 0 {
            // user speaks up while the model is "thinking" about its final answer
            inbox.push("also add a README");
            Ok("final answer v1".into())
        } else {
            Ok("final answer v2".into())
        }
    }));
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let res = agent.run("x", &control).unwrap();
    assert_eq!(res.final_response, "final answer v2");
    assert_eq!(res.steps.len(), 2);
    let p = prompts.lock().unwrap();
    assert!(p[0].contains("[USER_MESSAGE]\nprefer tabs over spaces"));
    assert!(p[1].contains("[USER_MESSAGE]\nalso add a README"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn cancel_before_and_during_run() {
    let ws = temp_ws("cancel");
    let control = RunControl::default();
    control.cancel.cancel();
    let (mock, prompts) = MockBackend::replies(vec![]);
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let res = agent.run("x", &control).unwrap();
    assert_eq!(res.status, AgentStatus::Cancelled);
    assert!(prompts.lock().unwrap().is_empty());
    let _ = fs::remove_dir_all(&ws);
}

#[cfg(unix)]
#[test]
fn timeout_survives_shell_exit_with_descendant_holding_pipes() {
    let ws = temp_ws("orphan_pipes");
    let mut ctx = ToolContext::new(ws.clone());
    ctx.shell_timeout = Duration::from_millis(150);
    let registry = ToolRegistry::with_builtin_coding_tools();
    let start = Instant::now();
    let result = registry.execute_call(&call("run_shell", json!({"command":"sleep 30 &"})), &ctx);
    assert!(!result.ok, "{}", result.output);
    assert!(start.elapsed() < Duration::from_secs(2));
    let _ = fs::remove_dir_all(ws);
}

#[test]
fn attachment_steering_is_delivered_before_its_model_turn() {
    struct Attached {
        inbox: SteerInbox,
        ids: Vec<String>,
        seen: Arc<Mutex<Vec<Vec<String>>>>,
    }
    impl AgentBackend for Attached {
        fn add_artifacts(&mut self, ids: &[String]) -> Result<(), String> {
            self.ids.extend_from_slice(ids);
            Ok(())
        }
        fn ask_step(
            &mut self,
            prompt: &str,
            _: bool,
            _: &mut dyn FnMut(&str),
        ) -> Result<String, String> {
            assert!(!prompt.contains("artifact_"));
            self.seen.lock().unwrap().push(self.ids.clone());
            if self.seen.lock().unwrap().len() == 1 {
                self.inbox.push_with_artifacts(
                    "also use the second attachment".into(),
                    vec!["artifact_second".into()],
                );
            }
            Ok("Done".into())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let control = RunControl::default();
    control
        .inbox
        .push_with_artifacts("use the attachment".into(), vec!["artifact_first".into()]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = Attached {
        inbox: control.inbox.clone(),
        ids: vec![],
        seen: seen.clone(),
    };
    let mut agent = CodingAgent::new(backend, cfg(dir.path()));
    let result = agent.run("Summarize attachments", &control).unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            vec!["artifact_first".to_owned()],
            vec!["artifact_first".to_owned(), "artifact_second".to_owned()]
        ]
    );
}

#[test]
fn screenshot_tool_images_reach_only_the_next_model_turn() {
    struct Images {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
        pending: Vec<String>,
        turn: usize,
    }
    impl AgentBackend for Images {
        fn set_step_artifacts(&mut self, ids: &[String]) -> Result<(), String> {
            self.pending = ids.to_vec();
            Ok(())
        }
        fn ask_step(
            &mut self,
            _prompt: &str,
            _first: bool,
            _chunk: &mut dyn FnMut(&str),
        ) -> Result<String, String> {
            self.seen
                .lock()
                .unwrap()
                .push(std::mem::take(&mut self.pending));
            self.turn += 1;
            Ok(if self.turn == 1 {
                tool_block("browser_control", json!({"action":"help"}))
            } else if self.turn <= 13 {
                tool_block("browser_control", json!({"action":"screenshot"}))
            } else if self.turn == 14 {
                tool_block("browser_control", json!({"action":"tabs"}))
            } else {
                "Done".into()
            })
        }
    }
    let ws = temp_ws("visual");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let count = AtomicUsize::new(0);
    let mut config = cfg(&ws);
    config.max_steps = 20;
    let mut agent = CodingAgent::new(
        Images {
            seen: seen.clone(),
            pending: vec![],
            turn: 0,
        },
        config,
    )
    .with_browser_control(Arc::new(move |args, _| {
        Ok(if args["action"] == "screenshot" {
            let n = count.fetch_add(1, Ordering::SeqCst);
            ToolContent {
                text: format!("Screenshot shot_{n}"),
                artifacts: vec![format!("artifact_{n}")],
            }
        } else {
            "Tabs: tab_1".into()
        })
    }));
    let result = agent
        .run("Inspect the page", &RunControl::default())
        .unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 15);
    assert!(seen[0].is_empty());
    assert!(seen[1].is_empty());
    for n in 0..12 {
        assert_eq!(seen[n + 2], vec![format!("artifact_{n}")]);
        assert_eq!(result.steps[n + 1].observations[0].artifacts, seen[n + 2]);
    }
    assert!(seen[14].is_empty());
    fs::remove_dir_all(ws).unwrap();
}

#[test]
fn web_tools_remove_web_search_and_support_browser_fetch_and_progressive_browser_control() {
    let ws = temp_ws("webtools");
    let reg = ToolRegistry::with_builtin_coding_tools();
    assert!(!reg.contains("web_search"), "web_search must not be a tool");
    for name in ["web_fetch", "browser_fetch", "browser_control"] {
        assert!(reg.contains(name), "missing tool {name}");
    }
    let initial = reg
        .list_initial_schemas_with_overrides(&Default::default())
        .into_iter()
        .find(|tool| tool["name"] == "browser_control")
        .unwrap();
    assert!(crate::schema::is_valid(&initial["parameters"], &json!({"action":"help"})).unwrap());
    assert!(!crate::schema::is_valid(
        &initial["parameters"],
        &json!({"action":"open","url":"https://example.com/"})
    )
    .unwrap());
    let full = reg
        .list_schemas_with_overrides(&Default::default())
        .into_iter()
        .find(|tool| tool["name"] == "browser_control")
        .unwrap();
    assert_eq!(full["parameters"], crate::browser_control::schema());

    // Without a runtime bridge, browser_fetch and browser_control actions fail explicitly,
    // while browser_control action='help' needs no browser for progressive disclosure.
    let ctx_no_browser = ToolContext::new(ws.clone());
    let missing = reg.execute_call(
        &call("browser_fetch", json!({"url": "https://example.com/"})),
        &ctx_no_browser,
    );
    assert!(!missing.ok);
    assert!(missing.output.contains("unavailable"), "{}", missing.output);

    let help = reg.execute_call(
        &call("browser_control", json!({"action": "help"})),
        &ctx_no_browser,
    );
    assert!(help.ok, "{}", help.output);
    assert!(
        help.output.contains("\"action\": \"open\""),
        "{}",
        help.output
    );
    assert!(
        help.output.contains("\"action\": \"click\""),
        "{}",
        help.output
    );
    // The guide and full schema must survive normal tool-output truncation.
    let (_, schema) = help
        .output
        .split_once(
            "Full browser_control argument schema (replaces the initial help-only schema):\n",
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(schema).unwrap(),
        crate::browser_control::schema()
    );
    assert!(help
        .output
        .contains("Never insert an empty or nested tool_calls"));
    // Normally the engine acknowledges help after returning the entire batch.
    ctx_no_browser.acknowledge_browser_help();

    let missing_ctrl = reg.execute_call(
        &call(
            "browser_control",
            json!({"action": "open", "url": "https://example.com/"}),
        ),
        &ctx_no_browser,
    );
    assert!(!missing_ctrl.ok);
    assert!(
        missing_ctrl.output.contains("unavailable"),
        "{}",
        missing_ctrl.output
    );

    // Schema validation rejects unknown keys or invalid URLs/limits.
    let bad_args = reg.execute_call(
        &call(
            "web_fetch",
            json!({"url": "https://example.com/", "extra": 1}),
        ),
        &ctx_no_browser,
    );
    assert!(!bad_args.ok);
    assert!(bad_args.output.contains("schema"), "{}", bad_args.output);

    let ssrf = reg.execute_call(
        &call(
            "web_fetch",
            json!({"url": "http://127.0.0.1:9222/json/list"}),
        ),
        &ctx_no_browser,
    );
    assert!(!ssrf.ok);
    assert!(ssrf.output.contains("non-public IP"), "{}", ssrf.output);

    // CodingAgent::with_browser_fetch and with_browser_control pass bridges into ToolContext.
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = recorded.clone();
    let ctrl_recorded = Arc::new(Mutex::new(Vec::new()));
    let ctrl_clone = ctrl_recorded.clone();
    let (mock, prompts) = MockBackend::replies(vec![
        tool_block(
            "browser_fetch",
            json!({"url": "https://spa.example/", "account_id": "work", "wait_ms": 250, "max_chars": 500}),
        ),
        tool_block("browser_control", json!({"action": "help"})),
        tool_block(
            "browser_control",
            json!({"action": "open", "url": "https://reserve.example/"}),
        ),
        "Done".into(),
    ]);
    let mut agent = CodingAgent::new(mock, cfg(&ws))
        .with_browser_fetch(Arc::new(
            move |url, account, wait_ms, max_chars, _cancel| {
                rec_clone.lock().unwrap().push((
                    url.to_string(),
                    account.map(str::to_string),
                    wait_ms,
                    max_chars,
                ));
                Ok(crate::web::format_untrusted_web_page(
                    url,
                    None,
                    Some("SPA Title"),
                    "Dynamic content",
                    max_chars,
                ))
            },
        ))
        .with_browser_control(Arc::new(move |args, _cancel| {
            ctrl_clone.lock().unwrap().push(args.clone());
            Ok("TabID: btab_1\nRendered".into())
        }));
    let res = agent.run("Check SPA", &RunControl::default()).unwrap();
    assert_eq!(res.status, AgentStatus::Completed);
    assert_eq!(res.steps[0].observations.len(), 1);
    assert!(res.steps[0].observations[0].ok);
    assert!(res.steps[0].observations[0]
        .output
        .contains("[UNTRUSTED_WEB_CONTENT source=\"https://spa.example/\"]"));
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![("https://spa.example/".into(), Some("work".into()), 250, 500)]
    );
    assert_eq!(
        *ctrl_recorded.lock().unwrap(),
        vec![json!({"action": "open", "url": "https://reserve.example/"})]
    );
    let p = prompts.lock().unwrap();
    assert!(
        !p[0].contains("- web_search")
            && p[0].contains("- web_fetch(")
            && p[0].contains("- browser_fetch(")
            && p[0].contains("- browser_control(")
    );
    assert!(!p[0].contains("screenshot_id"));
    assert!(!p[0].contains("\"action\":\"open\""));
    assert!(p[2].contains("Browser control guide"));
    assert!(p[2].contains("Full browser_control argument schema"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn browser_actions_wait_for_help_from_an_earlier_model_turn_in_each_run() {
    let ws = temp_ws("browser-help");
    let actions = Arc::new(Mutex::new(Vec::new()));
    let observed = actions.clone();
    let open = json!({"action":"open","url":"https://example.com/"});
    let (mock, prompts) = MockBackend::replies(vec![
        tool_block("browser_control", open.clone()),
        format!(
            "[TOOL_CALL]\n{}\n[/TOOL_CALL]",
            json!({"tool_calls":[
                {"tool":"browser_control","args":{"action":"help"}},
                {"tool":"browser_control","args":open}
            ]})
        ),
        tool_block("browser_control", open.clone()),
        "Done".into(),
        tool_block("browser_control", open.clone()),
        "Done".into(),
    ]);
    let mut agent =
        CodingAgent::new(mock, cfg(&ws)).with_browser_control(Arc::new(move |args, _| {
            observed.lock().unwrap().push(args.clone());
            Ok("Opened page".into())
        }));
    let result = agent.run("Inspect a page", &RunControl::default()).unwrap();
    assert_eq!(result.status, AgentStatus::Completed);
    assert!(!result.steps[0].observations[0].ok);
    assert!(result.steps[0].observations[0]
        .output
        .contains("not executed"));
    assert!(result.steps[1].observations[0].ok);
    assert!(!result.steps[1].observations[1].ok);
    assert!(result.steps[2].observations[0].ok);
    assert!(prompts.lock().unwrap()[2].contains("Browser control guide"));
    assert_eq!(*actions.lock().unwrap(), vec![open.clone()]);

    let next = agent
        .run("Inspect another page", &RunControl::default())
        .unwrap();
    assert!(!next.steps[0].observations[0].ok);
    assert_eq!(*actions.lock().unwrap(), vec![open]);
    fs::remove_dir_all(ws).unwrap();
}

#[test]
fn spawn_agent_runs_parallel_workers_isolates_context_and_blocks_recursive_spawning() {
    let ws = temp_ws("spawn_agent");
    let reg = ToolRegistry::with_builtin_coding_tools();
    assert!(reg.contains("spawn_agent"));

    // Without a spawn_agent runner (e.g. inside a worker sub-agent), recursive calls are rejected.
    let ctx_worker = ToolContext::new(ws.clone());
    let blocked = reg.execute_call(
        &call("spawn_agent", json!({"task": "recursive work"})),
        &ctx_worker,
    );
    assert!(!blocked.ok);
    assert!(
        blocked.output.contains("unavailable") || blocked.output.contains("recursively"),
        "{}",
        blocked.output
    );

    // Temp-chat CodingAgent automatically removes spawn_agent from its registry.
    let (temp_mock, temp_prompts) = MockBackend::replies(vec!["done".into()]);
    let mut temp_cfg = cfg(&ws);
    temp_cfg.temp_chat = true;
    let mut temp_agent = CodingAgent::new(temp_mock, temp_cfg);
    temp_agent
        .run("sub-agent task", &RunControl::default())
        .unwrap();
    assert!(
        !temp_prompts.lock().unwrap()[0].contains("- spawn_agent"),
        "temp_chat worker must not advertise spawn_agent"
    );

    // Batched spawn_agent calls execute concurrently across threads and return compact summaries.
    let batched_calls = json!({
        "tool_calls": [
            {"tool": "spawn_agent", "args": {"label": "Trace auth", "task": "Inspect auth module", "read_only": true}},
            {"tool": "spawn_agent", "args": {"label": "Patch config", "task": "Update config defaults", "read_only": false}}
        ]
    });
    let (master_mock, master_prompts) = MockBackend::replies(vec![
        format!("[TOOL_CALL]\n{batched_calls}\n[/TOOL_CALL]"),
        "Combined both worker findings.".into(),
    ]);
    let active_workers = Arc::new(AtomicUsize::new(0));
    let peak_workers = Arc::new(AtomicUsize::new(0));
    let active_clone = active_workers.clone();
    let peak_clone = peak_workers.clone();

    let mut master = CodingAgent::new(master_mock, cfg(&ws)).with_spawn_agent(Arc::new(
        move |label, task, read_only, _cancel| {
            let cur = active_clone.fetch_add(1, Ordering::SeqCst) + 1;
            peak_clone.fetch_max(cur, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(120));
            active_clone.fetch_sub(1, Ordering::SeqCst);
            let files_changed = if read_only {
                Vec::new()
            } else {
                vec!["src/config.rs".to_string()]
            };
            Ok(SpawnAgentOutcome {
                summary: format!("[{label}] summary for '{task}' (read_only={read_only})"),
                files_changed,
            })
        },
    ));
    let res = master
        .run("Coordinate two sub-tasks", &RunControl::default())
        .unwrap();
    assert_eq!(res.status, AgentStatus::Completed);
    assert!(
        peak_workers.load(Ordering::SeqCst) >= 2,
        "batched spawn_agent calls must execute in parallel"
    );
    assert_eq!(res.files_changed, vec!["src/config.rs".to_string()]);
    let prompts = master_prompts.lock().unwrap();
    assert!(prompts[1].contains("[Trace auth] summary for 'Inspect auth module' (read_only=true)"));
    assert!(prompts[1]
        .contains("[Patch config] summary for 'Update config defaults' (read_only=false)"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn runtime_prompts_tool_overrides_and_agents_md_reload_without_recompile() {
    let ws = temp_ws("prompts_override");
    let prompts_dir = ws.join("custom_prompts");
    fs::create_dir_all(prompts_dir.join("tools")).unwrap();

    // 1. Seed starter files once; subsequent calls never overwrite existing user edits.
    let reg = ToolRegistry::with_builtin_coding_tools();
    let (seeded_dir, created) = init_prompt_files(Some(&prompts_dir), &reg).unwrap();
    assert_eq!(seeded_dir, prompts_dir);
    assert_eq!(created, vec!["system.md", "tools.toml", "subagent.md"]);

    // Modify tools.toml, add tools/grep_search.md (which takes precedence), customize system.md,
    // and place workspace AGENTS.md + .anticrumb/AGENTS.md.
    fs::write(
        prompts_dir.join("tools.toml"),
        "# Custom descriptions\n\
         [tools]\n\
         read_file = \"Read file via CUSTOM description.\"\n\
         grep_search = \"Should be overridden by tools/grep_search.md\"\n\
         edit_file = \"\"\"\nLine 1 custom edit.\nLine 2 strict exact match.\n\"\"\"\n",
    )
    .unwrap();
    fs::write(
        prompts_dir.join("tools/grep_search.md"),
        "Markdown override for grep_search.",
    )
    .unwrap();
    fs::write(
        prompts_dir.join("system.md"),
        "Custom persona in {{workspace}}.\n\n{{injected_instructions}}\n\n{{tools}}\n\n{{user_task}}",
    )
    .unwrap();
    fs::write(
        prompts_dir.join("subagent.md"),
        "Custom subagent worker directive.",
    )
    .unwrap();
    fs::write(ws.join("AGENTS.md"), "Always prefer zero-copy parsing.").unwrap();
    fs::create_dir_all(ws.join(".anticrumb")).unwrap();
    fs::write(
        ws.join(".anticrumb/AGENTS.md"),
        "Run cargo check before replying.",
    )
    .unwrap();

    // Running init_prompt_files again preserves custom files.
    let (_, second_created) = init_prompt_files(Some(&prompts_dir), &reg).unwrap();
    assert!(second_created.is_empty());

    assert_eq!(
        load_subagent_instructions(Some(&prompts_dir)),
        "Custom subagent worker directive."
    );

    let (mock, prompts) = MockBackend::replies(vec!["ok".into()]);
    let mut c = cfg(&ws);
    c.prompts_dir = Some(prompts_dir.clone());
    c.load_instructions = true;
    let mut agent = CodingAgent::new(mock, c.clone());
    agent
        .run("Check runtime prompts", &RunControl::default())
        .unwrap();

    let first = &prompts.lock().unwrap()[0];
    assert!(first.starts_with("Custom persona in "));
    assert!(first.contains("- read_file(path: str, start_line?: int, end_line?: int) \u{2014} Read file via CUSTOM description."));
    assert!(first.contains("- grep_search(query: str"));
    assert!(first.contains("\u{2014} Markdown override for grep_search."));
    assert!(first.contains("- edit_file(path: str, old_text: str, new_text: str, replace_all?: bool) \u{2014} Line 1 custom edit. Line 2 strict exact match."));
    assert!(first.contains("[PROJECT_INSTRUCTIONS source=\"AGENTS.md\"]\nAlways prefer zero-copy parsing.\n[/PROJECT_INSTRUCTIONS]"));
    assert!(first.contains("[PROJECT_INSTRUCTIONS source=\".anticrumb/AGENTS.md\"]\nRun cargo check before replying.\n[/PROJECT_INSTRUCTIONS]"));
    // Safety fallback keeps [TOOL_CALL] marker instructions even when omitted from custom system.md.
    assert!(first.contains("[TOOL_CALL]"));
    assert!(first.contains("Check runtime prompts"));

    // Live reload on next run without recompiling or rebuilding CodingAgent:
    fs::write(ws.join("AGENTS.md"), "Updated workspace directive v2.").unwrap();
    let (mock2, prompts2) = MockBackend::replies(vec!["ok".into()]);
    let mut agent2 = CodingAgent::new(mock2, c);
    agent2.run("Second task", &RunControl::default()).unwrap();
    let second = &prompts2.lock().unwrap()[0];
    assert!(second.contains("Updated workspace directive v2."));
    assert!(!second.contains("Always prefer zero-copy parsing."));

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn compact_tool_signatures_rendering_contract() {
    let reg = ToolRegistry::with_builtin_coding_tools();
    let block = reg.render_compact_tools_block(&Default::default());

    // 1. edit_file: required params first, optional ? suffix
    assert!(
        block.contains("- edit_file(path: str, old_text: str, new_text: str, replace_all?: bool) \u{2014} Replace exact old_text with new_text in a file."),
        "actual: {block}"
    );

    // 2. read_file: path required, start_line before end_line optional
    assert!(
        block.contains("- read_file(path: str, start_line?: int, end_line?: int) \u{2014} Read file lines (1-based start_line..end_line, default: first 500 lines)."),
        "actual: {block}"
    );

    // 3. browser_control: const literal in initial schema
    assert!(
        block.contains("- browser_control(action: \"help\") \u{2014} Interactive browser session. Call with {\"action\":\"help\"} alone first."),
        "actual: {block}"
    );

    // 4. web_fetch: enum union formatting
    assert!(
        block.contains("- web_fetch(url: str, format?: \"text\"|\"raw\", max_chars?: int) \u{2014} Fast local HTTP/HTTPS fetch (clean Markdown/text or raw, max_chars <= 16000)."),
        "actual: {block}"
    );

    // 5. grep_search: query required, optional params
    assert!(
        block.contains("- grep_search(query: str, case_sensitive?: bool, context_lines?: int, dir?: str, file_pattern?: str, max_matches?: int) \u{2014} Search literal text across workspace files (case-insensitive by default)."),
        "actual: {block}"
    );

    // 6. spawn_agent: task required, label/read_only optional
    assert!(
        block.contains("- spawn_agent(task: str, label?: str, read_only?: bool) \u{2014} Delegate isolated broad exploration or 2+ parallel sub-tasks to a worker tab (read_only=true by default)."),
        "actual: {block}"
    );

    // 7. Overrides and multiline collapse test
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert(
        "write_file".to_string(),
        "Line 1 custom description.\n   Line 2 indented notes.  \nLine 3 final.".to_string(),
    );
    let custom_block = reg.render_compact_tools_block(&overrides);
    assert!(
        custom_block.contains("- write_file(path: str, content: str) \u{2014} Line 1 custom description. Line 2 indented notes. Line 3 final."),
        "actual: {custom_block}"
    );

    // 8. Array and items type formatting test
    let custom_schema = json!({
        "type": "object",
        "required": ["pattern"],
        "properties": {
            "pattern": { "type": "string" },
            "include_globs": { "type": "array", "items": { "type": "string" } }
        }
    });
    let custom_sig = crate::agent::tools::format_tool_signature(
        "custom_grep",
        "Custom grep description",
        &custom_schema,
    );
    assert_eq!(
        custom_sig,
        "- custom_grep(pattern: str, include_globs?: list[str]) \u{2014} Custom grep description"
    );
}

#[test]
fn persistent_session_deduplicates_unchanged_read_file_and_invalidates_on_edit() {
    let ws = temp_ws("dedup_read");
    fs::write(ws.join("sample.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
    let (mock, prompts) = MockBackend::replies(vec![
        tool_block("read_file", json!({"path": "sample.rs"})),
        tool_block("read_file", json!({"path": "sample.rs"})),
        tool_block(
            "edit_file",
            json!({"path": "sample.rs", "old_text": "fn beta() {}", "new_text": "fn gamma() {}"}),
        ),
        tool_block("read_file", json!({"path": "sample.rs"})),
        "done".into(),
    ]);
    let mut agent = CodingAgent::new(mock, cfg(&ws));
    let res = agent
        .run("Inspect and update sample.rs", &RunControl::default())
        .unwrap();
    assert_eq!(res.status, AgentStatus::Completed);
    let p = prompts.lock().unwrap();
    assert!(p[1].contains("fn alpha() {}") && p[1].contains("fn beta() {}"));
    assert!(
        p[2].contains("[unchanged since earlier read in this session")
            && !p[2].contains("fn alpha() {}"),
        "actual step 2 prompt: {}",
        p[2]
    );
    assert!(
        p[4].contains("fn gamma() {}") && !p[4].contains("[unchanged"),
        "actual step 4 prompt: {}",
        p[4]
    );
    let _ = fs::remove_dir_all(&ws);
}
