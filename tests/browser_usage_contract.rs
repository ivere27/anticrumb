//! Synthetic usage pages only. These tests never navigate to an account website.
use anticrumb::{adapter, browser_host as browser, cdp::CdpClient};
use serde_json::{json, Value};

struct Page {
    port: u16,
    target: String,
    cdp: CdpClient,
}
impl Page {
    fn new() -> Self {
        let port = std::env::var("ANTICRUMB_TEST_PORT")
            .unwrap_or_else(|_| "9222".into())
            .parse()
            .unwrap();
        let target = browser::create_tab(port, "about:blank").unwrap();
        let cdp =
            CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
        Self {
            port,
            target: target.id,
            cdp,
        }
    }
    fn read(&mut self, body: &str) -> Value {
        let tree = self.cdp.call("Page.getFrameTree", json!({})).unwrap();
        self.cdp
            .call(
                "Page.setDocumentContent",
                json!({"frameId":tree["frameTree"]["frame"]["id"],
                    "html":format!("<!doctype html><html><body>{body}</body></html>")}),
            )
            .unwrap();
        let value = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":adapter::script("usage.js").unwrap(),"returnByValue":true}),
            )
            .unwrap();
        assert!(value.get("exceptionDetails").is_none(), "{value}");
        value["result"]["value"].clone()
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        let _ = browser::close_tab(self.port, &self.target);
    }
}

fn row(label: &str, used: &str, reset: &str) -> String {
    format!(
        r#"<li data-testid="current-usage"><strong>{label}</strong>
        <strong><span>{used}</span></strong><span>{reset}</span>
        <span aria-hidden="true">decorative-reset</span>
        <button><span>Open details</span></button></li>"#
    )
}
fn island(rows: &str) -> String {
    format!(
        r#"<section data-usage-island-root><header><div><h1>Usage limits</h1>
        <span>Fixture plan</span></div></header><ul>{rows}</ul>
        <p data-testid="usage-updated">Updated just now</p></section>"#
    )
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no account website requests"]
fn usage_percentages_preserve_localized_text_and_unknown_counts() {
    let mut page = Page::new();
    for (label, used, expected, reset) in [
        ("Current usage", "0% used", Some(0.0), "Resets at 5:41 AM"),
        (
            "Current usage",
            "100% used",
            Some(100.0),
            "Resets at 5:41 AM",
        ),
        (
            "Current usage",
            "12.5% used",
            Some(12.5),
            "Resets at 5:41 AM",
        ),
        (
            "الاستخدام الحالي",
            "١٢٫٥٪ مستخدم",
            Some(12.5),
            "يعاد التعيين لاحقًا",
        ),
        (
            "الاستخدام الحالي",
            "۱۲٫۵٪ مستخدم",
            Some(12.5),
            "يعاد التعيين لاحقًا",
        ),
        ("Current usage", "12,5% used", Some(12.5), "Resets later"),
        ("Current usage", "&lt;1% used", None, "Resets later"),
        ("Current usage", "Not reported", None, "Resets later"),
    ] {
        let rows = format!(
            "{}{}",
            row(label, used, reset),
            r#"<li data-testid="weekly-usage"><strong>Weekly limit</strong>
            <strong>0% used</strong><time>Resets next week</time></li>"#
        );
        let usage = page.read(&island(&rows));
        assert_eq!(usage["state"], "ready", "{usage}");
        let report = &usage["usage"];
        assert_eq!(report["plan"], "Fixture plan");
        assert_eq!(report["limits"].as_array().unwrap().len(), 2);
        let current = &report["limits"][0];
        assert_eq!(current["label"], label);
        assert_eq!(current["used"], used.replace("&lt;", "<"));
        assert_eq!(current["used_percent"].as_f64(), expected);
        assert_eq!(current["reset"], reset);
        assert_eq!(
            current["status"],
            if expected == Some(100.0) {
                "exhausted"
            } else {
                "reported"
            }
        );
        assert!(current["remaining"].is_null());
        assert!(current["limit"].is_null());
        assert_eq!(report["limits"][1]["used_percent"], 0.0);
        assert_eq!(report["updated_label"], "Updated just now");
        assert!(report["timezone"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(!report.to_string().contains("decorative-reset"));
        assert!(!report.to_string().contains("Open details"));
    }
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no account website requests"]
fn usage_ignores_private_noise_and_stale_hidden_dom_until_counters_are_ready() {
    let mut page = Page::new();
    let private_noise = r#"<aside><span>fixture-private@example.invalid</span>
        <div data-testid="usage-updated">private conversation history</div>
        <ul><li data-testid="current-usage"><strong>Private stale quota</strong>
        <strong>99% used</strong><span>private reset</span></li></ul></aside>"#;
    for body in [
        private_noise.to_owned(),
        format!(
            "<div hidden>{}</div>",
            island(&row("Hidden quota", "99% used", "private reset"))
        ),
        island(""),
        island(r#"<li data-testid="current-usage"><strong>Current usage</strong></li>"#),
        island(&format!(
            "<div aria-busy=\"true\">{}</div>",
            row("Current usage", "1% used", "later")
        )),
        island(&format!(
            "<div role=\"progressbar\"></div>{}",
            row("Current usage", "1% used", "later")
        )),
        island(&format!(
            "<div style=\"visibility:hidden\">{}</div>",
            row("Hidden quota", "99% used", "private reset")
        )),
        island(&format!(
            "<div aria-hidden=\"true\">{}</div>",
            row("Hidden quota", "99% used", "private reset")
        )),
    ] {
        let value = page.read(&format!("{private_noise}{body}"));
        assert_eq!(value["state"], "loading", "{value}");
        assert!(value.get("usage").is_none(), "{value}");
        assert!(!value.to_string().contains("private"));
    }
    let hidden_island = format!(
        "<div hidden>{}</div>",
        island(&row("Private stale quota", "99% used", "private reset"))
    );
    let rows = format!(
        "{}<div hidden>{}<span role=\"progressbar\"></span></div><div aria-hidden=\"true\">{}</div><div style=\"visibility:hidden\">{}<span aria-busy=\"true\"></span></div><div role=\"progressbar\" aria-valuenow=\"1\"></div>",
        row("Current usage", "1% used", "Resets later"),
        row("Private stale quota", "99% used", "private reset"),
        row("Private stale quota", "99% used", "private reset"),
        row("Private stale quota", "99% used", "private reset"),
    );
    let value = page.read(&format!("{private_noise}{hidden_island}{}", island(&rows)));
    assert_eq!(value["state"], "ready", "{value}");
    assert_eq!(value["usage"]["limits"].as_array().unwrap().len(), 1);
    assert_eq!(value["usage"]["limits"][0]["label"], "Current usage");
    assert_eq!(value["usage"]["limits"][0]["used_percent"], 1.0);
    assert!(!value.to_string().contains("private"));
    assert!(!value.to_string().contains("Private"));
    for (body, expected) in [
        (
            r#"<a href="https://accounts.google.com/ServiceLogin">Sign in</a>"#,
            "auth_required",
        ),
        (
            r#"<a hidden href="https://accounts.google.com/signin">Sign in</a>"#,
            "loading",
        ),
        (
            r#"<div id="captcha">Human verification</div>"#,
            "verification",
        ),
        (
            r#"<div hidden id="captcha">Stale challenge</div>"#,
            "loading",
        ),
    ] {
        let value = page.read(body);
        assert_eq!(value["state"], expected, "{value}");
        assert!(value.get("usage").is_none());
    }
}
