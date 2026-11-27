use crate::{
    agent::CancelToken,
    browser_host::{self, TargetInfo},
    cdp::CdpClient,
    store::Store,
};
use serde_json::json;
use std::time::{Duration, Instant};

pub(crate) fn is_nav_context_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("execution context was destroyed")
        || lower.contains("cannot find context with specified id")
}

pub(crate) fn mark_pending_nav(cdp: &mut CdpClient) -> bool {
    cdp.call(
        "Runtime.evaluate",
        json!({
            "expression": "(() => { if (location.href === 'about:blank') return false; window.__anticrumbPendingNav = true; return true; })()",
            "returnByValue": true
        }),
    )
    .is_ok_and(|v| v["result"]["value"] == true)
}

pub(crate) fn poll_pending_nav(cdp: &mut CdpClient) -> Result<bool, String> {
    match cdp.call(
        "Runtime.evaluate",
        json!({"expression": "window.__anticrumbPendingNav === true", "returnByValue": true}),
    ) {
        Ok(v) if v["result"]["value"] == true => Ok(true),
        Ok(_) => Ok(false),
        Err(e) if is_nav_context_error(&e) => Ok(true),
        Err(e) => Err(e),
    }
}

pub(crate) fn navigate_tab_commit(
    port: u16,
    target: &TargetInfo,
    url: &str,
    cancel: &CancelToken,
    deadline: Instant,
) -> Result<(), String> {
    let mut cdp = CdpClient::connect(port, &browser_host::ws_path_from_url(&target.ws_url)?)?;
    cdp.set_control(cancel.clone(), deadline);
    let _ = cdp.call("Runtime.enable", json!({}));
    let pending = mark_pending_nav(&mut cdp);
    let nav = cdp.call("Page.navigate", json!({"url": url}))?;
    if let Some(err) = nav["errorText"].as_str().filter(|s| !s.is_empty()) {
        return Err(format!("browser navigation failed: {err}"));
    }
    if !pending {
        return Ok(());
    }
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err("deadline exceeded waiting for tab navigation".into());
        }
        if !poll_pending_nav(&mut cdp)? {
            return Ok(());
        }
        if cancel.sleep_or_cancelled(Duration::from_millis(40)) {
            return Err("cancelled".into());
        }
    }
}

/// Shared system/maintenance tab guard serialized by the account gate.
/// Reuses an unowned Gemini tab when present, and keeps at least one Gemini tab
/// warm across maintenance commands (`/resume`, `/browser clear-history`, `/usage`)
/// instead of repeatedly creating and closing tabs.
pub(crate) struct SystemGeminiTab {
    pub port: u16,
    pub target: TargetInfo,
    pub created_new: bool,
    pub keep_persistent: bool,
    _lease: Option<crate::tab_pool::SystemTabLease>,
}

impl SystemGeminiTab {
    #[cfg(test)]
    pub fn acquire(
        port: u16,
        reusable: Option<TargetInfo>,
        initial_url: &str,
    ) -> Result<Self, String> {
        Self::acquire_inner(port, reusable, None, initial_url, false)
    }

    pub fn acquire_persistent(
        port: u16,
        reusable: Option<TargetInfo>,
        initial_url: &str,
    ) -> Result<Self, String> {
        Self::acquire_inner(port, reusable, None, initial_url, true)
    }

    pub fn acquire_for_route(
        store: Option<&Store>,
        account_id: &str,
        port: u16,
        route: &crate::provider::AccountRoute,
        initial_url: &str,
    ) -> Result<Self, String> {
        let reserved = store.and_then(|s| {
            crate::tab_pool::reserve_system_gemini_target_for_route(s, account_id, port, route)
        });
        let (reusable, lease) = match reserved {
            Some((target, lease)) => (Some(target), Some(lease)),
            None => (None, None),
        };
        let mut tab = if store.is_some() {
            Self::acquire_inner(port, reusable, lease, initial_url, true)?
        } else {
            Self::acquire_inner(port, reusable, lease, initial_url, false)?
        };
        if let Some(s) = store {
            tab.keep_persistent =
                crate::service::record_system_tab_id(s, account_id, port, &tab.target.id);
        }
        Ok(tab)
    }

    fn acquire_inner(
        port: u16,
        reusable: Option<TargetInfo>,
        lease: Option<crate::tab_pool::SystemTabLease>,
        initial_url: &str,
        keep_persistent: bool,
    ) -> Result<Self, String> {
        if let Some(target) = reusable {
            let lease =
                lease.or_else(|| Some(crate::tab_pool::SystemTabLease::acquire(target.id.clone())));
            Ok(Self {
                port,
                target,
                created_new: false,
                keep_persistent,
                _lease: lease,
            })
        } else {
            let target = browser_host::create_tab(port, initial_url)?;
            let lease = Some(crate::tab_pool::SystemTabLease::acquire(target.id.clone()));
            Ok(Self {
                port,
                target,
                created_new: true,
                keep_persistent,
                _lease: lease,
            })
        }
    }
}

impl Drop for SystemGeminiTab {
    fn drop(&mut self) {
        if self.keep_persistent || !self.created_new {
            return;
        }
        let _ = browser_host::close_tab(self.port, &self.target.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::mock_cdp::MockCdpServer;

    #[test]
    fn navigate_tab_commit_waits_for_context_reset_and_reused_system_tab_does_not_close() {
        let mut eval_index = 0usize;
        let server = MockCdpServer::start(1, Duration::from_secs(2), move |request, _| {
            let method = request["method"].as_str().unwrap_or("");
            if method == "Runtime.evaluate" {
                eval_index += 1;
                match eval_index {
                    // initial marker expression
                    1 => {
                        json!({"id": request["id"], "result": {"result": {"type": "boolean", "value": true}}})
                    }
                    // old document briefly still present before navigation commits
                    2 => {
                        json!({"id": request["id"], "result": {"result": {"type": "boolean", "value": true}}})
                    }
                    // mid-navigation context teardown error
                    3 => {
                        json!({"id": request["id"], "error": {"code": -32000, "message": "Execution context was destroyed."}})
                    }
                    // new document committed: marker cleared
                    _ => {
                        json!({"id": request["id"], "result": {"result": {"type": "boolean", "value": false}}})
                    }
                }
            } else {
                json!({"id": request["id"], "result": {}})
            }
        });

        let reusable = server.target("reused-system-tab", browser_host::GEMINI_APP_URL);
        let tab = SystemGeminiTab::acquire(
            server.port,
            Some(reusable),
            "https://gemini.google.com/usage",
        )
        .unwrap();
        assert!(!tab.created_new);
        assert_eq!(tab.target.id, "reused-system-tab");

        navigate_tab_commit(
            server.port,
            &tab.target,
            "https://gemini.google.com/usage",
            &CancelToken::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(is_nav_context_error("Execution context was destroyed."));
        assert!(is_nav_context_error(
            "Cannot find context with specified id"
        ));
        assert!(!is_nav_context_error("websocket disconnected"));
        drop(tab);
        server.join();
    }
}
