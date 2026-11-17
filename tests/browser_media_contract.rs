//! Synthetic generated media; never submits a prompt or loads an account page.
use anticrumb::{
    agent::CancelToken,
    browser_host as browser,
    cdp::CdpClient,
    gemini::{ChatSessionMode, GeminiTab},
};
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires a local Chrome CDP endpoint; generated fixture only, no account website requests"]
fn later_connection_recovers_loaded_revoked_blob_without_network_capture() {
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or_else(|_| "9222".into())
        .parse()
        .unwrap();
    let target = browser::create_tab(port, "about:blank").unwrap();
    struct Close(u16, String);
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = browser::close_tab(self.0, &self.1);
        }
    }
    let _close = Close(port, target.id.clone());
    let mut setup =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let frame = setup.call("Page.getFrameTree", json!({})).unwrap();
    setup
        .call(
            "Page.setDocumentContent",
            json!({"frameId":frame["frameTree"]["frame"]["id"],"html":r#"
                <!doctype html><html><body>
                <rich-textarea><div contenteditable="true" style="min-height:20px"></div></rich-textarea>
                <button data-test-id="bard-mode-menu-button">Fixture model</button>
                <model-response><generated-image><img></generated-image></model-response>
                </body></html>"#}),
        )
        .unwrap();
    let loaded = setup
        .call(
            "Runtime.evaluate",
            json!({"awaitPromise":true,"returnByValue":true,"expression":r#"
                (async () => {
                  const canvas = document.createElement('canvas');
                  canvas.width = canvas.height = 256;
                  const context = canvas.getContext('2d');
                  context.fillStyle = '#3964cc';
                  context.fillRect(0, 0, 256, 256);
                  context.fillStyle = '#ffd966';
                  context.fillRect(64, 64, 128, 128);
                  const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
                  const url = URL.createObjectURL(blob);
                  const img = document.querySelector('generated-image img');
                  await new Promise((resolve, reject) => {
                    img.onload = resolve; img.onerror = reject; img.src = url;
                  });
                  URL.revokeObjectURL(url);
                  let fetchFailed = false;
                  try { await fetch(url); } catch { fetchFailed = true; }
                  return {fetchFailed, complete: img.complete, width: img.naturalWidth};
                })()
            "#}),
        )
        .unwrap();
    assert!(loaded.get("exceptionDetails").is_none(), "{loaded}");
    assert_eq!(loaded["result"]["value"]["fetchFailed"], true);
    assert_eq!(loaded["result"]["value"]["complete"], true);
    assert_eq!(loaded["result"]["value"]["width"], 256);
    // The later command has neither the original Network request ID nor any
    // domain enablement from the connection that created the image.
    drop(setup);
    let mut tab = GeminiTab::attach(
        port,
        target,
        ChatSessionMode::Continue,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(20),
        false,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let paths = tab.download_generated_media(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let bytes = std::fs::read(&paths[0]).unwrap();
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 256);
    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 256);
}
