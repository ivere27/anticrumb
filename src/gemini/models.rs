use super::{jitter_u64, GeminiTab};
use crate::domain::{ModelOption, ThinkingOption};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
pub struct ModelCatalog {
    pub models: Vec<ModelOption>,
    pub thinking: Vec<ThinkingOption>,
    pub model_thinking: std::collections::BTreeMap<String, Vec<ThinkingOption>>,
}

pub(crate) fn normalize_model_label(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = collapsed.as_str();
    for icon in ["keyboard_arrow_down", "arrow_drop_down", "expand_more"] {
        trimmed = trimmed.strip_suffix(icon).unwrap_or(trimmed).trim();
    }
    trimmed.to_lowercase()
}

pub(crate) fn probe_matches_model_aliases(probe: &Value, aliases: &[String]) -> bool {
    let normalized_aliases: Vec<String> = aliases
        .iter()
        .map(|a| normalize_model_label(a))
        .filter(|a| !a.is_empty())
        .collect();
    if normalized_aliases.is_empty() {
        return false;
    }
    for key in ["model_button", "model"] {
        let Some(val) = probe[key].as_str().map(normalize_model_label) else {
            continue;
        };
        if !val.is_empty()
            && normalized_aliases.iter().any(|alias| {
                alias == &val
                    || val.strip_prefix(alias.as_str()).is_some_and(|rest| {
                        rest.starts_with(' ')
                            && matches!(rest.trim(), "low" | "medium" | "high" | "minimal")
                    })
            })
        {
            return true;
        }
    }
    false
}

pub(crate) fn probe_matches_thinking(probe: &Value, thinking: &str) -> bool {
    let target = thinking.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }
    for key in ["model_button", "model"] {
        let Some(val) = probe[key].as_str().map(normalize_model_label) else {
            continue;
        };
        if let Some((_, suffix)) = val.rsplit_once(' ') {
            if suffix == target {
                return true;
            }
        }
    }
    false
}

