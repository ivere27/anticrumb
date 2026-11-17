use super::GeminiTab;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamUpdate<'a> {
    Delta(&'a str),
    Thinking { summary: &'a str, elapsed_secs: u64 },
    Answering { chars: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ObservedStreamUpdate {
    Thinking { summary: String, elapsed_secs: u64 },
    Answering { chars: usize },
}

pub struct AskResult {
    pub response: String,
    pub model: Option<String>,
    pub chat_url: String,
}

pub(crate) struct StreamWaitTracker {
    idle_window: Duration,
    wait_start: Instant,
    hard_cap: Instant,
    last_thinking_text: String,
    last_thinking_len: u64,
    reported_thinking: bool,
    last_thinking_emit: Option<Instant>,
    reported_answering: bool,
}

impl StreamWaitTracker {
    pub(crate) fn new(idle_window: Duration, now: Instant) -> Self {
        let idle_window = idle_window.max(Duration::from_secs(1));
        let cap_secs = crate::domain::max_turn_timeout_secs(idle_window.as_secs().max(1));
        Self {
            idle_window,
            wait_start: now,
            hard_cap: now + Duration::from_secs(cap_secs),
            last_thinking_text: String::new(),
            last_thinking_len: 0,
            reported_thinking: false,
            last_thinking_emit: None,
            reported_answering: false,
        }
    }

    pub(crate) fn observe(
        &mut self,
        state: &Value,
        initial_resp_count: u64,
        text_changed: bool,
        now: Instant,
        deadline: &mut Instant,
    ) -> Option<ObservedStreamUpdate> {
        let resp_count = state["respCount"].as_u64().unwrap_or(0);
        let text = if resp_count > initial_resp_count {
            state["text"].as_str().unwrap_or("")
        } else {
            ""
        };
        let has_stop = state["hasStop"] == true;
        let aria_busy = state["ariaBusy"] == "true";
        let is_thinking = state["thinking"] == true;
        let thinking_text = state["thinkingText"].as_str().unwrap_or("").trim();
        let thinking_len = state["thinkingLen"].as_u64().unwrap_or(0);
        let summary_text = if thinking_text.is_empty() && (is_thinking || has_stop || aria_busy) {
            "Thinking..."
        } else {
            thinking_text
        };

        let thinking_changed = thinking_len > self.last_thinking_len
            || (!summary_text.is_empty() && summary_text != self.last_thinking_text);
        if thinking_len > self.last_thinking_len {
            self.last_thinking_len = thinking_len;
        }
        if !summary_text.is_empty() && summary_text != self.last_thinking_text {
            self.last_thinking_text = summary_text.to_owned();
        }

        let is_active = has_stop || aria_busy || is_thinking || text_changed || thinking_changed;
        if is_active {
            let extended = (now + self.idle_window).min(self.hard_cap);
            if extended > *deadline {
                *deadline = extended;
            }
        }

        if !text.is_empty() {
            if !self.reported_answering {
                self.reported_answering = true;
                return Some(ObservedStreamUpdate::Answering {
                    chars: text.chars().count(),
                });
            }
            return None;
        }

        if is_thinking || has_stop || aria_busy || thinking_changed {
            let throttled_change = thinking_changed
                && self
                    .last_thinking_emit
                    .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_millis(300));
            let heartbeat = self
                .last_thinking_emit
                .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_secs(10));
            if !self.reported_thinking || throttled_change || heartbeat {
                self.reported_thinking = true;
                self.last_thinking_emit = Some(now);
                let elapsed_secs = now.saturating_duration_since(self.wait_start).as_secs();
                let summary = if summary_text.is_empty() {
                    "Thinking...".to_owned()
                } else {
                    summary_text.to_owned()
                };
                return Some(ObservedStreamUpdate::Thinking {
                    summary,
                    elapsed_secs,
                });
            }
        }
        None
    }
}

const ACTION_BAR_SETTLE_MS: u64 = 500;
const IDLE_FALLBACK_SETTLE_SECS: u64 = 6;

pub(crate) fn is_stream_finished(state: &Value, stable_for: Duration) -> bool {
    if state["hasStop"] == true || state["ariaBusy"] == "true" {
        return false;
    }
    if state["hasActionBar"] == true {
        return stable_for >= Duration::from_millis(ACTION_BAR_SETTLE_MS);
    }
    if state["thinking"] == true {
        return false;
    }
    let has_text = !state["text"].as_str().unwrap_or("").is_empty();
    let media_ready = state["mediaReady"] == true;
    let media_pending = state["hasMedia"] == true && !media_ready;
    (has_text || media_ready)
        && !media_pending
        && stable_for >= Duration::from_secs(IDLE_FALLBACK_SETTLE_SECS)
}

pub(crate) fn emit_observed<F: FnMut(StreamUpdate<'_>)>(
    observed: Option<ObservedStreamUpdate>,
    updates: &mut Option<&mut F>,
) {
    let (Some(update), Some(callback)) = (observed, updates.as_deref_mut()) else {
        return;
    };
    match &update {
        ObservedStreamUpdate::Thinking {
            summary,
            elapsed_secs,
        } => callback(StreamUpdate::Thinking {
            summary,
            elapsed_secs: *elapsed_secs,
        }),
        ObservedStreamUpdate::Answering { chars } => {
            callback(StreamUpdate::Answering { chars: *chars })
        }
    }
}

impl GeminiTab {
    pub fn ask_with_stream<F: FnMut(StreamUpdate<'_>)>(
        &mut self,
        prompt: &str,
        mut updates: Option<&mut F>,
    ) -> Result<AskResult, String> {
        self.check()?;
        self.cdp.call("Page.bringToFront", json!({}))?;
        let initial = self.probe()?["response_count"].as_u64().unwrap_or(0);
        if self.action("prepare", json!({}))?["ok"] != true {
            return Err("UI contract changed: composer is not empty or ready".into());
        }
        let _ = self.human_click("editor", json!({}))?;
        self.human_pause(30, 75)?;
        self.cdp.call("Input.insertText", json!({"text":prompt}))?;
        let input_ready = Instant::now() + Duration::from_secs(2);
        loop {
            let checked = self.action("verify_input", json!({"text":prompt}))?;
            if checked["ok"] == true {
                break;
            }
            if Instant::now() >= input_ready {
                return Err(format!(
                    "UI contract changed: inserted prompt differs (expected {} chars, observed {})",
                    prompt.chars().count(),
                    checked["actual_length"]
                ));
            }
            self.pause_jitter(25, 55)?;
        }
        self.cdp
            .call("Runtime.addBinding", json!({"name":"geminiCdpChanged"}))?;
        self.eval(&crate::adapter::script("observer.js")?)?;
        let mut events = self.cdp.subscribe();
        self.human_pause(70, 160)?;
        let mut sent = false;
        for _ in 0..20 {
            if self.human_click("send", json!({}))?["ok"] == true {
                sent = true;
                break;
            }
            self.pause_jitter(50, 95)?;
        }
        if !sent {
            return Err("UI contract changed: send control is disabled".into());
        }
        let mut emitted = String::new();
        let mut stable = String::new();
        let mut since = Instant::now();
        let wait_start = Instant::now();
        let mut tracker = StreamWaitTracker::new(
            self.deadline.saturating_duration_since(wait_start),
            wait_start,
        );
        loop {
            self.check()?;
            self.collect_image_requests();
            let state = self.eval(&crate::adapter::script("extract-response.js")?)?;
            if state["verification"] == true {
                return Err("verification required".into());
            }
            let mut text_changed = false;
            let has_response = state["respCount"].as_u64().unwrap_or(0) > initial;
            let text = if has_response {
                let text = state["text"].as_str().unwrap_or("");
                if text.len() > 512 * 1024 {
                    return Err("response exceeds 512 KiB".into());
                }
                if text != stable {
                    stable = text.into();
                    since = Instant::now();
                    text_changed = true;
                }
                Some(text)
            } else {
                None
            };
            let prev_deadline = self.deadline;
            let observed = tracker.observe(
                &state,
                initial,
                text_changed,
                Instant::now(),
                &mut self.deadline,
            );
            if self.deadline != prev_deadline {
                self.sync_deadline();
            }
            emit_observed(observed, &mut updates);
            if let Some(text) = text {
                let finished = is_stream_finished(&state, since.elapsed());
                let end = if finished {
                    text.len()
                } else {
                    text.rfind('\n').unwrap_or(0)
                };
                if text.starts_with(&emitted) && end > emitted.len() {
                    if let Some(callback) = updates.as_deref_mut() {
                        callback(StreamUpdate::Delta(&text[emitted.len()..end]));
                    }
                    emitted = text[..end].into();
                }
                if finished && (!text.is_empty() || state["mediaReady"] == true) {
                    return Ok(AskResult {
                        response: text.into(),
                        model: state["modePill"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                        chat_url: state["url"].as_str().unwrap_or("").into(),
                    });
                }
            }
            let until = Instant::now() + Duration::from_millis(300);
            loop {
                match events.try_recv() {
                    Ok(e) if e["method"] == "Runtime.bindingCalled" => break,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                    Err(_) => break,
                }
                if Instant::now() >= until {
                    break;
                }
                self.pause(25)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::CancelToken,
        gemini::{mock_cdp::MockCdpServer, ChatSessionMode},
    };

    #[test]
    fn stream_wait_tracker_extends_deadline_during_active_thinking_and_stops_when_idle() {
        let start = Instant::now();
        let idle = Duration::from_secs(120);
        let mut deadline = start + idle;
        let mut tracker = StreamWaitTracker::new(idle, start);

        // Initial thinking observation at +5s extends deadline to +125s and emits Thinking.
        let t1 = start + Duration::from_secs(5);
        let u1 = tracker.observe(
            &json!({
                "respCount": 1,
                "hasStop": true,
                "ariaBusy": "true",
                "thinking": true,
                "thinkingText": "Analyzing Cargo.toml",
                "thinkingLen": 20,
                "text": ""
            }),
            0,
            false,
            t1,
            &mut deadline,
        );
        assert_eq!(
            u1,
            Some(ObservedStreamUpdate::Thinking {
                summary: "Analyzing Cargo.toml".into(),
                elapsed_secs: 5
            })
        );
        assert_eq!(deadline, t1 + idle);

        // At +100s, still thinking with updated thoughts -> extends deadline past initial 120s (to 220s).
        let t2 = start + Duration::from_secs(100);
        let u2 = tracker.observe(
            &json!({
                "respCount": 1,
                "hasStop": true,
                "ariaBusy": "true",
                "thinking": true,
                "thinkingText": "Checking build.rs dependencies",
                "thinkingLen": 65,
                "text": ""
            }),
            0,
            false,
            t2,
            &mut deadline,
        );
        assert_eq!(
            u2,
            Some(ObservedStreamUpdate::Thinking {
                summary: "Checking build.rs dependencies".into(),
                elapsed_secs: 100
            })
        );
        assert_eq!(deadline, t2 + idle);

        // Idle/hung page (hasStop=false, thinking=false, no text change) never extends deadline.
        let frozen_deadline = deadline;
        let t3 = start + Duration::from_secs(150);
        let u3 = tracker.observe(
            &json!({
                "respCount": 0,
                "hasStop": false,
                "ariaBusy": "",
                "thinking": false,
                "thinkingText": "",
                "thinkingLen": 0,
                "text": ""
            }),
            0,
            false,
            t3,
            &mut deadline,
        );
        assert_eq!(u3, None);
        assert_eq!(deadline, frozen_deadline);

        // Even before hard_cap (1800s), active answering emits Answering once and is capped cleanly.
        let t4 = start + Duration::from_secs(1790);
        let u4 = tracker.observe(
            &json!({
                "respCount": 1,
                "hasStop": true,
                "ariaBusy": "true",
                "thinking": false,
                "thinkingText": "",
                "thinkingLen": 65,
                "text": "Here is the summary"
            }),
            0,
            true,
            t4,
            &mut deadline,
        );
        assert_eq!(u4, Some(ObservedStreamUpdate::Answering { chars: 19 }));
        assert_eq!(deadline, start + Duration::from_secs(1800));
    }

    #[test]
    fn stream_wait_tracker_surfaces_transient_thinking_and_clears() {
        let start = Instant::now();
        let idle = Duration::from_secs(60);
        let mut deadline = start + idle;
        let mut tracker = StreamWaitTracker::new(idle, start);

        let t1 = start + Duration::from_millis(100);
        let u1 = tracker.observe(
            &json!({"respCount": 1, "hasStop": true, "thinking": true, "thinkingText": "Reading Cargo.toml", "thinkingLen": 18, "text": ""}),
            0,
            false,
            t1,
            &mut deadline,
        );
        assert_eq!(
            u1,
            Some(ObservedStreamUpdate::Thinking {
                summary: "Reading Cargo.toml".into(),
                elapsed_secs: 0
            })
        );

        // Clearing back to empty while still thinking emits "Thinking..." once 300ms elapsed.
        let t2 = start + Duration::from_millis(450);
        let u2 = tracker.observe(
            &json!({"respCount": 1, "hasStop": true, "thinking": true, "thinkingText": "", "thinkingLen": 18, "text": ""}),
            0,
            false,
            t2,
            &mut deadline,
        );
        assert_eq!(
            u2,
            Some(ObservedStreamUpdate::Thinking {
                summary: "Thinking...".into(),
                elapsed_secs: 0
            })
        );

        // Next transient thought line surfaces right away after 300ms throttle.
        let t3 = start + Duration::from_millis(800);
        let u3 = tracker.observe(
            &json!({"respCount": 1, "hasStop": true, "thinking": true, "thinkingText": "Checking src/main.rs", "thinkingLen": 38, "text": ""}),
            0,
            false,
            t3,
            &mut deadline,
        );
        assert_eq!(
            u3,
            Some(ObservedStreamUpdate::Thinking {
                summary: "Checking src/main.rs".into(),
                elapsed_secs: 0
            })
        );
    }

    #[test]
    fn ask_with_stream_syncs_cdp_deadline_when_thinking_extends_past_initial_timeout() {
        let mut extract_polls = 0usize;
        let mut prompt_verified = false;
        let server = MockCdpServer::start_results(1, Duration::from_secs(5), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            if method == "Runtime.evaluate" {
                let expr = request["params"]["expression"].as_str().unwrap_or("");
                if expr.contains(r#""op":"probe""#) {
                    json!({"result":{"type":"object","value":{
                        "editor":true,"models":true,"signed_in":true,"verification":false,
                        "url":"https://gemini.google.com/app",
                        "response_count": 0
                    }}})
                } else if expr.contains(r#""op":"prepare""#) {
                    json!({"result":{"type":"object","value":{"ok":true}}})
                } else if expr.contains(r#""op":"verify_input""#) {
                    prompt_verified = true;
                    json!({"result":{"type":"object","value":{"ok":true,"actual_length":5}}})
                } else if expr.contains(r#""target":"editor""#) {
                    json!({"result":{"type":"object","value":{"ok":true,"x":10.0,"y":10.0,"width":40.0,"height":20.0}}})
                } else if expr.contains(r#""target":"send""#) {
                    json!({"result":{"type":"object","value":{"ok":true,"x":20.0,"y":20.0,"width":40.0,"height":20.0}}})
                } else if expr.contains("respCount") && prompt_verified {
                    extract_polls += 1;
                    if extract_polls < 4 {
                        // 3 thinking polls * 750ms > 1.8s initial deadline (1.5s) -> requires synced CDP deadline.
                        json!({"result":{"type":"object","value":{
                            "url":"https://gemini.google.com/app/chat_1",
                            "respCount":1,
                            "hasStop":true,
                            "ariaBusy":"true",
                            "hasActionBar":false,
                            "hasMedia":false,
                            "mediaReady":false,
                            "thinking":true,
                            "thinkingText": format!("Step {extract_polls}"),
                            "thinkingLen": extract_polls * 10,
                            "modePill":"Pro",
                            "text":""
                        }}})
                    } else {
                        json!({"result":{"type":"object","value":{
                            "url":"https://gemini.google.com/app/chat_1",
                            "respCount":1,
                            "hasStop":false,
                            "ariaBusy":"false",
                            "hasActionBar":true,
                            "hasMedia":false,
                            "mediaReady":false,
                            "thinking":false,
                            "thinkingText":"",
                            "thinkingLen": 30,
                            "modePill":"Pro",
                            "text":"Done thinking"
                        }}})
                    }
                } else {
                    json!({"result":{"type":"object","value":{}}})
                }
            } else {
                json!({})
            }
        });
        let mut tab = GeminiTab::attach(
            server.port,
            server.gemini_target(),
            ChatSessionMode::Continue,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(10),
            false,
        )
        .unwrap();

        // Set tight initial deadline (1500ms) right before asking; 3 thinking polls * 750ms (~2.25s)
        // must survive past 1500ms without hitting CDP deadline exceeded.
        tab.set_control(
            CancelToken::new(),
            Instant::now() + Duration::from_millis(1500),
        );
        let mut updates = Vec::new();
        let mut on_update = |u: StreamUpdate<'_>| match u {
            StreamUpdate::Thinking { summary, .. } => updates.push(summary.to_owned()),
            StreamUpdate::Answering { .. } => updates.push("ANSWERING".into()),
            StreamUpdate::Delta(_) => {}
        };
        let res = tab.ask_with_stream("hello", Some(&mut on_update)).unwrap();
        assert_eq!(res.response, "Done thinking");
        assert!(updates.iter().any(|u| u.starts_with("Step ")));

        drop(tab);
        server.join();
    }

    #[test]
    fn stream_finished_handles_action_bar_and_idle_fallback() {
        let with_bar = json!({
            "hasStop": false,
            "ariaBusy": "",
            "hasActionBar": true,
            "thinking": false,
            "hasMedia": false,
            "mediaReady": false,
            "text": "Line 1\nFinal line"
        });
        assert!(!is_stream_finished(&with_bar, Duration::from_millis(400)));
        assert!(is_stream_finished(&with_bar, Duration::from_millis(500)));

        let without_bar = json!({
            "hasStop": false,
            "ariaBusy": "",
            "hasActionBar": false,
            "thinking": false,
            "hasMedia": false,
            "mediaReady": false,
            "text": "Line 1\nFinal line"
        });
        assert!(!is_stream_finished(&without_bar, Duration::from_secs(5)));
        assert!(is_stream_finished(&without_bar, Duration::from_secs(6)));

        // Active indicators must keep blocking fallback even past 6s.
        assert!(!is_stream_finished(
            &json!({"hasStop": true, "ariaBusy": "", "hasActionBar": false, "thinking": false, "text": "x"}),
            Duration::from_secs(30)
        ));
        assert!(!is_stream_finished(
            &json!({"hasStop": false, "ariaBusy": "true", "hasActionBar": false, "thinking": false, "text": "x"}),
            Duration::from_secs(30)
        ));
        assert!(!is_stream_finished(
            &json!({"hasStop": false, "ariaBusy": "", "hasActionBar": false, "thinking": true, "text": "x"}),
            Duration::from_secs(30)
        ));
        // Empty text or pending media must not trigger idle fallback.
        assert!(!is_stream_finished(
            &json!({"hasStop": false, "ariaBusy": "", "hasActionBar": false, "thinking": false, "text": ""}),
            Duration::from_secs(30)
        ));
        assert!(!is_stream_finished(
            &json!({"hasStop": false, "ariaBusy": "", "hasActionBar": false, "thinking": false, "hasMedia": true, "mediaReady": false, "text": "Rendering"}),
            Duration::from_secs(30)
        ));
        // Completed media without action bar can settle after fallback threshold.
        assert!(is_stream_finished(
            &json!({"hasStop": false, "ariaBusy": "", "hasActionBar": false, "thinking": false, "hasMedia": true, "mediaReady": true, "text": ""}),
            Duration::from_secs(6)
        ));
    }
}
