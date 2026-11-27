//! Gemini UI mechanics. No localized button text is used for control decisions.
mod delete;
mod history;
mod input;
mod media;
#[cfg(test)]
mod mock_cdp;
mod models;
mod pages;
mod stream;
mod system_tab;
mod url;

pub use crate::domain::{ModelOption, ThinkingOption};
use crate::{
    agent::CancelToken,
    browser_host::{self, TargetInfo},
    cdp::CdpClient,
};
pub use delete::{
    clear_web_history, clear_web_history_with_store, delete_web_conversation,
    delete_web_conversation_with_store,
};
pub use history::list_web_conversations;
pub use input::AttachmentProgress;
pub(crate) use input::{
    evaluate_chip_poll, ChipPollState, CHIP_APPEAR_TIMEOUT, CHIP_UPLOAD_TIMEOUT,
};
pub use media::{read_web_usage, read_web_usage_checked};
pub use models::ModelCatalog;
pub(crate) use models::{
    model_probe_confirmed, normalize_model_label, probe_matches_model_aliases,
    probe_matches_thinking,
};
pub use pages::{fetch_page, BrowserFetchOutput};
pub(crate) use stream::{emit_observed, is_stream_finished, StreamWaitTracker};
pub use stream::{AskResult, StreamUpdate};
pub(crate) use system_tab::{navigate_tab_commit, SystemGeminiTab};
pub use url::is_reusable_gemini_url;
pub(crate) use url::{
    gemini_app_url, gemini_url_scope, gemini_usage_url, is_gemini_origin, is_gemini_page_url,
    is_gemini_usage_url_for, is_reusable_gemini_url_for, normalized_slot, rebase_gemini_chat_url,
    url_has_chat_id,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanPace {
    Fast,
    #[default]
    Low,
    High,
}

impl HumanPace {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "fast" | "direct" | "immediate" | "none" | "off" | "0" => Some(Self::Fast),
            "low" | "normal" | "balanced" | "1" => Some(Self::Low),
            "high" | "safe" | "human" | "cautious" | "2" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Low => "low",
            Self::High => "high",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Fast => "fast (no artificial pointer/typing delays; quickest)",
            Self::Low => "low (balanced human mouse arc & short pauses; default)",
            Self::High => "high (cautious multi-step mouse & longer human pauses)",
        }
    }
}

// 0 = unset, 1 = Fast, 2 = Low, 3 = High
static HUMAN_PACE_OVERRIDE: AtomicU8 = AtomicU8::new(0);
pub(crate) fn pace_override() -> Option<String> {
    (HUMAN_PACE_OVERRIDE.load(Ordering::SeqCst) > 0)
        .then(|| load_human_pace(None).as_str().to_owned())
}

pub fn set_human_pace(store: Option<&crate::store::Store>, pace: HumanPace) {
    let code = match pace {
        HumanPace::Fast => 1,
        HumanPace::Low => 2,
        HumanPace::High => 3,
    };
    HUMAN_PACE_OVERRIDE.store(code, Ordering::SeqCst);
    if let Some(store) = store {
        let _ = store.put(
            "local",
            "runtime",
            "human_pace",
            &json!({ "pace": pace.as_str() }),
        );
    }
}

pub fn load_human_pace(store: Option<&crate::store::Store>) -> HumanPace {
    match HUMAN_PACE_OVERRIDE.load(Ordering::SeqCst) {
        1 => return HumanPace::Fast,
        2 => return HumanPace::Low,
        3 => return HumanPace::High,
        _ => {}
    }
    if let Some(pace) = store
        .and_then(|s| s.get::<Value>("local", "runtime", "human_pace").ok())
        .and_then(|v| v["pace"].as_str().and_then(HumanPace::parse))
    {
        return pace;
    }
    HumanPace::Low
}

static STALLED_MOUSE_MOVE_PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();

