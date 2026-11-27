//! Generic CDP WebChat runner for runtime web providers.
//!
//! Orchestrates browser-level primitives (`tab_pool`, `Page.navigate`, trusted
//! pointer clicks, `Input.insertText`, `DOM.setFileInputFiles`, `Runtime.addBinding`,
//! cancellation/deadlines, and media extraction) while delegating DOM selectors and
//! markdown extraction to provider-supplied in-page scripts (`driver.js`,
//! `observer.js`, `extract-response.js`, and optional `usage.js`).

use crate::{
    agent::CancelToken,
    browser_host::{self, TargetInfo},
    cdp::CdpClient,
    domain::{ModelOption, ThinkingOption},
    gemini::{
        emit_observed, evaluate_chip_poll, is_mouse_move_stalled, is_stream_finished, jitter_f64,
        jitter_u64, mark_mouse_move_stalled, model_probe_confirmed, AskResult, AttachmentProgress,
        AuthStatus, ChatSessionMode, ChipPollState, HumanPace, ModelCatalog, StreamUpdate,
        StreamWaitTracker, CHIP_APPEAR_TIMEOUT, CHIP_UPLOAD_TIMEOUT,
    },
    tab_pool::TabPoolMatcher,
    url_lite::Url,
};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub const DEFAULT_OBSERVER_BINDING: &str = "anticrumbCdpChanged";

pub type ScriptResolver =
    Arc<dyn Fn(&str) -> std::result::Result<Cow<'static, str>, String> + Send + Sync>;

#[derive(Clone)]
pub struct WebChatConfig {
    pub provider_id: String,
    pub provider_name: String,
    pub entry_url: String,
    pub allowed_origins: Vec<String>,
    pub auth_url_substrings: Vec<String>,
    pub chat_url_prefixes: Vec<String>,
    pub reusable_path_prefixes: Vec<String>,
    pub observer_binding: String,
    pub requires_models_control: bool,
    pub usage_url: Option<String>,
    pub resolve_script: ScriptResolver,
}

impl WebChatConfig {
    pub fn url_matches_origin(&self, raw_url: &str) -> bool {
        let Ok(url) = Url::parse(raw_url) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return false;
        }
        let target_origin = url.origin().ascii_serialization();
        if !self.allowed_origins.is_empty() {
            return self.allowed_origins.iter().any(|allowed| {
                Url::parse(allowed)
                    .map(|u| u.origin().ascii_serialization() == target_origin)
                    .unwrap_or(false)
            });
        }
        Url::parse(&self.entry_url)
            .map(|entry| entry.origin().ascii_serialization() == target_origin)
            .unwrap_or(false)
    }

    pub fn is_auth_url(&self, raw_url: &str) -> bool {
        self.auth_url_substrings
            .iter()
            .any(|needle| !needle.is_empty() && raw_url.contains(needle.as_str()))
    }

    pub fn is_chat_url(&self, raw_url: &str) -> bool {
        if !self.url_matches_origin(raw_url) {
            return false;
        }
        let Ok(parsed) = Url::parse(raw_url) else {
            return false;
        };
        let normalized = format!("{}{}", parsed.origin().ascii_serialization(), parsed.path());
        self.chat_url_prefixes.iter().any(|prefix| {
            Url::parse(prefix)
                .map(|p| {
                    let expected = format!("{}{}", p.origin().ascii_serialization(), p.path());
                    normalized.starts_with(&expected) && normalized.len() > expected.len()
                })
                .unwrap_or_else(|_| raw_url.starts_with(prefix))
        })
    }

    pub fn is_system_page_url(&self, raw_url: &str) -> bool {
        let Some(usage_url) = self.usage_url.as_deref() else {
            return false;
        };
        let (Ok(target), Ok(usage)) = (Url::parse(raw_url), Url::parse(usage_url)) else {
            return false;
        };
        target.origin().ascii_serialization() == usage.origin().ascii_serialization()
            && target.path().starts_with(usage.path())
    }

    pub fn is_reusable_url(&self, raw_url: &str) -> bool {
        if !self.url_matches_origin(raw_url) || self.is_auth_url(raw_url) {
            return false;
        }
        if self.is_system_page_url(raw_url) {
            return false;
        }
        if self.reusable_path_prefixes.is_empty() {
            return true;
        }
        let Ok(parsed) = Url::parse(raw_url) else {
            return false;
        };
        let path = parsed.path();
        self.reusable_path_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix.as_str()))
    }
}

