//! Synthetic read-only history-adapter fixtures; no account requests or deletions.
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
    fn eval(&mut self, expression: &str) -> Value {
        let result = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
            )
            .unwrap();
        assert!(result.get("exceptionDetails").is_none(), "{result}");
        result["result"]["value"].clone()
    }
    fn fixture(&mut self) {
        let tree = self.cdp.call("Page.getFrameTree", json!({})).unwrap();
        self.cdp.call("Page.setDocumentContent", json!({"frameId":tree["frameTree"]["frame"]["id"],"html":r#"<!doctype html><html><body>
            <button data-test-id="delete-button" onclick="window.deleted=true">Delete</button>
            <conversations-list data-test-id="pinned-conversations"><div data-test-id="conversation"><a href="/app/pinned">Pinned fixture</a></div></conversations-list>
            <infinite-scroller style="display:block;overflow:auto;height:100px"><conversations-list data-test-id="all-conversations" style="display:block;height:300px">
            <div data-test-id="conversation"><a href="/app/recent">Recent fixture</a></div>
            <div data-test-id="conversation"><a href="https://example.com/app/outside">Outside fixture</a></div>
            <div data-test-id="conversation"><a href="/app">New chat</a></div>
            </conversations-list></infinite-scroller></body></html>"#})).unwrap();
        self.eval("window.fixtureLocation=new URL('https://gemini.google.com/app?authuser=2'); window.deleted=false; window.scrolls=0; document.querySelector('infinite-scroller').addEventListener('scroll',()=>scrolls++);");
    }
    fn step(&mut self, scroll: bool) -> Value {
        self.eval(&format!(
            "((location)=>({})({{scroll:{scroll}}}))(window.fixtureLocation)",
            adapter::script("list-conversations.js").unwrap()
        ))
    }
    fn read(&mut self) -> Value {
        self.eval(&format!(
            "((location,fetch)=>({})({}))(window.fixtureLocation,window.fixtureFetch)",
            adapter::script("read-history.js").unwrap(),
            json!({"url":"https://gemini.google.com/_/BardChatUi/data/batchexecute?rpcids=MaZiqc&authuser=2", "scope":"2", "post":"fixture-read-only-body", "timeoutMs":1000})
        ))
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        let _ = browser::close_tab(self.port, &self.target);
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixture only"]
fn history_refresh_uses_same_document_and_never_treats_auth_or_transport_failure_as_empty() {
    let mut page = Page::new();
    page.fixture();
    page.eval(
        r#"window.originalDocument=document; window.requests=[]; window.replyStatus=200;
        window.fixtureFetch=async(url,options)=>{
          requests.push({url,options});
          return {url,status:replyStatus,text:async()=>JSON.stringify({snapshot:requests.length})};
        };"#,
    );
    assert_eq!(page.read()["body"], "{\"snapshot\":1}");
    assert_eq!(page.read()["body"], "{\"snapshot\":2}");
    assert_eq!(page.eval("document===originalDocument"), true);
    assert_eq!(
        page.eval("requests.map(r=>r.options.cache)"),
        json!(["no-store", "no-store"])
    );
    assert_eq!(
        page.eval("requests[0].options.body"),
        "fixture-read-only-body"
    );
    page.eval("replyStatus=403");
    assert_eq!(page.read()["state"], "auth_required");
    page.eval("replyStatus=503");
    assert_eq!(page.read()["state"], "transport_error");
    page.eval("fixtureLocation=new URL('https://gemini.google.com/u/1/app')");
    assert_eq!(page.read()["state"], "error");
    page.eval("fixtureLocation=new URL('https://www.google.com/sorry/index')");
    assert_eq!(page.read()["state"], "verification_required");
    assert_eq!(
        page.eval("requests.length"),
        4,
        "account switches/challenges must stop before fetch"
    );
    assert_eq!(page.eval("deleted"), false);
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixture only"]
fn provider_history_reads_complete_active_and_archived_pages_and_rejects_partial_results() {
    let mut page = Page::new();
    page.fixture();
    for (provider, origin, path) in [
        ("claude-web", "https://claude.ai", "/chat/"),
        ("chatgpt-web", "https://chatgpt.com", "/c/"),
    ] {
        page.eval(r#"window.historyReads=[];window.historyBroken=false;window.fakeFetch=async(raw,options)=>{
            const u=new URL(raw,fixtureLocation);historyReads.push(u.pathname+u.search);
            if(u.pathname==='/api/auth/session')return {ok:true,json:async()=>({accessToken:'fixture-only',account:{id:'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'}})};
            const archived=u.searchParams.get('archived')==='true'||u.searchParams.get('is_archived')==='true';
            const offset=Number(u.searchParams.get('offset'));
            const id=archived?'33333333-3333-3333-3333-333333333333':offset?'22222222-2222-2222-2222-222222222222':'11111111-1111-1111-1111-111111111111';
            let value=u.pathname.includes('chat_conversations_v2')?{data:[{uuid:id}],has_more:!archived&&!offset}:{items:[{id}],total:archived?1:2,offset};
            if(historyBroken&&offset)value={items:[],data:[]};
            return {ok:true,status:200,json:async()=>value};
        };"#);
        page.eval(&format!("fixtureLocation=new URL({})", json!(origin)));
        let script = adapter::script_for(provider, "read-history.js").unwrap();
        let expr=format!("((location,document,fetch)=>({script})({{timeoutMs:10000}}))(fixtureLocation,{{title:'Fixture',cookie:'lastActiveOrg=aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'}},fakeFetch)");
        let result = page.eval(&expr);
        assert_eq!(result["complete"], true, "{provider}: {result}");
        assert_eq!(result["urls"].as_array().unwrap().len(), 3);
        assert!(result["urls"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.as_str().unwrap().starts_with(&format!("{origin}{path}"))));
        assert!(page
            .eval("historyReads.some(u=>u.includes('archived=true'))")
            .as_bool()
            .unwrap());
        page.eval("historyBroken=true");
        assert_eq!(page.eval(&expr)["state"], "error");
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixture only"]
fn claude_upload_chip_reports_exact_filename_without_duplicate_labels() {
    let mut page = Page::new();
    page.fixture();
    page.eval(r#"document.body.innerHTML='<div data-testid="attachments"><div data-testid="file-thumbnail"><span>sample.txt</span><span>sample.txt</span><span>1 line</span><span>TXT</span><button aria-label="Remove sample.txt">Remove</button></div></div>';"#);
    let script = adapter::script_for("claude-web", "driver.js").unwrap();
    let result = page.eval(&format!("({script})({{op:'chips'}})"));
    assert_eq!(
        result,
        json!([{"name":"sample.txt","pending":false,"failed":false}])
    );
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixture only"]
fn chatgpt_upload_chip_uses_composer_attachment_identity_and_loading_state() {
    let mut page = Page::new();
    page.fixture();
    page.eval(r#"document.body.innerHTML='<div data-composer-attachments><span class="group/composer-attachment"><span>sample.txt</span><button aria-label="sample.txt">Open</button><button aria-label="Remove sample.txt">Remove</button><span id="progress" role="progressbar"></span></span></div><div data-testid="file-tile">Old transcript attachment</div>';"#);
    let script = adapter::script_for("chatgpt-web", "driver.js").unwrap();
    let expr = format!("({script})({{op:'chips'}})");
    assert_eq!(
        page.eval(&expr),
        json!([{"name":"sample.txt","pending":true,"failed":false}])
    );
    page.eval("document.getElementById('progress').remove()");
    assert_eq!(
        page.eval(&expr),
        json!([{"name":"sample.txt","pending":false,"failed":false}])
    );
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixture only"]
fn collects_pinned_and_recent_urls_with_account_scope_without_deleting() {
    let mut page = Page::new();
    page.fixture();
    let value = page.step(false);
    assert_eq!(value["state"], "ready");
    assert_eq!(
        value["urls"],
        json!([
            "https://gemini.google.com/app/pinned?authuser=2",
            "https://gemini.google.com/app/recent?authuser=2"
        ])
    );
    assert_eq!(page.eval("deleted"), false);
    assert_eq!(page.eval("scrolls"), 0);
    assert!(
        value.get("complete").is_none(),
        "DOM row count must never assert completion"
    );
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixture only"]
fn delayed_and_virtualized_rows_are_observed_without_claiming_completion() {
    let mut page = Page::new();
    page.fixture();
    page.step(true);
    assert!(page.eval("scrolls").as_u64().unwrap() > 0);
    page.eval("document.querySelector('[data-test-id=all-conversations] a').setAttribute('href','/app/older')");
    let value = page.step(false);
    assert!(value["urls"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "https://gemini.google.com/app/older?authuser=2"));
    assert!(value.get("complete").is_none());
    assert_eq!(page.eval("deleted"), false);
    page.eval("fixtureLocation=new URL('https://accounts.google.com/signin')");
    assert_eq!(page.step(true)["state"], "auth_required");
}
