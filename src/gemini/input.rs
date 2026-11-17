use super::{jitter_f64, jitter_u64, mark_mouse_move_stalled, GeminiTab};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub struct AttachmentProgress {
    pub name: String,
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChipPollState {
    Absent,
    Pending,
    Ready,
}

pub(crate) const CHIP_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const CHIP_UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn evaluate_chip_poll(
    chips: &[Value],
    before_chips: &[Value],
    expected_name: &str,
) -> Result<ChipPollState, String> {
    let exact: Vec<&Value> = chips
        .iter()
        .filter(|c| c["name"].as_str() == Some(expected_name))
        .collect();
    if exact.len() > 1 {
        return Err("ambiguous attachment identity".into());
    }
    if let Some(chip) = exact.first() {
        if chip["failed"] == true {
            return Err("attachment rejected".into());
        }
        return Ok(if chip["pending"] == false {
            ChipPollState::Ready
        } else {
            ChipPollState::Pending
        });
    }

    // Image/media previews on web chat UIs often omit the filename or expose generic
    // alt/tooltip text (e.g. "Image preview", "Remove", or ""). Track the newly added
    // chip relative to the pre-upload snapshot, consuming already-settled chips first
    // so a new pending chip with the same generic label is never masked.
    if chips.len() <= before_chips.len() {
        return Ok(ChipPollState::Absent);
    }
    let mut remaining: Vec<&Value> = chips.iter().collect();
    for old in before_chips {
        let old_name = old["name"].as_str().unwrap_or("");
        if let Some(pos) = remaining
            .iter()
            .position(|c| {
                c["name"].as_str().unwrap_or("") == old_name
                    && c["pending"] == false
                    && c["failed"] != true
            })
            .or_else(|| {
                remaining
                    .iter()
                    .position(|c| c["name"].as_str().unwrap_or("") == old_name)
            })
        {
            remaining.swap_remove(pos);
        }
    }
    if remaining.is_empty() {
        return Ok(ChipPollState::Absent);
    }
    if remaining.iter().any(|c| c["failed"] == true) {
        return Err("attachment rejected".into());
    }
    if remaining.len() > 1 {
        return Err("ambiguous attachment identity".into());
    }
    let chip = remaining[0];
    Ok(if chip["pending"] == false {
        ChipPollState::Ready
    } else {
        ChipPollState::Pending
    })
}

impl GeminiTab {
    pub(super) fn dispatch_mouse(
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

    pub(super) fn human_click(&mut self, op: &str, args: Value) -> Result<Value, String> {
        let mut rect_args = args.clone();
        rect_args["op"] = json!("target_rect");
        rect_args["target"] = json!(op);
        let rect = self.eval(&format!(
            "({})({rect_args})",
            crate::adapter::script("driver.js")?
        ))?;
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
            super::HumanPace::Fast => 0usize,
            super::HumanPace::Low => 3usize,
            super::HumanPace::High => 4usize,
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
                    // Occluded/unfocused Wayland Chrome surfaces stall ~5s per mouseMoved
                    // compositor ack while mousePressed/mouseReleased complete immediately.
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
        if files.len() > 10 {
            return Err("maximum 10 attachments".into());
        }
        for file in files {
            let file = std::fs::canonicalize(file).map_err(|e| e.to_string())?;
            let name = file
                .file_name()
                .ok_or("missing filename")?
                .to_string_lossy()
                .to_string();
            if std::fs::metadata(&file).map_err(|e| e.to_string())?.len() > 32 * 1024 * 1024 {
                return Err("attachment exceeds 32 MiB".into());
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
                let expression = format!(
                    "({})({})",
                    crate::adapter::script("driver.js")?,
                    json!({"op":"file_input","image":image})
                );
                let value = self.cdp.call(
                    "Runtime.evaluate",
                    json!({"expression":expression,"returnByValue":false}),
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
                json!({"files":[file],"objectId":object}),
            );
            let _ = self
                .cdp
                .call("Runtime.releaseObject", json!({"objectId":object}));
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
    fn human_click_dispatches_trusted_pointer_sequence_and_skips_already_open() {
        let recorded = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let recorded_server = recorded.clone();
        let mut eval_index = 0usize;
        let server = MockCdpServer::start_results(1, Duration::from_secs(2), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            if method == "Input.dispatchMouseEvent" {
                recorded_server
                    .lock()
                    .unwrap()
                    .push((method.to_owned(), request["params"].clone()));
            }
            if method == "Runtime.evaluate" {
                eval_index += 1;
                match eval_index {
                    // wait_ready probe
                    1 => json!({"result":{"type":"object","value":{
                        "editor":true,"models":true,"signed_in":true,"verification":false,
                        "url":"https://gemini.google.com/app"
                    }}}),
                    // target_rect for send button
                    2 => json!({"result":{"type":"object","value":{
                        "ok":true,"x":200.0,"y":400.0,"width":40.0,"height":32.0
                    }}}),
                    // target_rect already_open for models_open
                    _ => json!({"result":{"type":"object","value":{
                        "ok":true,"already_open":true
                    }}}),
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
        let res = tab.human_click("send", json!({})).unwrap();
        assert_eq!(res["ok"], true);
        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events.len(),
            5,
            "expected 3 moves + press + release: {events:?}"
        );
        assert_eq!(events[0].1["type"], "mouseMoved");
        assert_eq!(events[1].1["type"], "mouseMoved");
        assert_eq!(events[2].1["type"], "mouseMoved");
        assert_eq!(events[3].1["type"], "mousePressed");
        assert_eq!(events[4].1["type"], "mouseReleased");
        let click_x = events[3].1["x"].as_f64().unwrap();
        let click_y = events[3].1["y"].as_f64().unwrap();
        assert!((210.0..=230.0).contains(&click_x), "x={click_x}");
        assert!((408.0..=424.0).contains(&click_y), "y={click_y}");
        assert!(tab.cursor.is_some());

        let open_res = tab.human_click("models_open", json!({})).unwrap();
        assert_eq!(open_res["ok"], true);
        assert_eq!(
            recorded.lock().unwrap().len(),
            5,
            "already_open should not emit mouse events"
        );
        drop(tab);
        server.join();
    }

    #[test]
    fn human_click_respects_human_pace_levels() {
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded_server = recorded.clone();
        let mut eval_index = 0usize;
        let server = MockCdpServer::start_results(1, Duration::from_secs(2), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            if method == "Input.dispatchMouseEvent" {
                recorded_server
                    .lock()
                    .unwrap()
                    .push(request["params"]["type"].as_str().unwrap_or("").to_owned());
            }
            if method == "Runtime.evaluate" {
                eval_index += 1;
                if eval_index == 1 {
                    json!({"result":{"type":"object","value":{
                        "editor":true,"models":true,"signed_in":true,"verification":false,
                        "url":"https://gemini.google.com/app"
                    }}})
                } else {
                    json!({"result":{"type":"object","value":{
                        "ok":true,"x":200.0,"y":400.0,"width":40.0,"height":32.0
                    }}})
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

        tab.set_pace(crate::gemini::HumanPace::Fast);
        tab.human_click("send", json!({})).unwrap();
        assert_eq!(
            *recorded.lock().unwrap(),
            vec!["mousePressed", "mouseReleased"]
        );

        recorded.lock().unwrap().clear();
        tab.set_pace(crate::gemini::HumanPace::High);
        tab.human_click("send", json!({})).unwrap();
        assert_eq!(
            *recorded.lock().unwrap(),
            vec![
                "mouseMoved",
                "mouseMoved",
                "mouseMoved",
                "mouseMoved",
                "mousePressed",
                "mouseReleased"
            ]
        );

        drop(tab);
        server.join();
    }

    #[test]
    fn evaluate_chip_poll_handles_exact_and_unnamed_image_previews() {
        // Exact filename match
        assert_eq!(
            evaluate_chip_poll(
                &[json!({"name":"1-prompt.txt","pending":true,"failed":false})],
                &[],
                "1-prompt.txt"
            )
            .unwrap(),
            ChipPollState::Pending
        );
        assert_eq!(
            evaluate_chip_poll(
                &[json!({"name":"1-prompt.txt","pending":false,"failed":false})],
                &[],
                "1-prompt.txt"
            )
            .unwrap(),
            ChipPollState::Ready
        );
        assert_eq!(
            evaluate_chip_poll(
                &[
                    json!({"name":"1-prompt.txt","pending":false,"failed":false}),
                    json!({"name":"1-prompt.txt","pending":false,"failed":false})
                ],
                &[],
                "1-prompt.txt"
            )
            .unwrap_err(),
            "ambiguous attachment identity"
        );

        // Unnamed / generic image preview fallback (e.g. PNG screenshot with no filename in DOM)
        assert_eq!(
            evaluate_chip_poll(&[], &[], "0-shot.png").unwrap(),
            ChipPollState::Absent
        );
        assert_eq!(
            evaluate_chip_poll(
                &[json!({"name":"Image preview","pending":true,"failed":false})],
                &[],
                "0-shot.png"
            )
            .unwrap(),
            ChipPollState::Pending
        );
        assert_eq!(
            evaluate_chip_poll(
                &[json!({"name":"","pending":false,"failed":false})],
                &[],
                "0-shot.png"
            )
            .unwrap(),
            ChipPollState::Ready
        );

        // Second generic-named image chip prepended before an older settled chip must stay Pending
        let first_settled = vec![json!({"name":"Image preview","pending":false,"failed":false})];
        assert_eq!(
            evaluate_chip_poll(
                &[
                    json!({"name":"Image preview","pending":true,"failed":false}),
                    json!({"name":"Image preview","pending":false,"failed":false}),
                ],
                &first_settled,
                "1-shot.png"
            )
            .unwrap(),
            ChipPollState::Pending
        );
        assert_eq!(
            evaluate_chip_poll(
                &[
                    json!({"name":"Image preview","pending":false,"failed":false}),
                    json!({"name":"Image preview","pending":false,"failed":false}),
                ],
                &first_settled,
                "1-shot.png"
            )
            .unwrap(),
            ChipPollState::Ready
        );

        // Failed generic image chip reports attachment rejected
        assert_eq!(
            evaluate_chip_poll(
                &[json!({"name":"","pending":false,"failed":true})],
                &first_settled,
                "1-shot.png"
            ),
            Ok(ChipPollState::Absent)
        );
        assert_eq!(
            evaluate_chip_poll(
                &[
                    json!({"name":"Image preview","pending":false,"failed":false}),
                    json!({"name":"","pending":false,"failed":true}),
                ],
                &first_settled,
                "1-shot.png"
            )
            .unwrap_err(),
            "attachment rejected"
        );
    }

    #[test]
    fn attach_files_succeeds_for_unnamed_image_followed_by_named_prompt_file() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("0-shot.png");
        let prompt = dir.path().join("1-prompt.txt");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(&prompt, "prompt text").unwrap();

        let uploaded_files = Arc::new(Mutex::new(Vec::<String>::new()));
        let uploaded_files_srv = uploaded_files.clone();
        let mut chip_polls_after_first = 0usize;
        let server = MockCdpServer::start_results(1, Duration::from_secs(5), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            match method {
                "Runtime.evaluate" => {
                    let expr = request["params"]["expression"].as_str().unwrap_or("");
                    if expr.contains("\"op\":\"probe\"") {
                        json!({"result":{"type":"object","value":{
                            "editor":true,"models":true,"signed_in":true,"verification":false,
                            "url":"https://gemini.google.com/app"
                        }}})
                    } else if expr.contains("\"op\":\"target_rect\"") {
                        json!({"result":{"type":"object","value":{"ok":true,"already_open":true}}})
                    } else if expr.contains("\"op\":\"uploads_open\"") {
                        json!({"result":{"type":"object","value":{"ok":true}}})
                    } else if expr.contains("\"op\":\"file_input\"") {
                        json!({"result":{"objectId":"input-obj-1"}})
                    } else if expr.contains("\"op\":\"chips\"") {
                        let count = uploaded_files_srv.lock().unwrap().len();
                        let chips = match count {
                            0 => json!([]),
                            1 => {
                                chip_polls_after_first += 1;
                                if chip_polls_after_first == 1 {
                                    json!([{"name":"Image preview","pending":true,"failed":false}])
                                } else {
                                    json!([{"name":"Image preview","pending":false,"failed":false}])
                                }
                            }
                            _ => json!([
                                {"name":"Image preview","pending":false,"failed":false},
                                {"name":"1-prompt.txt","pending":false,"failed":false}
                            ]),
                        };
                        json!({"result":{"type":"object","value":chips}})
                    } else if expr.contains("\"op\":\"overlay_open\"") {
                        json!({"result":{"type":"object","value":false}})
                    } else {
                        json!({"result":{"type":"object","value":{}}})
                    }
                }
                "DOM.setFileInputFiles" => {
                    if let Some(first) = request["params"]["files"]
                        .as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                    {
                        uploaded_files_srv.lock().unwrap().push(first.to_owned());
                    }
                    json!({})
                }
                _ => json!({}),
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
        let mut progress = Vec::new();
        let mut on_progress = |p: &AttachmentProgress| progress.push((p.name.clone(), p.ready));
        tab.attach_files(&[shot, prompt], Some(&mut on_progress))
            .unwrap();

        assert_eq!(
            progress,
            vec![
                ("0-shot.png".to_string(), true),
                ("1-prompt.txt".to_string(), true),
            ]
        );
        assert_eq!(uploaded_files.lock().unwrap().len(), 2);

        drop(tab);
        server.join();
    }
}