pub(crate) fn is_mouse_move_stalled(port: u16) -> bool {
    port != 0
        && STALLED_MOUSE_MOVE_PORTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .is_ok_and(|set| set.contains(&port))
}

pub(crate) fn mark_mouse_move_stalled(port: u16) {
    if port != 0 {
        if let Ok(mut set) = STALLED_MOUSE_MOVE_PORTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
        {
            set.insert(port);
        }
    }
}

static JITTER_SEQ: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn next_rand_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let seq = JITTER_SEQ.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let mut x = nanos ^ seq.rotate_left(17);
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

pub(crate) fn jitter_u64(min: u64, max: u64) -> u64 {
    if max <= min {
        return min;
    }
    min + (next_rand_u64() % (max - min + 1))
}

pub(crate) fn jitter_f64() -> f64 {
    ((next_rand_u64() >> 11) as f64) / ((1u64 << 53) as f64)
}

#[derive(Serialize)]
pub struct AuthStatus {
    pub signed_in: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatSessionMode {
    NewChat,
    Continue,
    TempChat,
}

pub struct GeminiTab {
    pub(super) port: u16,
    pub(super) cdp: CdpClient,
    target: TargetInfo,
    pub(super) home_url: String,
    pub(super) input_lock: Option<Arc<Mutex<()>>>,
    pub(super) cancel: CancelToken,
    pub(super) deadline: Instant,
    pub(super) network: tokio::sync::broadcast::Receiver<Value>,
    pub(super) image_requests: std::collections::HashMap<String, String>,
    pub(super) cursor: Option<(f64, f64)>,
    pub(super) mouse_move_stalled: bool,
    pub(super) pace: HumanPace,
    pub(super) ready_probe: Value,
}

fn readiness_error(error: String, last_probe: &Value) -> String {
    let lower = error.to_ascii_lowercase();
    if !lower.contains("deadline") && !lower.contains("timed out") {
        return error;
    }
    let flag = |name: &str| match last_probe[name].as_bool() {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    };
    format!(
        "deadline exceeded: browser controls not ready (editor={}, models={}, signed_in={}, verification={}); show the browser window or use headless mode",
        flag("editor"),
        flag("models"),
        flag("signed_in"),
        flag("verification")
    )
}

impl GeminiTab {
    pub fn set_control(&mut self, cancel: CancelToken, deadline: Instant) {
        self.cancel = cancel.clone();
        self.deadline = deadline;
        self.cdp.set_control(cancel, deadline);
    }

    pub(super) fn sync_deadline(&mut self) {
        self.cdp.set_control(self.cancel.clone(), self.deadline);
    }

    pub fn set_pace(&mut self, pace: HumanPace) {
        self.pace = pace;
    }

    pub fn set_input_lock(&mut self, lock: Option<Arc<Mutex<()>>>) {
        self.input_lock = lock;
    }

    pub fn initial_probe(&self) -> &Value {
        &self.ready_probe
    }

