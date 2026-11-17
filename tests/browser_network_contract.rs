//! Opt-in browser networking checks against isolated targets in a real Chrome.
use anticrumb::{agent::CancelToken, browser_host as browser, cdp::CdpClient, gemini};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

struct LocalServer {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl LocalServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_hits = hits.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut bytes = [0u8; 4096];
                        if stream.read(&mut bytes).unwrap_or(0) > 0 {
                            worker_hits.fetch_add(1, Ordering::SeqCst);
                            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\nprivate");
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(err) => panic!("fixture server: {err}"),
                }
            }
        });
        Self {
            port,
            hits,
            stop,
            worker: Some(worker),
        }
    }
    fn assert_no_requests(&self) {
        assert_eq!(
            self.hits.load(Ordering::SeqCst),
            0,
            "Chrome sent a request to the private server"
        );
    }
}
impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct CloseTarget(u16, String);
impl Drop for CloseTarget {
    fn drop(&mut self) {
        let _ = browser::close_tab(self.0, &self.1);
    }
}

fn isolated_target() -> (u16, browser::TargetInfo, CloseTarget) {
    assert!(
        !anticrumb::web::allow_private_env(),
        "unset ANTICRUMB_ALLOW_PRIVATE_FETCH for these policy tests"
    );
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or_else(|_| "9222".into())
        .parse()
        .unwrap();
    let target = browser::create_tab(port, "about:blank").unwrap();
    let close = CloseTarget(port, target.id.clone());
    (port, target, close)
}

#[test]
#[ignore = "requires real Chrome CDP; creates only an isolated about:blank target"]
fn browser_fetch_rejects_private_aliases_before_any_request() {
    let server = LocalServer::start();
    let (port, target, _close) = isolated_target();
    for host in [
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f000001",
        "localhost.",
    ] {
        let result = gemini::fetch_page(
            port,
            target.clone(),
            &format!("http://{host}:{}/private", server.port),
            0,
            1000,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(10),
        );
        assert!(result.unwrap_err().contains("blocked"), "{host}");
    }
    server.assert_no_requests();
}

#[test]
#[ignore = "requires real Chrome CDP; creates only an isolated about:blank target"]
fn restricted_cdp_blocks_navigation_and_script_subresources() {
    let server = LocalServer::start();
    let (port, target, _close) = isolated_target();
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    cdp.set_control(CancelToken::new(), Instant::now() + Duration::from_secs(30));
    cdp.restrict_to_public_network().unwrap();
    let navigation = cdp
        .call(
            "Page.navigate",
            json!({"url": format!("http://127.1:{}/navigation", server.port)}),
        )
        .unwrap();
    assert!(
        navigation["errorText"]
            .as_str()
            .unwrap_or_default()
            .contains("BLOCKED"),
        "{navigation}"
    );
    server.assert_no_requests();
    cdp.call("Page.navigate", json!({"url":"about:blank"}))
        .unwrap();
    let expression = format!(
        r#"new Promise(resolve => {{
        const image = new Image();
        image.onload = () => resolve('loaded');
        image.onerror = () => resolve('blocked');
        image.src = 'http://127.0.0.1:{}/subresource';
        document.body.appendChild(image);
        setTimeout(() => resolve('timeout'), 5000);
    }})"#,
        server.port
    );
    let resource = cdp
        .call(
            "Runtime.evaluate",
            json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
        )
        .unwrap();
    assert_eq!(resource["result"]["value"], "blocked", "{resource}");
    server.assert_no_requests();
}