pub(crate) fn model_probe_confirmed(
    before: Option<&Value>,
    after: Option<&Value>,
    option: &ModelOption,
) -> bool {
    let Some(after) = after else {
        return false;
    };
    let label = option.label.trim().to_ascii_lowercase();
    let sublabel = option.sublabel.trim().to_ascii_lowercase();
    for key in ["model_button", "model"] {
        let Some(after_val) = after[key].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let before_val = before
            .and_then(|b| b[key].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if before_val == Some(after_val) {
            continue;
        }
        let after_lower = after_val.to_ascii_lowercase();
        let matches_label = !label.is_empty()
            && (after_lower == label
                || (label.len() >= 2 && after_lower.contains(&label))
                || (after_lower.len() >= 2 && label.contains(&after_lower)));
        let matches_sublabel = !sublabel.is_empty()
            && (after_lower == sublabel
                || (sublabel.len() >= 2 && after_lower.contains(&sublabel))
                || (after_lower.len() >= 2 && sublabel.contains(&after_lower)));
        if matches_label || matches_sublabel {
            return true;
        }
    }
    false
}

impl GeminiTab {
    pub fn current_model(&mut self) -> Result<Option<String>, String> {
        Ok(self.probe()?["model"].as_str().map(str::to_owned))
    }

    pub fn is_model_active(&mut self, aliases: &[String]) -> Result<bool, String> {
        let probe = self.probe()?;
        Ok(probe_matches_model_aliases(&probe, aliases))
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
                // Escape or slow menu mounts can leave a transitioning overlay in the DOM briefly.
                // Wait long enough for the menu to render before retrying so we don't toggle it closed.
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

    pub(super) fn close_overlays(&mut self) {
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
                    // If clicking a model radio option just closed the parent gem-mode-menu
                    // overlay (or its fade-out animation finished), reopen the model menu
                    // and retry opening the thinking submenu instead of falsely reporting
                    // `unsupported capability: thinking menu`.
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
                serde_json::from_value(self.action("models", json!({"thinking":true}))?)
                    .map_err(|e| e.to_string())?;
            if !values.is_empty() {
                return Ok(Some(values));
            }
            if just_clicked && self.pause(90).is_ok() {
                let values: Vec<ModelOption> =
                    serde_json::from_value(self.action("models", json!({"thinking":true}))?)
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
        let Ok(value) = self.action("models", json!({"thinking":true})) else {
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
        if self.human_click("select", json!({"id":selected.mode_id}))?["ok"] != true {
            self.close_overlays();
            return Err("UI contract changed: model selection failed".into());
        }
        let confirm_until = Instant::now() + Duration::from_millis(650);
        loop {
            self.pause_jitter(35, 70)?;
            let open_items = self.open_menu_items()?;
            if !open_items.is_empty() {
                let verified = open_items
                    .iter()
                    .any(|m| m.mode_id == selected.mode_id && m.selected);
                if verified {
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
        let verified = verified_menu
            .iter()
            .any(|m| m.mode_id == selected.mode_id && m.selected);
        if !verified {
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
        if self.human_click("select", json!({"id":selected.mode_id,"thinking":true}))?["ok"] != true
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
            // When Gemini Web closes the dropdown overlay upon selecting a thinking level,
            // closing after an explicit click on an enabled item confirms selection without
            // reopening the entire model menu a third time.
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
                    // Selecting a different model usually closes the dropdown; reopening once
                    // here both confirms the active model and exposes its thinking submenu.
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
        let mut selected = models.iter().filter(|m| m.selected);
        let original = selected
            .next()
            .ok_or("UI contract changed: selected model is unknown")?
            .mode_id
            .clone();
        if selected.next().is_some() {
            return Err("UI contract changed: selected model is ambiguous".into());
        }
        let mut model_thinking = std::collections::BTreeMap::new();
        model_thinking.insert(original.clone(), thinking.clone());
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
        Ok(ModelCatalog {
            models,
            thinking,
            model_thinking,
        })
    }

    pub fn select_thinking(&mut self, query: &str) -> Result<(), String> {
        let input_lock = self.input_lock.clone();
        let _input_guard = input_lock
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        self.select_thinking_unlocked(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::CancelToken,
        gemini::{mock_cdp::MockCdpServer, ChatSessionMode},
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn select_model_and_thinking_avoid_duplicate_menu_opens_and_redundant_clicks() {
        let counters = Arc::new(Mutex::new((0usize, 0usize, 0usize)));
        let server_counters = counters.clone();
        let mut active_model = "fast".to_string();
        let mut active_thinking = "low".to_string();
        let mut menu_open = false;
        let mut thinking_open = false;
        let server = MockCdpServer::start_results(1, Duration::from_secs(2), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            if method == "Runtime.evaluate" {
                let expr = request["params"]["expression"].as_str().unwrap_or("");
                if expr.contains(r#""op":"probe""#) {
                    json!({"result":{"type":"object","value":{
                        "editor":true,"models":true,"signed_in":true,"verification":false,
                        "url":"https://gemini.google.com/app",
                        "model":"Gemini",
                        "model_button": if active_model == "pro" { "Pro" } else { "Fast" }
                    }}})
                } else if expr.contains(r#""op":"target_rect""#)
                    && expr.contains(r#""target":"models_open""#)
                {
                    if menu_open {
                        json!({"result":{"type":"object","value":{"ok":true,"already_open":true}}})
                    } else {
                        menu_open = true;
                        server_counters.lock().unwrap().0 += 1;
                        json!({"result":{"type":"object","value":{"ok":true,"x":10.0,"y":10.0,"width":40.0,"height":20.0}}})
                    }
                } else if expr.contains(r#""op":"target_rect""#)
                    && expr.contains(r#""target":"thinking_open""#)
                {
                    if thinking_open {
                        json!({"result":{"type":"object","value":{"ok":true,"available":true,"already_open":true}}})
                    } else {
                        thinking_open = true;
                        server_counters.lock().unwrap().1 += 1;
                        json!({"result":{"type":"object","value":{"ok":true,"available":true,"x":20.0,"y":20.0,"width":40.0,"height":20.0}}})
                    }
                } else if expr.contains(r#""op":"target_rect""#)
                    && expr.contains(r#""target":"select""#)
                {
                    server_counters.lock().unwrap().2 += 1;
                    if expr.contains(r#""thinking":true"#) {
                        active_thinking = "high".into();
                    } else {
                        active_model = "pro".into();
                        menu_open = false;
                        thinking_open = false;
                    }
                    json!({"result":{"type":"object","value":{"ok":true,"x":30.0,"y":30.0,"width":40.0,"height":20.0}}})
                } else if expr.contains(r#""op":"models""#) && expr.contains(r#""thinking":true"#) {
                    let list = if menu_open && thinking_open {
                        json!([
                            {"index":1,"mode_id":"low","label":"Low","sublabel":"","selected":active_thinking=="low","locked":false,"rate_limit_reset":null},
                            {"index":2,"mode_id":"high","label":"High","sublabel":"","selected":active_thinking=="high","locked":false,"rate_limit_reset":null}
                        ])
                    } else {
                        json!([])
                    };
                    json!({"result":{"type":"object","value":list}})
                } else if expr.contains(r#""op":"models""#) {
                    let list = if menu_open {
                        json!([
                            {"index":1,"mode_id":"fast","label":"Fast","sublabel":"2.5 Flash","selected":active_model=="fast","locked":false,"rate_limit_reset":null},
                            {"index":2,"mode_id":"pro","label":"Pro","sublabel":"2.5 Pro","selected":active_model=="pro","locked":false,"rate_limit_reset":null}
                        ])
                    } else {
                        json!([])
                    };
                    json!({"result":{"type":"object","value":list}})
                } else if expr.contains(r#""op":"overlay_open""#) {
                    json!({"result":{"type":"boolean","value":menu_open}})
                } else {
                    json!({})
                }
            } else if method == "Input.dispatchKeyEvent" && request["params"]["key"] == "Escape" {
                menu_open = false;
                thinking_open = false;
                json!({})
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

        // 1. Model already active ("fast"): 1 menu open, 0 select clicks.
        tab.select_model("fast").unwrap();
        assert_eq!(*counters.lock().unwrap(), (1, 0, 0));

        // 2. Switching to "pro": menu opens once, click closes it, probe model_button ("Pro") confirms with NO 2nd menu open.
        tab.select_model("pro").unwrap();
        assert_eq!(*counters.lock().unwrap(), (2, 0, 1));

        // 3. Combined model + thinking when both are already active ("pro" + "low"): single menu open total!
        tab.select_model_and_thinking(Some("pro"), Some("low"))
            .unwrap();
        assert_eq!(*counters.lock().unwrap(), (3, 1, 1));

        // 4. Switching thinking ("high") stays inside open inline thinking menu to verify without reopening menu.
        tab.select_thinking("high").unwrap();
        assert_eq!(*counters.lock().unwrap(), (4, 2, 2));

        drop(tab);
        server.join();
    }

    #[test]
    fn select_model_and_thinking_do_not_reopen_when_thinking_click_closes_menu() {
        let counters = Arc::new(Mutex::new((0usize, 0usize, 0usize)));
        let server_counters = counters.clone();
        let mut active_model = "fast".to_string();
        let mut active_thinking = "low".to_string();
        let mut menu_open = false;
        let mut thinking_open = false;
        let mut probes_since_pro_click = 0usize;
        let server = MockCdpServer::start_results(1, Duration::from_secs(2), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            if method == "Runtime.evaluate" {
                let expr = request["params"]["expression"].as_str().unwrap_or("");
                if expr.contains(r#""op":"probe""#) {
                    // Simulate Angular taking >1 poll after click to update the button label.
                    if active_model == "fast" && probes_since_pro_click == 1 {
                        probes_since_pro_click = 2;
                    }
                    let button = if active_model == "pro" {
                        "Pro"
                    } else if probes_since_pro_click >= 2 {
                        active_model = "fast".into();
                        "Fast"
                    } else {
                        "Pro"
                    };
                    json!({"result":{"type":"object","value":{
                        "editor":true,"models":true,"signed_in":true,"verification":false,
                        "url":"https://gemini.google.com/app",
                        "model":"Gemini",
                        "model_button": button
                    }}})
                } else if expr.contains(r#""op":"target_rect""#)
                    && expr.contains(r#""target":"models_open""#)
                {
                    if menu_open {
                        json!({"result":{"type":"object","value":{"ok":true,"already_open":true}}})
                    } else {
                        menu_open = true;
                        server_counters.lock().unwrap().0 += 1;
                        json!({"result":{"type":"object","value":{"ok":true,"x":10.0,"y":10.0,"width":40.0,"height":20.0}}})
                    }
                } else if expr.contains(r#""op":"target_rect""#)
                    && expr.contains(r#""target":"thinking_open""#)
                {
                    if thinking_open {
                        json!({"result":{"type":"object","value":{"ok":true,"available":true,"already_open":true}}})
                    } else {
                        thinking_open = true;
                        server_counters.lock().unwrap().1 += 1;
                        json!({"result":{"type":"object","value":{"ok":true,"available":true,"x":20.0,"y":20.0,"width":40.0,"height":20.0}}})
                    }
                } else if expr.contains(r#""op":"target_rect""#)
                    && expr.contains(r#""target":"select""#)
                {
                    server_counters.lock().unwrap().2 += 1;
                    if expr.contains(r#""thinking":true"#) {
                        active_thinking = "high".into();
                        // Real Gemini Web closes the dropdown overlay on thinking selection.
                        menu_open = false;
                        thinking_open = false;
                    } else if expr.contains(r#""id":"fast""#) {
                        active_model = "fast".into();
                        menu_open = false;
                        thinking_open = false;
                        probes_since_pro_click = 1;
                    } else {
                        active_model = "pro".into();
                        menu_open = false;
                        thinking_open = false;
                    }
                    json!({"result":{"type":"object","value":{"ok":true,"x":30.0,"y":30.0,"width":40.0,"height":20.0}}})
                } else if expr.contains(r#""op":"models""#) && expr.contains(r#""thinking":true"#) {
                    let list = if menu_open && thinking_open {
                        json!([
                            {"index":1,"mode_id":"low","label":"Low","sublabel":"","selected":active_thinking=="low","locked":false,"rate_limit_reset":null},
                            {"index":2,"mode_id":"high","label":"High","sublabel":"","selected":active_thinking=="high","locked":false,"rate_limit_reset":null}
                        ])
                    } else {
                        json!([])
                    };
                    json!({"result":{"type":"object","value":list}})
                } else if expr.contains(r#""op":"models""#) {
                    let list = if menu_open {
                        json!([
                            {"index":1,"mode_id":"fast","label":"Fast","sublabel":"2.5 Flash","selected":active_model=="fast","locked":false,"rate_limit_reset":null},
                            {"index":2,"mode_id":"pro","label":"Pro","sublabel":"2.5 Pro","selected":active_model=="pro","locked":false,"rate_limit_reset":null}
                        ])
                    } else {
                        json!([])
                    };
                    json!({"result":{"type":"object","value":list}})
                } else if expr.contains(r#""op":"overlay_open""#) {
                    json!({"result":{"type":"boolean","value":menu_open}})
                } else {
                    json!({})
                }
            } else if method == "Input.dispatchKeyEvent" && request["params"]["key"] == "Escape" {
                menu_open = false;
                thinking_open = false;
                json!({})
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

        // Model + thinking switch where BOTH clicks close the dropdown:
        // 1 open for model click + 1 open to pick thinking (NO 3rd reopen after thinking closes overlay).
        tab.select_model_and_thinking(Some("pro"), Some("high"))
            .unwrap();
        assert_eq!(*counters.lock().unwrap(), (2, 1, 2));

        // 1. Switching back to "fast" without thinking: 1 open, confirmed by probe polling (NO 2nd reopen).
        tab.select_model("fast").unwrap();
        assert_eq!(*counters.lock().unwrap(), (3, 1, 3));

        // 2. When the active pill already matches ("Fast"), is_model_active succeeds with 0 menu opens.
        let fast_aliases = vec!["fast".to_string(), "2.5 Flash".to_string()];
        assert!(tab.is_model_active(&fast_aliases).unwrap());
        let pro_aliases = vec!["pro".to_string(), "2.5 Pro".to_string()];
        assert!(!tab.is_model_active(&pro_aliases).unwrap());
        assert_eq!(*counters.lock().unwrap(), (3, 1, 3));

        // 3. Probe normalization handles icon ligatures, thinking suffix, and whitespace cleanly.
        assert!(probe_matches_model_aliases(
            &json!({"model": null, "model_button": "Fast mode  keyboard_arrow_down"}),
            &["56fdd".to_string(), "Fast mode".to_string()]
        ));
        assert!(probe_matches_model_aliases(
            &json!({"model": "Argon High", "model_button": "Argon\nHigh"}),
            &["argon".to_string(), "4 Argon".to_string()]
        ));
        assert!(!probe_matches_model_aliases(
            &json!({"model": "Flash-Lite", "model_button": "Flash-Lite"}),
            &["Flash".to_string()]
        ));

        drop(tab);
        server.join();
    }
}
