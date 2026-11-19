//! Synthetic browser test: no model requests, no third-party pages or credentials.
use anticrumb::{
    agent::CancelToken,
    browser_control::{self, BrowserControlRegistry},
    browser_host as browser,
    domain::Account,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

const HTML: &str = r#"<!doctype html><html><head><title>Browser fixture</title></head><body>
<label for="search">Search</label><input id="search" value="old">
<label for="secret">Password</label><input id="secret" type="password" value="never-print-this">
<button onclick="document.querySelector('#result').textContent=document.querySelector('#search').value">Apply</button>
<button onclick="document.cookie='login=fixture; path=/';document.querySelector('#result').textContent='saved'">Save cookie</button>
<button onclick="document.querySelector('#result').textContent=document.cookie || 'no cookie'">Read cookie</button>
<button onclick="window.open('/popup','_blank')">Popup</button>
<button disabled>Disabled</button><button>Duplicate</button><button>Duplicate</button>
<button id="change" onclick="this.textContent='Changed'">Change name</button>
<label for="options">Guests</label><select id="options"><option value="one">One</option><option value="two">Two</option></select>
<div id="result">ready</div><div id="later" hidden>Later</div>
<button onclick="setTimeout(()=>document.querySelector('#later').hidden=false,200)">Later</button>
<div style="height:2000px"></div><button>Bottom</button>
<script>document.querySelector('#search').addEventListener('keydown',e=>{if(e.key==='Enter')document.querySelector('#result').textContent='enter:'+e.target.value;});</script>
<script>
if (location.pathname === '/visual') {
  document.body.innerHTML = `<input id="visual" aria-label="Visual input" value="old" style="position:absolute;left:200px;top:120px;width:200px;height:30px">
  <div id="pane" style="position:absolute;left:200px;top:200px;width:300px;height:100px;overflow:auto"><div style="height:1000px;background:linear-gradient(red,blue)">Scrollable panel</div></div>
  <div id="out" style="position:absolute;top:330px">ready</div>
  <div style="position:absolute;left:20px;top:400px;width:200px;height:150px;background:#123456"></div><div style="height:2000px"></div>`;
  document.querySelector('#visual').addEventListener('keydown',e=>{if(e.key==='Enter')document.querySelector('#out').textContent='submitted:'+e.target.value;});
  document.querySelector('#pane').addEventListener('wheel',e=>{document.querySelector('#out').textContent='wheel:'+e.deltaY;});
}
</script>
</body></html>"#;

struct Fixture {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let thread = thread::spawn(move || {
            while !signal.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut buf = [0; 4096];
                    let _ = stream.read(&mut buf);
                    if buf.starts_with(b"GET /disconnect ") {
                        continue;
                    }
                    let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",HTML.len(),HTML);
                    let _ = stream.write_all(response.as_bytes());
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        });
        Self {
            port,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
#[ignore = "requires local Chrome; launches an isolated synthetic fixture, no provider requests"]
fn browser_sessions_forms_refs_history_popups_and_cleanup() {
    let fixture = Fixture::start();
    let temp = tempfile::tempdir().unwrap();
    std::env::set_var("ANTICRUMB_HOME", temp.path());
    std::env::set_var("ANTICRUMB_ALLOW_PRIVATE_FETCH", "1");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let account = Account {
        id: "fixture".into(),
        provider: "gemini-web".into(),
        profile: temp.path().join("profile"),
        subprofile: None,
        port,
        headless: true,
        state: "ready".into(),
    };
    let mut options = browser::LaunchOptions::headless_account(&account, "about:blank");
    options.detached = false;
    let _chrome = browser::ensure_browser(&options).unwrap();
    let registry = BrowserControlRegistry::default();
    let call = |session: &str, args: Value| {
        let req = browser_control::request(&args, &account.id, session).unwrap();
        registry.execute(&account, &req, CancelToken::new())
    };
    let url = format!("http://127.0.0.1:{}/", fixture.port);
    let opened = call("one", json!({"action":"open","url":url})).unwrap();
    let first = opened.active_tab_id.clone();
    let page = opened.page.unwrap();
    assert_eq!(page.title, "Browser fixture");
    assert_eq!(
        page.elements
            .iter()
            .find(|e| e.label == "Search")
            .unwrap()
            .value,
        "old"
    );
    assert!(!serde_json::to_string(&page)
        .unwrap()
        .contains("never-print-this"));
    let old_ref = page
        .elements
        .iter()
        .find(|e| e.label == "Apply")
        .unwrap()
        .ref_id
        .clone();
    call(
        "one",
        json!({"action":"fill","target":{"role":"textbox","name":"Search"},"text":"Unicode 🌍 & text"}),
    )
    .unwrap();
    assert!(
        call("one", json!({"action":"click","target":{"ref":old_ref}}))
            .unwrap_err()
            .message
            .contains("stale")
    );
    let applied = call("one", json!({"action":"click","target":{"name":"Apply"}})).unwrap();
    assert!(applied.page.unwrap().text.contains("Unicode 🌍 & text"));
    let entered = call(
        "one",
        json!({"action":"press","key":"Enter","target":{"name":"Search"}}),
    )
    .unwrap();
    assert!(entered
        .page
        .unwrap()
        .text
        .contains("enter:Unicode 🌍 & text"));
    let cleared = call(
        "one",
        json!({"action":"fill","target":{"name":"Search"},"text":""}),
    )
    .unwrap();
    assert_eq!(
        cleared
            .page
            .unwrap()
            .elements
            .iter()
            .find(|e| e.label == "Search")
            .unwrap()
            .value,
        ""
    );
    let selected = call(
        "one",
        json!({"action":"select","target":{"name":"Guests"},"value":"two"}),
    )
    .unwrap();
    assert_eq!(
        selected
            .page
            .unwrap()
            .elements
            .iter()
            .find(|e| e.label == "Guests")
            .unwrap()
            .value,
        "two"
    );
    assert!(call(
        "one",
        json!({"action":"click","target":{"name":"Duplicate"}})
    )
    .unwrap_err()
    .message
    .contains("ambiguous"));
    assert!(call(
        "one",
        json!({"action":"click","target":{"name":"Disabled"}})
    )
    .unwrap_err()
    .message
    .contains("disabled"));
    call(
        "one",
        json!({"action":"click","target":{"role":"button","name":"Later"}}),
    )
    .unwrap();
    call("one", json!({"action":"wait","target":{"css":"#later"}})).unwrap();
    assert!(call(
        "one",
        json!({"action":"wait","target":{"css":"#later"},"state":"hidden","timeout_ms":200})
    )
    .is_err());
    call(
        "one",
        json!({"action":"click","target":{"name":"Save cookie"}}),
    )
    .unwrap();
    let second = call("one", json!({"action":"open","url":url}))
        .unwrap()
        .active_tab_id;
    let cookie = call(
        "one",
        json!({"action":"click","target":{"name":"Read cookie"}}),
    )
    .unwrap();
    assert!(cookie.page.unwrap().text.contains("login=fixture"));
    call("two", json!({"action":"open","url":url})).unwrap();
    let isolated = call(
        "two",
        json!({"action":"click","target":{"name":"Read cookie"}}),
    )
    .unwrap();
    assert!(isolated.page.unwrap().text.contains("no cookie"));
    assert!(call("two", json!({"action":"snapshot","tab":first})).is_err());
    call("one", json!({"action":"use","tab":first})).unwrap();
    let nav = call(
        "one",
        json!({"action":"navigate","url":format!("{url}next")}),
    )
    .unwrap();
    assert!(nav.page.unwrap().url.ends_with("/next"));
    assert_eq!(
        call("one", json!({"action":"back"}))
            .unwrap()
            .page
            .unwrap()
            .url,
        url
    );
    assert!(call("one", json!({"action":"forward"}))
        .unwrap()
        .page
        .unwrap()
        .url
        .ends_with("/next"));
    call("one", json!({"action":"reload"})).unwrap();
    let popup = call("one", json!({"action":"click","target":{"name":"Popup"}})).unwrap();
    assert_eq!(popup.tabs.len(), 3);
    let popup_id = popup
        .tabs
        .iter()
        .find(|t| t.tab_id != first && t.tab_id != second)
        .unwrap()
        .tab_id
        .clone();
    call("one", json!({"action":"use","tab":popup_id})).unwrap();
    call("one", json!({"action":"scroll","y":1800})).unwrap();
    let closed = call("one", json!({"action":"close"})).unwrap();
    assert_eq!(closed.tabs.len(), 2);
    assert!(!closed.active_tab_id.is_empty());
    call("one", json!({"action":"reset"})).unwrap();
    assert!(call("one", json!({"action":"tabs"}))
        .unwrap()
        .tabs
        .is_empty());
    call("one", json!({"action":"open","url":url})).unwrap();
    assert!(call(
        "one",
        json!({"action":"click","target":{"name":"Read cookie"}})
    )
    .unwrap()
    .page
    .unwrap()
    .text
    .contains("no cookie"));

    // Real compositor pixels, cropped coordinates, focused Unicode input, shortcuts,
    // wheel hit-testing inside a nested scroller, and invalidation of old observations.
    let visual_url = format!("{url}visual");
    call("visual", json!({"action":"open","url":visual_url})).unwrap();
    let target = browser::list_targets(port)
        .unwrap()
        .into_iter()
        .find(|t| t.url == visual_url)
        .unwrap();
    let mut cdp = anticrumb::cdp::CdpClient::connect(
        port,
        &browser::ws_path_from_url(&target.ws_url).unwrap(),
    )
    .unwrap();
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({"width":800,"height":600,"deviceScaleFactor":2,"mobile":false}),
    )
    .unwrap();
    let shot = call(
        "visual",
        json!({"action":"screenshot","region":{"x":180,"y":100,"width":320,"height":80}}),
    )
    .unwrap()
    .page
    .unwrap()
    .screenshot
    .unwrap();
    assert_eq!((shot.width, shot.height), (320, 80));
    assert!(shot.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let point = json!({"screenshot_id":shot.id,"x":40,"y":35});
    let clicked = call("visual", json!({"action":"click","point":point}))
        .unwrap()
        .page
        .unwrap();
    assert_ne!(clicked.screenshot.unwrap().id, shot.id);
    assert!(call("visual", json!({"action":"click","point":point}))
        .unwrap_err()
        .message
        .contains("stale"));
    call(
        "visual",
        json!({"action":"press","key":"a","modifiers":["Control"]}),
    )
    .unwrap();
    let typed = call(
        "visual",
        json!({"action":"type","text":"Unicode 🌍 visual input"}),
    )
    .unwrap()
    .page
    .unwrap();
    assert_eq!(
        typed
            .elements
            .iter()
            .find(|e| e.label == "Visual input")
            .unwrap()
            .value,
        "Unicode 🌍 visual input"
    );
    assert!(typed.screenshot.is_some());
    let entered = call("visual", json!({"action":"press","key":"Enter"}))
        .unwrap()
        .page
        .unwrap();
    assert!(entered.text.contains("submitted:Unicode 🌍 visual input"));
    let shot = entered.screenshot.unwrap();
    let wheeled=call("visual",json!({"action":"wheel","point":{"screenshot_id":shot.id,"x":250,"y":250},"delta_y":180,"wait_ms":200})).unwrap().page.unwrap();
    assert!(wheeled.text.contains("wheel:180"));
    let scrolled=cdp.call("Runtime.evaluate",json!({"expression":"({outer:scrollY,inner:document.querySelector('#pane').scrollTop})","returnByValue":true})).unwrap();
    assert_eq!(scrolled["result"]["value"]["outer"], 0);
    assert!(scrolled["result"]["value"]["inner"].as_f64().unwrap() > 0.0);
    let shot = wheeled.screenshot.unwrap();
    cdp.call(
        "Runtime.evaluate",
        json!({"expression":"document.querySelector('#visual').style.left='400px'"}),
    )
    .unwrap();
    assert!(call(
        "visual",
        json!({"action":"click","point":{"screenshot_id":shot.id,"x":220,"y":135}})
    )
    .unwrap_err()
    .message
    .contains("changed"));
    let shot = call("visual", json!({"action":"screenshot"}))
        .unwrap()
        .page
        .unwrap()
        .screenshot
        .unwrap();
    assert!(call(
        "one",
        json!({"action":"click","point":{"screenshot_id":shot.id,"x":10,"y":10}})
    )
    .unwrap_err()
    .message
    .contains("unknown screenshot"));
    assert!(call(
        "visual",
        json!({"action":"click","point":{"screenshot_id":shot.id,"x":shot.width,"y":10}})
    )
    .unwrap_err()
    .message
    .contains("outside"));
    let before_scroll = call(
        "visual",
        json!({"action":"screenshot","region":{"x":20,"y":410,"width":200,"height":100}}),
    )
    .unwrap()
    .page
    .unwrap()
    .screenshot
    .unwrap();
    {
        use base64::Engine;
        let data = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&before_scroll.png)
        );
        let expression = format!("new Promise((resolve,reject)=>{{const i=new Image();i.onload=()=>{{const c=document.createElement('canvas');c.width=i.width;c.height=i.height;const ctx=c.getContext('2d');ctx.drawImage(i,0,0);resolve([...ctx.getImageData(10,10,1,1).data]);}};i.onerror=reject;i.src={};}})",json!(data));
        let pixel = cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
            )
            .unwrap();
        assert_eq!(
            pixel["result"]["value"],
            json!([18, 52, 86, 255]),
            "crop must contain the marker's actual rendered pixels"
        );
    }
    // Scrolling in another client invalidates a screenshot even without a DOM mutation.
    cdp.call(
        "Runtime.evaluate",
        json!({"expression":"window.scrollTo(0,300)"}),
    )
    .unwrap();
    assert!(call(
        "visual",
        json!({"action":"click","point":{"screenshot_id":before_scroll.id,"x":10,"y":10}})
    )
    .unwrap_err()
    .message
    .contains("changed"));
    let shot = call(
        "visual",
        json!({"action":"screenshot","region":{"x":20,"y":110,"width":200,"height":100}}),
    )
    .unwrap()
    .page
    .unwrap()
    .screenshot
    .unwrap();
    assert_eq!((shot.width, shot.height), (200, 100));
    assert_eq!(
        shot.png, before_scroll.png,
        "the same content must be captured after a viewport-relative region moves with scrolling"
    );
    assert!(call(
        "visual",
        json!({"action":"screenshot","region":{"x":750,"y":0,"width":100,"height":100}})
    )
    .unwrap_err()
    .message
    .contains("inside"));

    // Live updates must not prevent observation. Their frames remain unsuitable
    // for coordinate actions once the page has changed.
    cdp.call(
        "Runtime.evaluate",
        json!({"expression":r#"
            const ticker = document.createElement('div');
            ticker.id = 'ticker';
            ticker.style = 'position:fixed;top:10px;left:10px';
            document.body.appendChild(ticker);
            let tick = 0;
            window.fixtureTimer = setInterval(() => ticker.textContent = String(++tick), 1);
        "#}),
    )
    .unwrap();
    let live_shot = call("visual", json!({"action":"screenshot"}))
        .unwrap()
        .page
        .unwrap()
        .screenshot
        .unwrap();
    assert!(live_shot.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    cdp.call(
        "Runtime.evaluate",
        json!({"expression":"clearInterval(window.fixtureTimer); document.querySelector('#ticker').textContent = 'stopped'"}),
    )
    .unwrap();
    assert!(call(
        "visual",
        json!({"action":"click","point":{"screenshot_id":live_shot.id,"x":10,"y":10}})
    )
    .unwrap_err()
    .message
    .contains("changed"));
    drop(cdp);

    // Failed opens keep their own selected tab so a following screenshot can
    // show the browser error, and navigation can recover in the same tab.
    let failed = call(
        "failed-open",
        json!({"action":"open","url":format!("{url}disconnect")}),
    )
    .unwrap_err();
    assert!(failed.message.contains("navigation failed"), "{failed}");
    assert!(failed.message.contains("remains selected"), "{failed}");
    let tabs = call("failed-open", json!({"action":"tabs"})).unwrap();
    assert_eq!(tabs.tabs.len(), 1);
    assert!(!tabs.active_tab_id.is_empty());
    let failed_page = call("failed-open", json!({"action":"screenshot"}))
        .unwrap()
        .page
        .unwrap();
    assert_eq!(failed_page.tab_id, tabs.active_tab_id);
    assert!(failed_page
        .screenshot
        .unwrap()
        .png
        .starts_with(b"\x89PNG\r\n\x1a\n"));
    let recovered = call("failed-open", json!({"action":"navigate","url":url}))
        .unwrap()
        .page
        .unwrap();
    assert_eq!(recovered.tab_id, tabs.active_tab_id);
    assert_eq!(recovered.title, "Browser fixture");
    call("failed-open", json!({"action":"close"})).unwrap();
    assert!(call("failed-open", json!({"action":"screenshot"}))
        .unwrap_err()
        .message
        .contains("no open tabs"));
    registry.close_all();
    // The RPC boundary persists pixels under the correct account and returns only
    // artifact metadata, so images travel through the normal attachment transport.
    {
        use anticrumb::rpc::{host::Host, pb};
        use prost::Message;
        let host = Host::browser_with_providers_dir(temp.path(), None).unwrap();
        host.service
            .store
            .put("local", "account", &account.id, &account)
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let shot = runtime.block_on(async {
            let path = "/anticrumb.v1.AccountService/BrowserControl";
            let open = browser_control::request(
                &json!({"action":"open","url":visual_url}),
                &account.id,
                "rpc-visual",
            )
            .unwrap();
            host.dispatch(path, open.encode_to_vec()).await.unwrap();
            let capture = browser_control::request(
                &json!({"action":"screenshot","region":{"x":180,"y":100,"width":320,"height":80}}),
                &account.id,
                "rpc-visual",
            )
            .unwrap();
            let wire = host.dispatch(path, capture.encode_to_vec()).await.unwrap();
            pb::BrowserControlResponse::decode(wire.as_slice())
                .unwrap()
                .page
                .unwrap()
                .screenshot
                .unwrap()
        });
        assert!(shot.png.is_empty());
        assert!(!shot.artifact_id.is_empty());
        let (artifact, png) = host
            .service
            .store
            .artifact_bytes(&account.id, &shot.artifact_id)
            .unwrap();
        assert_eq!(artifact.name, format!("{}.png", shot.id));
        assert_eq!(artifact.mime, "image/png");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(host
            .service
            .store
            .artifact_bytes("other-account", &shot.artifact_id)
            .is_err());
        host.shutdown();
    }
    let targets = browser::list_targets(port).unwrap();
    assert!(
        !targets.iter().any(|t| t.url.starts_with(&url)),
        "controlled pages must close: {targets:?}"
    );
}
