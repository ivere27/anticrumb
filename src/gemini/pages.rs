use super::jitter_u64;
use crate::{
    agent::CancelToken,
    browser_host::{self, TargetInfo},
    cdp::CdpClient,
};
use serde_json::json;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserFetchOutput {
    pub url: String,
    pub title: String,
    pub text: String,
}

fn wait_for_ready_complete(
    cdp: &mut CdpClient,
    cancel: &CancelToken,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err("deadline exceeded waiting for page to load in Chrome".into());
        }
        let probe = match cdp.call(
            "Runtime.evaluate",
            json!({
                "expression": "({ready: String(document.readyState || ''), href: String(location.href || '')})",
                "returnByValue": true
            }),
        ) {
            Ok(val) => val,
            Err(err)
                if err.contains("Execution context was destroyed")
                    || err.contains("Cannot find context") =>
            {
                if cancel.sleep_or_cancelled(Duration::from_millis(jitter_u64(130, 210))) {
                    return Err("cancelled".into());
                }
                continue;
            }
            Err(err) => return Err(err),
        };
        let value = &probe["result"]["value"];
        let ready = value["ready"].as_str().unwrap_or("");
        let href = value["href"].as_str().unwrap_or("");
        if ready == "complete" && href != "about:blank" && !href.is_empty() {
            break;
        }
        if cancel.sleep_or_cancelled(Duration::from_millis(jitter_u64(130, 210))) {
            return Err("cancelled".into());
        }
    }
    Ok(())
}

fn sleep_bounded(wait_ms: u32, cancel: &CancelToken, deadline: Instant) -> Result<(), String> {
    let extra_wait = u64::from(wait_ms.min(10_000));
    if extra_wait > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("deadline exceeded before browser render wait".into());
        }
        let sleep_for = Duration::from_millis(extra_wait).min(remaining);
        if cancel.sleep_or_cancelled(sleep_for) {
            return Err("cancelled".into());
        }
    }
    Ok(())
}

/// Navigate an isolated temporary Chrome tab to `url`, wait for load + optional render delay,
/// and return the final URL, title, and bounded extracted text without reusing chat tabs.
pub fn fetch_page(
    port: u16,
    target: TargetInfo,
    url: &str,
    wait_ms: u32,
    max_chars: usize,
    cancel: CancelToken,
    deadline: Instant,
) -> Result<BrowserFetchOutput, String> {
    fetch_page_with_policy(
        port,
        target,
        url,
        wait_ms,
        max_chars,
        cancel,
        deadline,
        crate::web::allow_private_env(),
    )
}

#[allow(clippy::too_many_arguments)]
fn fetch_page_with_policy(
    port: u16,
    target: TargetInfo,
    url: &str,
    wait_ms: u32,
    max_chars: usize,
    cancel: CancelToken,
    deadline: Instant,
    allow_private: bool,
) -> Result<BrowserFetchOutput, String> {
    let validated = crate::web::validate_public_url(url, allow_private)?;
    let mut cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
    cdp.set_control(cancel.clone(), deadline);
    cdp.call("Runtime.enable", json!({}))?;
    let _ = cdp.call(
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled": true}),
    );
    cdp.call("Page.enable", json!({}))?;
    if !allow_private {
        cdp.restrict_to_public_network()?;
    }
    let nav = cdp.call("Page.navigate", json!({"url": validated.as_str()}))?;
    if let Some(err) = nav["errorText"].as_str().filter(|s| !s.is_empty()) {
        return Err(format!("browser navigation failed: {err}"));
    }

    wait_for_ready_complete(&mut cdp, &cancel, deadline)?;
    sleep_bounded(wait_ms, &cancel, deadline)?;

    let extracted = cdp.call(
        "Runtime.evaluate",
        json!({
            "expression": "(() => ({ url: String(location.href || ''), title: String(document.title || ''), text: String(document.body ? (document.body.innerText || document.body.textContent || '') : '') }))()",
            "returnByValue": true,
            "awaitPromise": false
        }),
    )?;
    if extracted.get("exceptionDetails").is_some() {
        return Err("browser page extraction failed".into());
    }
    let val = &extracted["result"]["value"];
    let final_url = val["url"]
        .as_str()
        .unwrap_or(validated.as_str())
        .trim()
        .to_string();
    if final_url.starts_with("chrome-error://") {
        return Err("browser navigation failed to load target page".into());
    }
    crate::web::validate_public_url(&final_url, allow_private)
        .map_err(|e| format!("blocked browser redirect target: {e}"))?;
    let title = val["title"].as_str().unwrap_or("").trim().to_string();
    let raw_text = val["text"].as_str().unwrap_or("");
    if final_url.contains("google.com/sorry")
        || crate::web::contains_bot_challenge(&title)
        || crate::web::contains_bot_challenge(raw_text)
    {
        return Err(
            "verification required: browser_fetch hit an anti-bot/CAPTCHA challenge; complete it manually in Chrome before retrying"
                .into(),
        );
    }
    let limit = crate::web::clamp_max_chars(max_chars);
    let text = crate::web::truncate_chars(raw_text.trim(), limit);
    Ok(BrowserFetchOutput {
        url: final_url,
        title,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::mock_cdp::MockCdpServer;

    #[test]
    fn fetch_page_extracts_rendered_dom_blocks_ssrf_and_surfaces_challenges() {
        let ssrf = fetch_page_with_policy(
            9222,
            TargetInfo {
                id: "t".into(),
                target_type: "page".into(),
                title: String::new(),
                url: "about:blank".into(),
                ws_url: "ws://127.0.0.1:9222/devtools/page/t".into(),
            },
            "http://127.0.0.1:9222/json/list",
            0,
            1000,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(2),
            false,
        )
        .unwrap_err();
        assert!(ssrf.contains("non-public IP"), "{ssrf}");

        let mut last_conn = usize::MAX;
        let mut eval_count = 0usize;
        let server =
            MockCdpServer::start_results(2, Duration::from_secs(2), move |request, conn| {
                if conn != last_conn {
                    last_conn = conn;
                    eval_count = 0;
                }
                if request["method"] == "Runtime.evaluate" {
                    eval_count += 1;
                    if eval_count == 1 {
                        json!({"result":{"type":"object","value":{
                            "ready": "complete",
                            "href": "https://example.com/app"
                        }}})
                    } else if conn == 0 {
                        json!({"result":{"type":"object","value":{
                            "url": "https://example.com/app",
                            "title": "App Title",
                            "text": "Rendered SPA text"
                        }}})
                    } else {
                        json!({"result":{"type":"object","value":{
                            "url": "https://example.com/challenge",
                            "title": "Human check",
                            "text": "Please verify you are human before continuing."
                        }}})
                    }
                } else {
                    json!({})
                }
            });

        let target = server.target("fixture", "about:blank");
        let page = fetch_page_with_policy(
            server.port,
            target.clone(),
            "https://example.com/app",
            0,
            1000,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(5),
            true,
        )
        .unwrap();
        assert_eq!(page.url, "https://example.com/app");
        assert_eq!(page.title, "App Title");
        assert_eq!(page.text, "Rendered SPA text");

        let challenge_err = fetch_page_with_policy(
            server.port,
            target,
            "https://example.com/challenge",
            0,
            1000,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(5),
            true,
        )
        .unwrap_err();
        assert!(
            challenge_err.contains("anti-bot/CAPTCHA"),
            "{challenge_err}"
        );

        server.join();
    }
}