#[test]
#[ignore = "requires real Chrome CDP and public HTTPS connectivity to example.com"]
fn browser_fetch_allows_public_https() {
    let (port, target, _close) = isolated_target();
    let page = gemini::fetch_page(
        port,
        target,
        "https://example.com/",
        0,
        2000,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    assert!(page.title.contains("Example Domain"), "{page:?}");
    assert!(!page.text.trim().is_empty(), "{page:?}");
    assert_eq!(page.url, "https://example.com/");
}

#[test]
#[ignore = "requires real Chrome CDP; closes only the test's isolated target"]
fn restricted_target_cannot_send_private_requests_after_guard_drop() {
    let server = LocalServer::start();
    let (port, target, _close) = isolated_target();
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    cdp.restrict_to_public_network().unwrap();
    cdp.call("Runtime.evaluate", json!({"expression": format!("setTimeout(() => {{ const img = new Image(); img.src = 'http://127.0.0.1:{}/after-detach'; document.body.appendChild(img); }}, 300)", server.port)})).unwrap();
    drop(cdp);
    thread::sleep(Duration::from_secs(2));
    server.assert_no_requests();
}

#[test]
#[ignore = "requires real Chrome CDP and public HTTPS; uses isolated contexts only"]
fn account_browser_tools_use_proxy_for_public_pages_and_popups() {
    use anticrumb::{
        account,
        domain::{default_provider, Account},
    };
    use std::collections::HashSet;
    assert!(!anticrumb::web::allow_private_env());
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or_else(|_| "9222".into())
        .parse()
        .unwrap();
    let meta = browser::inspect_running_cdp(port).expect("verified running Chrome profile");
    let account = Account {
        id: "network-contract-only".into(),
        provider: default_provider(),
        profile: meta.user_data_dir.into(),
        subprofile: Some(meta.profile_dir),
        port,
        headless: meta.headless,
        state: "ready".into(),
    };
    let fetched = account::browser_fetch(
        &account,
        "https://example.com/",
        0,
        2000,
        CancelToken::new(),
    )
    .unwrap();
    assert!(fetched.title.contains("Example Domain"), "{fetched:?}");
    assert!(!fetched.text.trim().is_empty());
    let before: HashSet<_> = browser::list_targets(port)
        .unwrap()
        .into_iter()
        .map(|target| target.id)
        .collect();
    let registry = anticrumb::browser_control::BrowserControlRegistry::default();
    let request = anticrumb::browser_control::request(
        &json!({"action":"open","url":"https://example.com/","max_chars":2000,"timeout_ms":60000}),
        &account.id,
        "network-test",
    )
    .unwrap();
    let opened = registry
        .execute(&account, &request, CancelToken::new())
        .unwrap();
    assert!(opened.page.unwrap().title.contains("Example Domain"));
    registry.close_all();
    let after: HashSet<_> = browser::list_targets(port)
        .unwrap()
        .into_iter()
        .map(|target| target.id)
        .collect();
    assert_eq!(before, after, "external browsing changed provider targets");

    let (port, owned, context) = browser::create_browser_tool_tab(0).unwrap();
    let context = context.expect("default public browsing owns a disposable browser");
    let server = LocalServer::start();
    let mut inspector =
        CdpClient::connect(port, &browser::ws_path_from_url(&owned.ws_url).unwrap()).unwrap();
    let opened_popup = inspector.call("Runtime.evaluate", json!({
        "expression": format!("window.open('http://127.0.0.1:{}/popup', '_blank') !== null", server.port),
        "returnByValue": true, "userGesture": true,
    })).unwrap();
    assert_eq!(opened_popup["result"]["value"], true, "{opened_popup}");
    let popup_url = format!("http://127.0.0.1:{}/popup", server.port);
    let deadline = Instant::now() + Duration::from_secs(10);
    let popup = loop {
        if let Some(target) = browser::list_targets(port)
            .unwrap()
            .into_iter()
            .find(|target| target.url == popup_url)
        {
            break target;
        }
        assert!(Instant::now() < deadline, "popup was not created");
        thread::sleep(Duration::from_millis(25));
    };
    // The popup has no Fetch interceptor. Its inherited context proxy must
    // enforce the private-address policy independently of the parent target.
    let mut popup_cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&popup.ws_url).unwrap()).unwrap();
    loop {
        let text = popup_cdp.call("Runtime.evaluate", json!({"expression":"document.body ? document.body.innerText : ''", "returnByValue":true})).unwrap();
        if text["result"]["value"]
            .as_str()
            .is_some_and(|value| value.contains("blocked"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "proxy denial did not render: {text}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    server.assert_no_requests();
    drop(popup_cdp);
    drop(inspector);
    drop(context);
    assert!(
        !browser::is_port_alive(port),
        "disposable browser survived guard drop"
    );
}

#[test]
#[ignore = "requires an installed Chrome binary; launches a disposable browser and a loopback UDP fixture"]
fn external_browser_disables_nonproxied_webrtc_udp() {
    use std::net::UdpSocket;
    assert!(!anticrumb::web::allow_private_env());
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let (port, target, context) = browser::create_browser_tool_tab(0).unwrap();
    let context = context.expect("public browsing must own an isolated browser");
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    cdp.set_control(CancelToken::new(), Instant::now() + Duration::from_secs(15));
    // The page has no Fetch interceptor: the process policy must independently
    // stop STUN, which bypasses both HTTP interception and HTTP proxies.
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": format!(r#"(async () => {{
            const pc = new RTCPeerConnection({{iceServers: [{{urls: 'stun:127.0.0.1:{}'}}]}});
            window.probePeer = pc;
            pc.createDataChannel('probe');
            await pc.setLocalDescription(await pc.createOffer());
            await new Promise(resolve => setTimeout(resolve, 3500));
            const state = pc.iceGatheringState;
            pc.close();
            return state;
        }})()"#, udp.local_addr().unwrap().port()),
                "awaitPromise": true, "returnByValue": true,
            }),
        )
        .unwrap();
    assert!(
        result.get("exceptionDetails").is_none(),
        "WebRTC probe did not run: {result}"
    );
    assert_eq!(result["result"]["value"], "complete", "{result}");
    let mut bytes = [0u8; 2048];
    let error = udp
        .recv_from(&mut bytes)
        .expect_err("Chrome sent nonproxied STUN to a private UDP socket");
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{error}"
    );
    drop(cdp);
    drop(context);
    assert!(
        !browser::is_port_alive(port),
        "disposable browser survived guard drop"
    );
}