/// Evaluates `TabPoolMatcher` cleanly without leaking closures.
pub(crate) fn with_tab_pool_matcher<R>(
    config: &WebChatConfig,
    f: impl FnOnce(&TabPoolMatcher<'_>) -> R,
) -> R {
    let is_reusable = |u: &str| config.is_reusable_url(u);
    let is_chat = |u: &str| config.is_chat_url(u);
    let is_system = |u: &str| config.is_system_page_url(u);
    let matcher = TabPoolMatcher {
        entry_url: &config.entry_url,
        is_reusable_url: &is_reusable,
        is_chat_url: &is_chat,
        is_system_page_url: &is_system,
    };
    f(&matcher)
}

pub struct WebChatTab {
    port: u16,
    cdp: CdpClient,
    target: TargetInfo,
    config: Arc<WebChatConfig>,
    input_lock: Option<Arc<std::sync::Mutex<()>>>,
    cancel: CancelToken,
    deadline: Instant,
    network: tokio::sync::broadcast::Receiver<Value>,
    image_requests: HashMap<String, String>,
    cursor: Option<(f64, f64)>,
    mouse_move_stalled: bool,
    pace: HumanPace,
    ready_probe: Value,
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

impl WebChatTab {
    pub fn set_control(&mut self, cancel: CancelToken, deadline: Instant) {
        self.cancel = cancel.clone();
        self.deadline = deadline;
        self.cdp.set_control(cancel, deadline);
    }

    fn sync_deadline(&mut self) {
        self.cdp.set_control(self.cancel.clone(), self.deadline);
    }

    pub fn set_pace(&mut self, pace: HumanPace) {
        self.pace = pace;
    }

    pub fn set_input_lock(&mut self, lock: Option<Arc<std::sync::Mutex<()>>>) {
        self.input_lock = lock;
    }

    pub fn initial_probe(&self) -> &Value {
        &self.ready_probe
    }

    pub fn into_target(self) -> TargetInfo {
        self.target
    }

    fn script(&self, name: &str) -> Result<Cow<'static, str>, String> {
        (self.config.resolve_script)(name)
    }

    pub fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let result = self.cdp.call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
        )?;
        if result.get("exceptionDetails").is_some() {
            return Err(format!(
                "adapter JavaScript failed for provider '{}'; inspect provider scripts",
                self.config.provider_id
            ));
        }
        Ok(result["result"]["value"].clone())
    }

    pub fn action(&mut self, op: &str, args: Value) -> Result<Value, String> {
        let mut args = args;
        args["op"] = json!(op);
        let driver = self.script("driver.js")?;
        self.eval(&format!("({driver})({args})"))
    }

    fn check(&self) -> Result<(), String> {
        self.cancel.check()?;
        if Instant::now() >= self.deadline {
            return Err("deadline exceeded".into());
        }
        Ok(())
    }

    fn pause(&self, ms: u64) -> Result<(), String> {
        if self.cancel.sleep_or_cancelled(Duration::from_millis(ms)) {
            return Err("cancelled".into());
        }
        self.check()
    }

    fn pause_jitter(&self, min_ms: u64, max_ms: u64) -> Result<(), String> {
        self.pause(jitter_u64(min_ms, max_ms))
    }

    fn human_pause(&self, min_ms: u64, max_ms: u64) -> Result<(), String> {
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

    pub fn probe(&mut self) -> Result<Value, String> {
        let value = self.action("probe", json!({}))?;
        if value["verification"] == true {
            return Err("verification required".into());
        }
        Ok(value)
    }

    fn probe_is_saved_chat(&self, probe: &Value) -> bool {
        probe["saved_chat"] == true
            || probe["url"]
                .as_str()
                .is_some_and(|u| self.config.is_chat_url(u))
    }

    fn wait_ready(&mut self) -> Result<Value, String> {
        let mut last_probe = Value::Null;
        let prepared: Result<Value, String> = (|| loop {
            self.check()?;
            last_probe = self.probe()?;
            let editor_ready = last_probe["editor"] == true;
            let models_ready = !self.config.requires_models_control || last_probe["models"] == true;
            if editor_ready && models_ready {
                return Ok(last_probe.clone());
            }
            if last_probe["url"]
                .as_str()
                .is_some_and(|u| self.config.is_auth_url(u))
            {
                return Err("sign in required".into());
            }
            self.pause(100)?;
        })();
        prepared.map_err(|error| readiness_error(error, &last_probe))
    }

    fn reset_in_tab_to_new_chat(&mut self, want_persistent: bool) -> Result<Value, String> {
        let is_clean = |tab: &Self, probe: &Value| {
            probe["editor"] == true
                && (!tab.config.requires_models_control || probe["models"] == true)
                && probe["response_count"].as_u64().unwrap_or(0) == 0
                && !tab.probe_is_saved_chat(probe)
                && (!want_persistent || probe["temporary"] != true)
        };
        let initial = self.probe()?;
        if initial["generating"] == true {
            return Err("reused browser tab is generating; retry after completion".into());
        }
        if is_clean(self, &initial) {
            return Ok(initial);
        }
        let click_deadline = Instant::now() + Duration::from_secs(4);
        let mut next_click = Instant::now();
        while Instant::now() < click_deadline {
            self.check()?;
            let current = self.probe()?;
            if is_clean(self, &current) {
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
                        if is_clean(self, &probed) {
                            return Ok(probed);
                        }
                    }
                }
            }
            self.pause_jitter(60, 110)?;
        }
        self.cdp
            .call("Page.navigate", json!({"url": self.config.entry_url}))?;
        self.wait_ready()?;
        let settle_deadline = Instant::now() + Duration::from_secs(6);
        loop {
            self.check()?;
            let current = self.probe()?;
            if is_clean(self, &current) {
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

    pub fn attach(
        port: u16,
        target: TargetInfo,
        mode: ChatSessionMode,
        config: Arc<WebChatConfig>,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        Self::attach_inner(
            port,
            target,
            mode,
            false,
            config,
            cancel,
            deadline,
            capture_media,
        )
    }

    pub fn attach_reused(
        port: u16,
        target: TargetInfo,
        mode: ChatSessionMode,
        config: Arc<WebChatConfig>,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        Self::attach_inner(
            port,
            target,
            mode,
            true,
            config,
            cancel,
            deadline,
            capture_media,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_inner(
        port: u16,
        target: TargetInfo,
        mode: ChatSessionMode,
        reset_reused: bool,
        config: Arc<WebChatConfig>,
        cancel: CancelToken,
        deadline: Instant,
        capture_media: bool,
    ) -> Result<Self, String> {
        let cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
        let network = cdp.subscribe();
        let mut tab = Self {
            port,
            cdp,
            target,
            config,
            input_lock: None,
            cancel,
            deadline,
            network,
            image_requests: HashMap::new(),
            cursor: None,
            mouse_move_stalled: is_mouse_move_stalled(port),
            pace: HumanPace::Low,
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
            tab.cdp.call("Page.bringToFront", json!({}))?;
            if tab.action("select_draft", json!({}))?["ok"] != true {
                return Err("UI contract changed: new composer unavailable".into());
            }
            for kind in ["keyDown", "keyUp"] {
                tab.cdp.call(
                    "Input.dispatchKeyEvent",
                    json!({"type":kind,"key":"Backspace","code":"Backspace","windowsVirtualKeyCode":8}),
                )?;
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

    pub fn verify_owned_turns(&mut self, expected: u64) -> Result<(), String> {
        if self.probe()?["response_count"].as_u64() != Some(expected) {
            return Err(
                "UI contract changed: owned conversation was modified outside this service".into(),
            );
        }
        Ok(())
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

    pub fn stop_owned(port: u16, target: &str, config: &WebChatConfig) -> Result<(), String> {
        let target = browser_host::list_targets(port)?
            .into_iter()
            .find(|t| t.id == target)
            .ok_or("owned target gone")?;
        let driver = (config.resolve_script)("driver.js")?;
        let mut cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
        cdp.set_control(CancelToken::new(), Instant::now() + Duration::from_secs(2));
        cdp.call(
            "Runtime.evaluate",
            json!({
                "expression": format!("({})({})", driver, json!({"op": "stop"})),
                "returnByValue": true
            }),
        )?;
        Ok(())
    }

    fn dispatch_mouse(
        &mut self,
        kind: &str,
        x: f64,
        y: f64,
        button: &str,
        click_count: i64,
        buttons: i64,
    ) -> Result<(), String> {
        self.cdp.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": kind,
                "x": x.round(),
                "y": y.round(),
                "button": button,
                "clickCount": click_count,
                "buttons": buttons,
                "pointerType": "mouse",
            }),
        )?;
        Ok(())
    }

    pub fn human_click(&mut self, op: &str, args: Value) -> Result<Value, String> {
        let mut rect_args = args.clone();
        rect_args["op"] = json!("target_rect");
        rect_args["target"] = json!(op);
        let driver = self.script("driver.js")?;
        let rect = self.eval(&format!("({driver})({rect_args})"))?;
        if rect["already_open"] == true || rect["already_active"] == true {
            return Ok(rect);
        }
        if op == "thinking_open" && rect["ok"] == true && rect["available"] == false {
            return Ok(json!({"ok": true, "available": false}));
        }
        let (Some(x), Some(y), Some(w), Some(h)) = (
            rect["x"].as_f64(),
            rect["y"].as_f64(),
            rect["width"].as_f64(),
            rect["height"].as_f64(),
        ) else {
            return self.action(op, args);
        };
        if rect["ok"] != true || w < 2.0 || h < 2.0 {
            return self.action(op, args);
        }
        let tx = (x + w * (0.25 + 0.5 * jitter_f64())).max(1.0);
        let ty = (y + h * (0.25 + 0.5 * jitter_f64())).max(1.0);
        let (sx, sy) = self.cursor.unwrap_or_else(|| {
            (
                (tx - 90.0 + 180.0 * jitter_f64()).max(1.0),
                (ty + 40.0 + 90.0 * jitter_f64()).max(1.0),
            )
        });
        let arc = (jitter_f64() - 0.5) * 28.0;
        let steps = match self.pace {
            HumanPace::Fast => 0usize,
            HumanPace::Low => 3usize,
            HumanPace::High => 4usize,
        };
        if !self.mouse_move_stalled && steps > 0 {
            for step in 1..=steps {
                let t = (step as f64) / (steps as f64);
                let curve = (t * std::f64::consts::PI).sin() * arc;
                let px = sx + (tx - sx) * t + curve;
                let py = sy + (ty - sy) * t - curve * 0.5;
                let move_started = Instant::now();
                self.dispatch_mouse("mouseMoved", px.max(1.0), py.max(1.0), "none", 0, 0)?;
                if move_started.elapsed() >= Duration::from_millis(500) {
                    self.mouse_move_stalled = true;
                    mark_mouse_move_stalled(self.port);
                    break;
                }
                self.human_pause(10, 26)?;
            }
        }
        self.human_pause(18, 45)?;
        self.dispatch_mouse("mousePressed", tx, ty, "left", 1, 1)?;
        self.human_pause(30, 75)?;
        self.dispatch_mouse("mouseReleased", tx, ty, "left", 1, 0)?;
        self.cursor = Some((tx, ty));
        Ok(if op == "thinking_open" {
            json!({"ok": true, "available": true})
        } else {
            json!({"ok": true})
        })
    }

    pub fn attach_files<F: FnMut(&AttachmentProgress)>(
        &mut self,
        files: &[PathBuf],
        mut progress: Option<&mut F>,
    ) -> Result<(), String> {
        if files.len() > crate::input::MAX_FILES {
            return Err(format!("maximum {} attachments", crate::input::MAX_FILES));
        }
        for file in files {
            let file = std::fs::canonicalize(file).map_err(|e| e.to_string())?;
            let name = file
                .file_name()
                .ok_or("missing filename")?
                .to_string_lossy()
                .to_string();
            if std::fs::metadata(&file).map_err(|e| e.to_string())?.len()
                > crate::input::MAX_FILE_BYTES
            {
                return Err(format!(
                    "attachment exceeds {}",
                    crate::domain::MAX_ARTIFACT_LABEL
                ));
            }
            let extension = file
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let image = ["png", "jpg", "jpeg", "webp", "gif"].contains(&extension.as_str());
            let file_ready = Instant::now() + Duration::from_secs(10);
            let mut next_open = Instant::now();
            let mut next_reset = Instant::now() + Duration::from_millis(1600);
            let mut opened = false;
            let object = loop {
                self.check()?;
                let now = Instant::now();
                if now >= next_open {
                    let reset = now >= next_reset;
                    let open_res = self.human_click("uploads_open", json!({"reset": reset}))?;
                    opened |= open_res["ok"] == true;
                    if reset {
                        next_reset = now + Duration::from_millis(jitter_u64(1500, 1950));
                    }
                    next_open = now + Duration::from_millis(jitter_u64(380, 560));
                }
                let driver = self.script("driver.js")?;
                let expression =
                    format!("({})({})", driver, json!({"op":"file_input","image":image}));
                let value = self.cdp.call(
                    "Runtime.evaluate",
                    json!({"expression": expression, "returnByValue": false}),
                )?;
                if let Some(object) = value["result"]["objectId"].as_str() {
                    break object.to_string();
                }
                if Instant::now() >= file_ready {
                    self.close_overlays();
                    return Err(if opened {
                        "UI contract changed: file input unavailable"
                    } else {
                        "UI contract changed: upload control unavailable"
                    }
                    .into());
                }
                self.pause(100)?;
            };
            let before_chips = self
                .action("chips", json!({}))?
                .as_array()
                .ok_or("UI contract changed: attachment state")?
                .clone();
            let result = self.cdp.call(
                "DOM.setFileInputFiles",
                json!({"files": [file], "objectId": object}),
            );
            let _ = self
                .cdp
                .call("Runtime.releaseObject", json!({"objectId": object}));
            result?;
            let appear_deadline = Instant::now() + CHIP_APPEAR_TIMEOUT;
            let upload_deadline = Instant::now() + CHIP_UPLOAD_TIMEOUT;
            let mut seen_chip = false;
            loop {
                let chips_val = self.action("chips", json!({}))?;
                let chips = chips_val
                    .as_array()
                    .ok_or("UI contract changed: attachment state")?;
                match evaluate_chip_poll(chips, &before_chips, &name)? {
                    ChipPollState::Ready => break,
                    ChipPollState::Pending => {
                        seen_chip = true;
                        if Instant::now() >= upload_deadline {
                            return Err(format!("attachment upload timed out for {name}"));
                        }
                    }
                    ChipPollState::Absent => {
                        let limit = if seen_chip {
                            upload_deadline
                        } else {
                            appear_deadline
                        };
                        if Instant::now() >= limit {
                            return Err(if seen_chip {
                                format!("attachment upload disappeared for {name}")
                            } else {
                                format!(
                                    "UI contract changed: attachment preview unavailable for {name}"
                                )
                            });
                        }
                    }
                }
                self.probe()?;
                self.pause(100)?;
            }
            if let Some(callback) = progress.as_deref_mut() {
                callback(&AttachmentProgress { name, ready: true });
            }
        }
        self.close_overlays();
        Ok(())
    }

    pub fn current_model(&mut self) -> Result<Option<String>, String> {
        Ok(self.probe()?["model"].as_str().map(str::to_owned))
    }

    fn escape(&mut self) {
        let _ = self.cdp.call(
            "Input.dispatchKeyEvent",
            json!({"type":"keyDown","key":"Escape","code":"Escape","windowsVirtualKeyCode":27}),
        );
        let _ = self.cdp.call(
            "Input.dispatchKeyEvent",
            json!({"type":"keyUp","key":"Escape","code":"Escape","windowsVirtualKeyCode":27}),
        );
    }

    pub fn close_overlays(&mut self) {
        self.escape();
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            if self.action("overlay_open", json!({})).ok().as_ref() != Some(&json!(true)) {
                break;
            }
            if self.pause(50).is_err() {
                break;
            }
        }
    }

    fn menu(&mut self) -> Result<Vec<ModelOption>, String> {
        let ready = Instant::now() + Duration::from_secs(12);
        let mut next_open = Instant::now();
        let mut opened = false;
        loop {
            self.check()?;
            let mut just_clicked = false;
            if Instant::now() >= next_open {
                let clicked = self.human_click("models_open", json!({}))?["ok"] == true;
                opened |= clicked;
                just_clicked = clicked;
                next_open = Instant::now() + Duration::from_millis(jitter_u64(500, 750));
            }
            let value = self.action("models", json!({}))?;
            if value.as_array().is_some_and(|a| !a.is_empty()) {
                return serde_json::from_value(value).map_err(|e| e.to_string());
            }
            if just_clicked && self.pause(90).is_ok() {
                let value = self.action("models", json!({}))?;
                if value.as_array().is_some_and(|a| !a.is_empty()) {
                    return serde_json::from_value(value).map_err(|e| e.to_string());
                }
            }
            if Instant::now() >= ready {
                return Err(if opened {
                    "UI contract changed: empty model menu"
                } else {
                    "UI contract changed: model menu unavailable"
                }
                .into());
            }
            self.pause_jitter(45, 85)?;
        }
    }

    fn open_menu_items(&mut self) -> Result<Vec<ModelOption>, String> {
        let Ok(value) = self.action("models", json!({})) else {
            return Ok(Vec::new());
        };
        if value.as_array().is_some_and(|a| !a.is_empty()) {
            serde_json::from_value(value).map_err(|e| e.to_string())
        } else {
            Ok(Vec::new())
        }
    }

    fn open_thinking_items(&mut self) -> Result<Option<Vec<ModelOption>>, String> {
        let Ok(value) = self.action("models", json!({"thinking": true})) else {
            return Ok(None);
        };
        if value.as_array().is_some_and(|a| !a.is_empty()) {
            serde_json::from_value::<Vec<ModelOption>>(value)
                .map(Some)
                .map_err(|e| e.to_string())
        } else {
            Ok(None)
        }
    }

    fn thinking_menu(&mut self) -> Result<Option<Vec<ModelOption>>, String> {
        let ready = Instant::now() + Duration::from_secs(4);
        let mut next_open = Instant::now();
        let mut unavailable_checks = 0u8;
        loop {
            self.check()?;
            let mut just_clicked = false;
            if Instant::now() >= next_open {
                let submenu = self.human_click("thinking_open", json!({}))?;
                if submenu["ok"] != true {
                    return Err("UI contract changed: thinking menu did not open".into());
                }
                if submenu["available"] != true {
                    if self.open_menu_items()?.is_empty() && Instant::now() < ready {
                        let _ = self.menu()?;
                        next_open = Instant::now();
                        continue;
                    }
                    unavailable_checks += 1;
                    if unavailable_checks >= 2 || Instant::now() >= ready {
                        return Ok(None);
                    }
                    self.pause_jitter(45, 85)?;
                    next_open = Instant::now();
                    continue;
                }
                just_clicked = true;
                next_open = Instant::now() + Duration::from_millis(jitter_u64(360, 560));
            }
            let values: Vec<ModelOption> =
                serde_json::from_value(self.action("models", json!({"thinking": true}))?)
                    .map_err(|e| e.to_string())?;
            if !values.is_empty() {
                return Ok(Some(values));
            }
            if just_clicked && self.pause(90).is_ok() {
                let values: Vec<ModelOption> =
                    serde_json::from_value(self.action("models", json!({"thinking": true}))?)
                        .map_err(|e| e.to_string())?;
                if !values.is_empty() {
                    return Ok(Some(values));
                }
            }
            if Instant::now() >= ready {
                return Err("UI contract changed: empty thinking menu".into());
            }
            self.pause_jitter(45, 85)?;
        }
    }

    fn list_models_and_thinking_unlocked(
        &mut self,
    ) -> Result<(Vec<ModelOption>, Vec<ThinkingOption>), String> {
        let models = self.menu()?;
        let thinking = self
            .thinking_menu()?
            .unwrap_or_default()
            .into_iter()
            .map(|m| ThinkingOption {
                index: m.index,
                level: m.mode_id,
                label: m.label,
                sublabel: m.sublabel,
                selected: m.selected,
                locked: m.locked,
                rate_limit_reset: m.rate_limit_reset,
            })
            .collect();
        self.close_overlays();
        Ok((models, thinking))
    }

    pub fn list_models_and_thinking(
        &mut self,
    ) -> Result<(Vec<ModelOption>, Vec<ThinkingOption>), String> {
        let input_lock = self.input_lock.clone();
        let _input_guard = input_lock
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        self.list_models_and_thinking_unlocked()
    }

    fn select_model_from_menu(
        &mut self,
        menu: Vec<ModelOption>,
        query: &str,
        keep_open_if_selected: bool,
    ) -> Result<(ModelOption, bool), String> {
        let selected = menu
            .into_iter()
            .find(|m| m.mode_id == query || m.label == query || m.sublabel == query)
            .ok_or_else(|| format!("unsupported model ID: {query}"))?;
        if selected.locked {
            self.close_overlays();
            return Err("rate limit or model unavailable".into());
        }
        if selected.selected {
            if !keep_open_if_selected {
                self.close_overlays();
            }
            return Ok((selected, keep_open_if_selected));
        }
        let before_probe = if keep_open_if_selected {
            None
        } else {
            self.probe().ok()
        };
        if self.human_click("select", json!({"id": selected.mode_id}))?["ok"] != true {
            self.close_overlays();
            return Err("UI contract changed: model selection failed".into());
        }
        let confirm_until = Instant::now() + Duration::from_millis(650);
        loop {
            self.pause_jitter(35, 70)?;
            let open_items = self.open_menu_items()?;
            if !open_items.is_empty() {
                if open_items
                    .iter()
                    .any(|m| m.mode_id == selected.mode_id && m.selected)
                {
                    if !keep_open_if_selected {
                        self.close_overlays();
                    }
                    return Ok((selected, keep_open_if_selected));
                }
                if Instant::now() >= confirm_until {
                    if !keep_open_if_selected {
                        self.close_overlays();
                    }
                    return Err("UI contract changed: model selection not confirmed".into());
                }
                continue;
            }
            if !keep_open_if_selected {
                let after_probe = self.probe().ok();
                if model_probe_confirmed(before_probe.as_ref(), after_probe.as_ref(), &selected) {
                    return Ok((selected, false));
                }
                if Instant::now() >= confirm_until {
                    break;
                }
                continue;
            }
            break;
        }
        let verified_menu = self.menu()?;
        if !verified_menu
            .iter()
            .any(|m| m.mode_id == selected.mode_id && m.selected)
        {
            self.close_overlays();
            return Err("UI contract changed: model selection not confirmed".into());
        }
        if !keep_open_if_selected {
            self.close_overlays();
        }
        Ok((selected, keep_open_if_selected))
    }

    fn select_thinking_from_open_menu(&mut self, query: &str) -> Result<(), String> {
        let menu = match self.thinking_menu() {
            Ok(Some(menu)) => menu,
            Ok(None) => {
                self.close_overlays();
                return Err("unsupported capability: thinking menu".into());
            }
            Err(e) => {
                self.close_overlays();
                return Err(e);
            }
        };
        let Some(selected) = menu
            .into_iter()
            .find(|m| m.mode_id == query || m.label == query)
        else {
            self.close_overlays();
            return Err("unsupported thinking level".into());
        };
        if selected.locked {
            self.close_overlays();
            return Err("thinking selection unavailable".into());
        }
        if selected.selected {
            self.close_overlays();
            return Ok(());
        }
        if self.human_click("select", json!({"id": selected.mode_id, "thinking": true}))?["ok"]
            != true
        {
            self.close_overlays();
            return Err("thinking selection unavailable".into());
        }
        let confirm_until = Instant::now() + Duration::from_millis(400);
        loop {
            self.pause_jitter(35, 70)?;
            if let Some(items) = self.open_thinking_items()? {
                if items
                    .iter()
                    .any(|m| m.mode_id == selected.mode_id && m.selected)
                {
                    self.close_overlays();
                    return Ok(());
                }
                if Instant::now() >= confirm_until {
                    self.close_overlays();
                    return Err("UI contract changed: thinking selection not confirmed".into());
                }
                continue;
            }
            if self.action("overlay_open", json!({})).ok().as_ref() != Some(&json!(true)) {
                return Ok(());
            }
            if Instant::now() >= confirm_until {
                break;
            }
        }
        self.close_overlays();
        let (_, thinking) = self.list_models_and_thinking_unlocked()?;
        if !thinking
            .iter()
            .any(|level| level.level == selected.mode_id && level.selected)
        {
            return Err("UI contract changed: thinking selection not confirmed".into());
        }
        Ok(())
    }

    fn select_model_unlocked(&mut self, query: &str) -> Result<ModelOption, String> {
        let menu = self.menu()?;
        self.select_model_from_menu(menu, query, false)
            .map(|(option, _)| option)
    }

    fn select_thinking_unlocked(&mut self, query: &str) -> Result<(), String> {
        self.menu()?;
        self.select_thinking_from_open_menu(query)
    }

    pub fn select_model(&mut self, query: &str) -> Result<ModelOption, String> {
        let input_lock = self.input_lock.clone();
        let _input_guard = input_lock
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        self.select_model_unlocked(query)
    }

    pub fn select_thinking(&mut self, query: &str) -> Result<(), String> {
        let input_lock = self.input_lock.clone();
        let _input_guard = input_lock
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        self.select_thinking_unlocked(query)
    }

    pub fn select_model_and_thinking(
        &mut self,
        model: Option<&str>,
        thinking: Option<&str>,
    ) -> Result<(), String> {
        if model.is_none() && thinking.is_none() {
            return Ok(());
        }
        let input_lock = self.input_lock.clone();
        let _input_guard = input_lock
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        match (model, thinking) {
            (None, None) => Ok(()),
            (Some(model), None) => self.select_model_unlocked(model).map(|_| ()),
            (None, Some(thinking)) => self.select_thinking_unlocked(thinking),
            (Some(model), Some(thinking)) => {
                let menu = self.menu()?;
                let (selected_model, mut menu_open) =
                    self.select_model_from_menu(menu, model, true)?;
                if !selected_model.selected && !menu_open {
                    let reopened = self.menu()?;
                    if !reopened
                        .iter()
                        .any(|m| m.mode_id == selected_model.mode_id && m.selected)
                    {
                        self.close_overlays();
                        return Err("UI contract changed: model selection not confirmed".into());
                    }
                    menu_open = true;
                }
                if !menu_open {
                    let _ = self.menu()?;
                }
                self.select_thinking_from_open_menu(thinking)
            }
        }
    }

    pub fn model_catalog(&mut self) -> Result<ModelCatalog, String> {
        let input_lock = self.input_lock.clone();
        let _input_guard = input_lock
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        let (models, thinking) = self.list_models_and_thinking_unlocked()?;
        let original = models
            .iter()
            .find(|m| m.selected)
            .or_else(|| models.iter().find(|m| !m.locked))
            .or_else(|| models.first())
            .ok_or("UI contract changed: empty model menu")?
            .mode_id
            .clone();
        let mut model_thinking = BTreeMap::new();
        model_thinking.insert(original.clone(), thinking.clone());
        if !thinking.is_empty() {
            let result = (|| {
                for model in models.iter().filter(|m| !m.locked && m.mode_id != original) {
                    self.select_model_unlocked(&model.mode_id)?;
                    let (_, levels) = self.list_models_and_thinking_unlocked()?;
                    model_thinking.insert(model.mode_id.clone(), levels);
                }
                Ok::<_, String>(())
            })();
            let restore = self.select_model_unlocked(&original);
            result?;
            restore?;
        }
        Ok(ModelCatalog {
            models,
            thinking,
            model_thinking,
        })
    }

    pub fn ask_with_stream<F: FnMut(StreamUpdate<'_>)>(
        &mut self,
        prompt: &str,
        mut updates: Option<&mut F>,
    ) -> Result<AskResult, String> {
        self.check()?;
        let (initial, extract_js, mut events) = {
            let input_lock = self.input_lock.clone();
            let _input_guard = input_lock
                .as_ref()
                .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
            self.cdp.call("Page.bringToFront", json!({}))?;
            let initial = self.probe()?["response_count"].as_u64().unwrap_or(0);
            if self.action("prepare", json!({}))?["ok"] != true {
                return Err("UI contract changed: composer is not empty or ready".into());
            }
            let _ = self.human_click("editor", json!({}))?;
            self.human_pause(30, 75)?;
            self.cdp.call("Input.insertText", json!({"text": prompt}))?;
            let input_ready = Instant::now() + Duration::from_secs(5);
            loop {
                let checked = self.action("verify_input", json!({"text": prompt}))?;
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
            self.cdp.call(
                "Runtime.addBinding",
                json!({"name": self.config.observer_binding}),
            )?;
            let observer_js = self.script("observer.js")?;
            if observer_js.contains("geminiCdpChanged")
                && self.config.observer_binding != "geminiCdpChanged"
            {
                let _ = self
                    .cdp
                    .call("Runtime.addBinding", json!({"name": "geminiCdpChanged"}));
            }
            self.eval(&observer_js)?;
            let extract_js = self.script("extract-response.js")?.into_owned();
            let events = self.cdp.subscribe();
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
            (initial, extract_js, events)
        };
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
            let state = self.eval(&extract_js)?;
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

    fn collect_image_requests(&mut self) {
        loop {
            match self.network.try_recv() {
                Ok(event) => {
                    let p = &event["params"];
                    if p["type"] == "Image" {
                        let url = p["response"]["url"]
                            .as_str()
                            .or_else(|| p["request"]["url"].as_str());
                        if let (Some(url), Some(id)) = (url, p["requestId"].as_str()) {
                            if self.image_requests.len() >= 128 {
                                self.image_requests.clear();
                            }
                            self.image_requests.insert(url.into(), id.into());
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }

    pub fn download_generated_media(&mut self, destination: &Path) -> Result<Vec<String>, String> {
        self.cdp.call("Page.enable", json!({}))?;
        let until = Instant::now() + Duration::from_secs(12);
        let media = loop {
            self.collect_image_requests();
            let media = self.action("media", json!({}))?;
            if media.as_array().is_some_and(|v| !v.is_empty()) || Instant::now() >= until {
                break media;
            }
            self.probe()?;
            self.pause(100)?;
        };
        let media = media.as_array().ok_or("invalid media state")?;
        if media.is_empty() {
            return Err("unsupported capability: no generated media elements".into());
        }
        let mut paths = Vec::new();
        for (index, item) in media.iter().enumerate() {
            let url = item["url"].as_str().ok_or("missing media URL")?;
            let frame = self.cdp.call("Page.getFrameTree", json!({}))?;
            let resource = if let Some(request) = self.image_requests.get(url) {
                self.cdp
                    .call("Network.getResponseBody", json!({"requestId": request}))
                    .map(|v| json!({"content": v["body"], "base64Encoded": v["base64Encoded"]}))
            } else {
                Err("image response was not observed".into())
            }
            .or_else(|_| {
                self.cdp.call(
                    "Page.getResourceContent",
                    json!({"frameId": frame["frameTree"]["frame"]["id"], "url": url}),
                )
            });
            let encoded = match resource {
                Ok(value) if value["base64Encoded"] == true => value["content"]
                    .as_str()
                    .ok_or("image resource has no content")?
                    .to_owned(),
                _ => {
                    let js = format!(
                        "(async()=>{{try{{const r=await fetch({});if(!r.ok)return null;const b=await r.blob();if(b.size>33554432)return null;return await new Promise(ok=>{{const reader=new FileReader();reader.onload=()=>ok(reader.result);reader.onerror=()=>ok(null);reader.readAsDataURL(b);}});}}catch{{return null;}}}})()",
                        serde_json::to_string(url).unwrap()
                    );
                    let result = self.eval(&js)?;
                    result
                        .as_str()
                        .and_then(|s| s.split_once(','))
                        .ok_or("generated image exists but its bytes could not be read; retry download without regenerating")?
                        .1
                        .to_owned()
                }
            };
            if encoded.len() > 44 * 1024 * 1024 {
                return Err("generated media exceeds 32 MiB".into());
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 32 * 1024 * 1024 {
                return Err("generated media exceeds 32 MiB".into());
            }
            let extension = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                "png"
            } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                "jpg"
            } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
                "webp"
            } else {
                return Err("unsupported generated media type".into());
            };
            let path = destination.join(format!("image-{}.{}", index + 1, extension));
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            paths.push(path.to_string_lossy().into());
        }
        Ok(paths)
    }
}

pub fn read_web_usage(
    port: u16,
    target: TargetInfo,
    config: &WebChatConfig,
    cancel: CancelToken,
    deadline: Instant,
) -> Result<Value, String> {
    let mut cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
    cdp.set_control(cancel.clone(), deadline);
    cdp.call("Runtime.enable", json!({}))?;
    let _ = cdp.call(
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled": true}),
    );
    let script = (config.resolve_script)("usage.js")?;
    let mut page_present = false;
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(if page_present {
                "UI contract changed: the usage page did not expose supported usage counters"
            } else {
                "deadline exceeded: web account usage page was not ready; show the browser window and try /usage again"
            }
            .into());
        }
        let loc_eval = cdp.call(
            "Runtime.evaluate",
            json!({
                "expression": "location.href",
                "returnByValue": true
            }),
        )?;
        let current_url = loc_eval["result"]["value"].as_str().unwrap_or("");
        if config.is_auth_url(current_url) {
            return Err("sign in required: use /browser login, then try /usage again".into());
        }
        if config.url_matches_origin(current_url) {
            let result = cdp.call(
                "Runtime.evaluate",
                json!({
                    "expression": format!("({script})"),
                    "returnByValue": true,
                    "awaitPromise": true
                }),
            )?;
            if result.get("exceptionDetails").is_some() {
                return Err("UI contract changed: usage adapter JavaScript failed".into());
            }
            let value = &result["result"]["value"];
            page_present |= value["page_present"] == true;
            match value["state"].as_str() {
                Some("ready") => return Ok(value["usage"].clone()),
                Some("auth_required") => {
                    return Err(
                        "sign in required: use /browser login, then try /usage again".into(),
                    );
                }
                Some("verification") => {
                    return Err("verification required on the Chrome host".into());
                }
                _ => (),
            }
        }
        if cancel.sleep_or_cancelled(Duration::from_millis(jitter_u64(160, 260))) {
            return Err("cancelled".into());
        }
    }
}