    pub(super) fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let result = self.cdp.call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
        )?;
        if result.get("exceptionDetails").is_some() {
            return Err("adapter JavaScript failed; inspect the local adapter fixture".into());
        }
        Ok(result["result"]["value"].clone())
    }

    pub(super) fn action(&mut self, op: &str, args: Value) -> Result<Value, String> {
        let mut args = args;
        args["op"] = json!(op);
        self.eval(&format!(
            "({})({args})",
            crate::adapter::script("driver.js")?
        ))
    }

    pub(super) fn check(&self) -> Result<(), String> {
        self.cancel.check()?;
        if Instant::now() >= self.deadline {
            return Err("deadline exceeded".into());
        }
        Ok(())
    }

    pub(super) fn pause(&self, ms: u64) -> Result<(), String> {
        if self.cancel.sleep_or_cancelled(Duration::from_millis(ms)) {
            return Err("cancelled".into());
        }
        self.check()
    }

    pub(super) fn pause_jitter(&self, min_ms: u64, max_ms: u64) -> Result<(), String> {
        self.pause(jitter_u64(min_ms, max_ms))
    }

    pub(super) fn human_pause(&self, min_ms: u64, max_ms: u64) -> Result<(), String> {
        // Keep tight unit-test or near-expired deadlines unaffected.
        if self.deadline.saturating_duration_since(Instant::now()) < Duration::from_secs(3) {
            return self.check();
        }
        match self.pace {
            HumanPace::Fast => self.check(),
            HumanPace::Low => self.pause_jitter(min_ms, max_ms),
            HumanPace::High => {
                self.pause_jitter(min_ms.saturating_mul(5) / 2, max_ms.saturating_mul(3))
            }
        }
    }

    pub(super) fn probe(&mut self) -> Result<Value, String> {
        let value = self.action("probe", json!({}))?;
        if value["verification"] == true {
            return Err("verification required".into());
        }
        Ok(value)
    }

    pub fn wait_owned_response(&mut self, expected: u64) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            self.collect_image_requests();
            let probe = self.probe()?;
            if probe["response_count"].as_u64() == Some(expected) && probe["generating"] != true {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err("UI contract changed: saved response count differs".into());
            }
            self.pause(100)?;
        }
    }

    pub fn verify_owned_turns(&mut self, expected: u64) -> Result<(), String> {
        if self.probe()?["response_count"].as_u64() != Some(expected) {
            return Err(
                "UI contract changed: owned conversation was modified outside this service".into(),
            );
        }
        Ok(())
    }

    pub fn attach(
        port: u16,
        target: TargetInfo,
        mode: ChatSessionMode,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        Self::attach_with_home_url(
            port,
            target,
            browser_host::GEMINI_APP_URL,
            mode,
            cancel,
            deadline,
            capture_media,
        )
    }

    pub fn attach_with_home_url(
        port: u16,
        target: TargetInfo,
        home_url: &str,
        mode: ChatSessionMode,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        Self::attach_inner(
            port,
            target,
            home_url.to_owned(),
            mode,
            false,
            cancel,
            deadline,
            capture_media,
        )
    }

    /// Attach to an already-open Gemini tab, starting a fresh chat in-tab when requested
    /// instead of creating a new browser tab or window.
    pub fn attach_reused(
        port: u16,
        target: TargetInfo,
        mode: ChatSessionMode,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        Self::attach_reused_with_home_url(
            port,
            target,
            browser_host::GEMINI_APP_URL,
            mode,
            cancel,
            deadline,
            capture_media,
        )
    }

    pub fn attach_reused_with_home_url(
        port: u16,
        target: TargetInfo,
        home_url: &str,
        mode: ChatSessionMode,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        Self::attach_inner(
            port,
            target,
            home_url.to_owned(),
            mode,
            true,
            cancel,
            deadline,
            capture_media,
        )
    }

    fn wait_ready(&mut self) -> Result<Value, String> {
        let mut last_probe = Value::Null;
        let prepared: Result<Value, String> = (|| loop {
            self.check()?;
            last_probe = self.probe()?;
            if last_probe["editor"] == true && last_probe["models"] == true {
                return Ok(last_probe.clone());
            }
            if last_probe["url"]
                .as_str()
                .is_some_and(|u| u.contains("accounts.google.com"))
            {
                return Err("sign in required".into());
            }
            self.pause(100)?;
        })();
        prepared.map_err(|error| readiness_error(error, &last_probe))
    }

    fn reset_in_tab_to_new_chat(&mut self, want_persistent: bool) -> Result<Value, String> {
        let is_clean = |probe: &Value| {
            probe["editor"] == true
                && probe["models"] == true
                && probe["response_count"].as_u64().unwrap_or(0) == 0
                && !probe["url"].as_str().is_some_and(url_has_chat_id)
                && (!want_persistent || probe["temporary"] != true)
        };
        let initial = self.probe()?;
        if initial["generating"] == true {
            return Err("reused browser tab is generating; retry after completion".into());
        }
        if is_clean(&initial) {
            return Ok(initial);
        }
        let click_deadline = Instant::now() + Duration::from_secs(4);
        let mut next_click = Instant::now();
        while Instant::now() < click_deadline {
            self.check()?;
            let current = self.probe()?;
            if is_clean(&current) {
                return Ok(current);
            }
            if Instant::now() >= next_click {
                let clicked = self.human_click("new_chat", json!({}))?["ok"] == true;
                next_click = Instant::now() + Duration::from_millis(jitter_u64(380, 620));
                if clicked && Instant::now() >= click_deadline {
                    let post_click = Instant::now() + Duration::from_millis(600);
                    while Instant::now() < post_click {
                        self.pause(100)?;
                        let probed = self.probe()?;
                        if is_clean(&probed) {
                            return Ok(probed);
                        }
                    }
                }
            }
            self.pause_jitter(60, 110)?;
        }
        // Fallback: navigate within the exact same existing tab rather than opening a new tab.
        self.cdp
            .call("Page.navigate", json!({"url": self.home_url}))?;
        self.wait_ready()?;
        let settle_deadline = Instant::now() + Duration::from_secs(6);
        loop {
            self.check()?;
            let current = self.probe()?;
            if is_clean(&current) {
                return Ok(current);
            }
            if Instant::now() >= settle_deadline {
                return Err(
                    "UI contract changed: could not start a new chat in existing tab".into(),
                );
            }
            self.pause_jitter(60, 110)?;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_inner(
        port: u16,
        target: TargetInfo,
        home_url: String,
        mode: ChatSessionMode,
        reset_reused: bool,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        let cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
        let network = cdp.subscribe();
        let mut tab = Self {
            port,
            cdp,
            network,
            image_requests: std::collections::HashMap::new(),
            target,
            home_url,
            input_lock: None,
            cancel,
            deadline,
            cursor: None,
            mouse_move_stalled: is_mouse_move_stalled(port),
            pace: load_human_pace(None),
            ready_probe: Value::Null,
        };
        tab.cdp.set_control(tab.cancel.clone(), tab.deadline);
        tab.cdp.call("Runtime.enable", json!({}))?;
        let _ = tab.cdp.call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
        );
        if capture_media {
            tab.cdp.call(
                "Network.enable",
                json!({"maxTotalBufferSize":67108864,"maxResourceBufferSize":33554432}),
            )?;
        }
        tab.ready_probe = tab.wait_ready()?;
        if reset_reused && mode != ChatSessionMode::Continue {
            tab.ready_probe = tab.reset_in_tab_to_new_chat(mode == ChatSessionMode::NewChat)?;
        }
        if mode == ChatSessionMode::NewChat
            && tab.ready_probe["response_count"].as_u64().unwrap_or(0) != 0
        {
            return Err("new chat target already contains messages".into());
        }
        if mode != ChatSessionMode::Continue && tab.ready_probe["has_draft"] == true {
            // Gemini can restore a draft into a freshly created /app target.
            // Clear only this newly owned non-empty composer; continuations refuse unexpected drafts.
            tab.cdp.call("Page.bringToFront", json!({}))?;
            if tab.action("select_draft", json!({}))?["ok"] != true {
                return Err("UI contract changed: new composer unavailable".into());
            }
            for kind in ["keyDown", "keyUp"] {
                tab.cdp.call("Input.dispatchKeyEvent",json!({"type":kind,"key":"Backspace","code":"Backspace","windowsVirtualKeyCode":8}))?;
            }
        }
        if mode == ChatSessionMode::TempChat {
            let ready = Instant::now() + Duration::from_secs(12);
            while tab.human_click("temporary", json!({}))?["ok"] != true {
                if Instant::now() >= ready {
                    return Err("unsupported capability: temporary chat control unavailable".into());
                }
                tab.pause_jitter(90, 165)?;
            }
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                if tab.probe()?["temporary"] == true {
                    break;
                }
                if Instant::now() >= until {
                    return Err(
                        "UI contract changed: temporary chat activation not verified".into(),
                    );
                }
                tab.pause_jitter(90, 165)?;
            }
        }
        Ok(tab)
    }

    pub fn check_auth_status(&mut self) -> Result<AuthStatus, String> {
        let ready = Instant::now() + Duration::from_secs(8);
        loop {
            if self.probe()?["signed_in"] == true {
                return Ok(AuthStatus { signed_in: true });
            }
            if Instant::now() >= ready {
                return Ok(AuthStatus { signed_in: false });
            }
            self.pause(100)?;
        }
    }

    pub fn into_target(self) -> TargetInfo {
        self.target
    }

    /// A fresh CDP connection stops only this owned target after the original call was cancelled.
    pub fn stop_owned(port: u16, target: &str) -> Result<(), String> {
        let target = browser_host::list_targets(port)?
            .into_iter()
            .find(|t| t.id == target)
            .ok_or("owned target gone")?;
        let mut cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
        cdp.set_control(CancelToken::new(), Instant::now() + Duration::from_secs(2));
        cdp.call("Runtime.evaluate",json!({"expression":format!("({})({})",crate::adapter::script("driver.js")?,json!({"op":"stop"})),"returnByValue":true}))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::mock_cdp::MockCdpServer;

    #[test]
    fn attach_deadline_reports_last_readiness_flags_without_page_content() {
        let server = MockCdpServer::start_results(1, Duration::from_secs(2), |request, _| {
            if request["method"] == "Runtime.evaluate" {
                json!({"result":{"type":"object","value":{
                    "editor":true,"models":false,"signed_in":true,"verification":false,
                    "url":"https://gemini.google.com/app/private-path?token=private-token",
                    "private_account":"person@example.invalid"
                }}})
            } else {
                json!({})
            }
        });
        let result = GeminiTab::attach(
            server.port,
            server.gemini_target(),
            ChatSessionMode::NewChat,
            CancelToken::new(),
            Instant::now() + Duration::from_millis(500),
            false,
        );
        server.join();
        let error = result.err().unwrap();
        assert!(error.contains("editor=true, models=false, signed_in=true, verification=false"));
        assert!(error.contains("show the browser window or use headless mode"));
        assert!(!error.contains("private"));
        assert!(!error.contains("person@"));
        assert_eq!(
            crate::domain::ServiceError::backend(error, false).code,
            crate::domain::ErrorCode::DeadlineExceeded
        );
    }

    #[test]
    fn readiness_context_does_not_change_cancel_or_sign_in_errors() {
        for error in ["cancelled", "sign in required", "verification required"] {
            assert_eq!(readiness_error(error.into(), &Value::Null), error);
        }
        assert!(
            readiness_error("CDP deadline exceeded".into(), &Value::Null).contains(
                "editor=unknown, models=unknown, signed_in=unknown, verification=unknown"
            )
        );
        let submitted = crate::domain::ServiceError::backend("deadline exceeded".into(), true);
        assert_eq!(
            submitted.code,
            crate::domain::ErrorCode::SubmissionOutcomeUnknown
        );
        assert_eq!(submitted.retry, "after_reconcile");
        let verification =
            crate::domain::ServiceError::backend("verification required".into(), false);
        assert_eq!(
            verification.code,
            crate::domain::ErrorCode::VerificationRequired
        );
    }

    #[test]
    fn jitter_helpers_stay_within_bounds() {
        for _ in 0..64 {
            let v = jitter_u64(10, 25);
            assert!((10..=25).contains(&v), "out of bounds: {v}");
            assert_eq!(jitter_u64(42, 42), 42);
            let f = jitter_f64();
            assert!((0.0..1.0).contains(&f), "out of bounds: {f}");
        }
    }
}
