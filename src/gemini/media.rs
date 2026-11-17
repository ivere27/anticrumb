use super::{jitter_u64, GeminiTab};
use crate::{
    agent::CancelToken,
    browser_host::{self, TargetInfo},
    cdp::CdpClient,
};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Read an owned usage page without preparing a composer or submitting a turn.
pub fn read_web_usage(
    port: u16,
    target: TargetInfo,
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
    let script = crate::adapter::script("usage.js")?;
    let expression = format!(
        "location.origin === 'https://gemini.google.com' ? ({script}) : {{state: location.hostname === 'accounts.google.com' ? 'auth_required' : 'loading'}}"
    );
    let mut page_present = false;
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(if page_present {
                "UI contract changed: the usage page did not expose supported usage counters; check https://gemini.google.com/usage in the browser"
            } else {
                "deadline exceeded: web account usage page was not ready; show the browser window and try /usage again"
            }
            .into());
        }
        let result = cdp.call(
            "Runtime.evaluate",
            json!({
                "expression": expression, "returnByValue": true, "awaitPromise": true
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
                return Err("sign in required: use /browser login, then try /usage again".into());
            }
            Some("verification") => {
                return Err(
                    "verification required on the Chrome host; run /browser show or /browser for recovery instructions"
                        .into(),
                );
            }
            _ => (),
        }
        if cancel.sleep_or_cancelled(Duration::from_millis(jitter_u64(160, 260))) {
            return Err("cancelled".into());
        }
    }
}

impl GeminiTab {
    pub(super) fn collect_image_requests(&mut self) {
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
        // A fresh recovery connection has no observed response bodies. Enable
        // cached page resources before reading an already loaded/revoked blob.
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
            // Read the image already loaded by this owned page. This avoids cross-origin fetch restrictions
            // and does not change browser-wide download settings or copy authentication cookies.
            let frame = self.cdp.call("Page.getFrameTree", json!({}))?;
            let resource = if let Some(request) = self.image_requests.get(url) {
                self.cdp
                    .call("Network.getResponseBody", json!({"requestId":request}))
                    .map(|v| json!({"content":v["body"],"base64Encoded":v["base64Encoded"]}))
            } else {
                Err("image response was not observed".into())
            }
            .or_else(|_| {
                self.cdp.call(
                    "Page.getResourceContent",
                    json!({"frameId":frame["frameTree"]["frame"]["id"],"url":url}),
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
