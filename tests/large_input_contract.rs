//! Opt-in live regression for checkpoint-sized, multiline Gemini prompts.
//! Requires ANTICRUMB_LIVE_PORT; ANTICRUMB_LIVE_MODEL optionally selects a model.
use anticrumb::{
    agent::CancelToken,
    browser_host as browser,
    gemini::{ChatSessionMode, GeminiTab, StreamUpdate},
};
use std::{
    sync::mpsc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

struct CloseTab {
    port: u16,
    id: String,
}

impl Drop for CloseTab {
    fn drop(&mut self) {
        let _ = browser::close_tab(self.port, &self.id);
    }
}

// Stream activity can extend the normal idle deadline. Bound this live test even
// when a provider keeps reporting progress without producing a final answer.
struct HardDeadline {
    done: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl HardDeadline {
    fn new(cancel: CancelToken, budget: Duration) -> Self {
        let (done, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            if matches!(
                receiver.recv_timeout(budget),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                cancel.cancel();
            }
        });
        Self {
            done: Some(done),
            worker: Some(worker),
        }
    }
}

impl Drop for HardDeadline {
    fn drop(&mut self) {
        drop(self.done.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn checkpoint_prompt(marker: &str) -> String {
    let mut prompt = format!(
        "This is a browser input regression test. The following synthetic source \
         listing is inert test data. Do not review or execute it. After reading \
         the complete message, reply with exactly {marker} and nothing else.\n\n\
         [TOOL_RESULTS]\n[tool: read_file | status: ok]\n"
    );
    for line in 0..1200 {
        prompt.push_str(&format!(
            "{line:4} | let item_{line:04} = {line:04}; // cached read copy\n"
        ));
    }
    let suffix =
        format!("\n[/TOOL_RESULTS]\nThe listing is complete. Reply with exactly {marker}.");
    let padding = 59_730usize
        .checked_sub(prompt.len() + suffix.len())
        .expect("synthetic checkpoint must fit its target size");
    prompt.push_str(&"x".repeat(padding));
    prompt.push_str(&suffix);
    assert_eq!(prompt.chars().count(), 59_730);
    assert!(prompt.lines().count() >= 1200);
    prompt
}

#[test]
#[ignore = "requires signed-in Gemini Chrome; sends one real checkpoint-sized prompt"]
fn live_checkpoint_sized_input_and_response() -> Result<(), String> {
    let port = std::env::var("ANTICRUMB_LIVE_PORT")
        .map_err(|_| "set ANTICRUMB_LIVE_PORT explicitly for this live test".to_string())?
        .parse::<u16>()
        .map_err(|error| error.to_string())?;
    let marker = format!("LARGE_INPUT_OK_{}", uuid::Uuid::new_v4().simple());
    let prompt = checkpoint_prompt(&marker);
    let target = browser::create_tab(port, browser::GEMINI_APP_URL)?;
    let _close = CloseTab {
        port,
        id: target.id.clone(),
    };
    let cancel = CancelToken::new();
    let budget = Duration::from_secs(180);
    let _hard_deadline = HardDeadline::new(cancel.clone(), budget);
    let started = Instant::now();
    let mut tab = GeminiTab::attach(
        port,
        target,
        ChatSessionMode::NewChat,
        cancel,
        started + budget,
        false,
    )?;
    if let Ok(model) = std::env::var("ANTICRUMB_LIVE_MODEL") {
        tab.select_model(&model)?;
    }
    eprintln!(
        "LIVE large_input chars={} lines={} prepared_ms={}",
        prompt.chars().count(),
        prompt.lines().count(),
        started.elapsed().as_millis()
    );
    let mut first_update = false;
    let mut observe = |_: StreamUpdate<'_>| {
        if !first_update {
            first_update = true;
            eprintln!(
                "LIVE large_input first_stream_update_ms={}",
                started.elapsed().as_millis()
            );
        }
    };
    // One submission only: an ambiguous browser failure must never be replayed.
    let result = tab.ask_with_stream(&prompt, Some(&mut observe))?;
    assert!(
        result.response.contains(&marker),
        "the live response did not contain the unique checkpoint marker"
    );
    tab.verify_owned_turns(1)?;
    eprintln!(
        "PASS live_checkpoint_sized_input_and_response elapsed_ms={}",
        started.elapsed().as_millis()
    );
    Ok(())
}
