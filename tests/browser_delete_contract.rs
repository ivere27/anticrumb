//! Destructive-adapter fixtures run in about:blank, never on an account website.
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
        let r = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true}),
            )
            .unwrap();
        assert!(r.get("exceptionDetails").is_none(), "{r}");
        r["result"]["value"].clone()
    }
    fn fixture(&mut self, html: &str) {
        let tree = self.cdp.call("Page.getFrameTree", json!({})).unwrap();
        self.cdp.call("Page.setDocumentContent", json!({"frameId":tree["frameTree"]["frame"]["id"],"html":format!("<!doctype html><html><body>{html}</body></html>")})).unwrap();
        self.eval("delete window.__anticrumbChatDeletion; delete window.__anticrumbChatLookup; window.fixtureLocation={href:'https://gemini.google.com/app/target'}; window.actions=[];");
    }
    fn step(&mut self) -> Value {
        self.delete_step("https://gemini.google.com/app/target")
    }
    fn delete_step(&mut self, url: &str) -> Value {
        self.eval(&format!(
            "((location)=>({})({{url:{}}}))(window.fixtureLocation)",
            adapter::script("delete-conversation.js").unwrap(),
            json!(url)
        ))
    }
    fn history_step(&mut self) -> Value {
        self.eval(&format!(
            "((location)=>({})({{scroll:false}}))(window.fixtureLocation)",
            adapter::script("list-conversations.js").unwrap()
        ))
    }
    fn provider_delete_step(&mut self, provider: &str, url: &str) -> Value {
        self.eval(&format!(
            "((location)=>({})({}))(window.fixtureLocation)",
            adapter::script_for(provider, "delete-conversation.js").unwrap(),
            json!({"url":url})
        ))
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        let _ = browser::close_tab(self.port, &self.target);
    }
}

const FIXTURE: &str = r#"
<button data-test-id="delete-button" onclick="actions.push('unrelated')">Global</button>
<div role="dialog" id="old-dialog"><button class="confirm-button" onclick="actions.push('old-dialog')">Old</button></div>
<conversations-list data-test-id="all-conversations">
<div data-test-id="conversation" id="other"><a href="https://gemini.google.com/app/other">Other</a><button data-test-id="actions-menu-button" onclick="actions.push('other')">...</button></div>
<div data-test-id="conversation" id="target"><a href="https://gemini.google.com/app/target">Target</a><button data-test-id="actions-menu-button" aria-controls="menu" onclick="actions.push('target-menu');document.getElementById('menu').hidden=false">...</button></div>
</conversations-list>
<div role="menu" id="hidden-menu" hidden><button data-test-id="delete-button" onclick="actions.push('hidden')">Hidden</button></div>
<div role="menu" id="menu" hidden><button data-test-id="delete-button" onclick="actions.push('target-delete');document.getElementById('confirm').hidden=false;this.parentElement.hidden=true"><span data-cds-part="paint" class="bg-fill-danger"></span>Unspecified</button></div>
<div role="dialog" id="confirm" hidden><button class="confirm-button" onclick="actions.push('target-confirm');this.parentElement.hidden=true;document.getElementById('target').remove();fixtureLocation.href='https://gemini.google.com/app'">Delete</button></div>
"#;

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn deletion_is_scoped_to_exact_url_menu_and_new_confirmation() {
    let mut page = Page::new();
    page.fixture(FIXTURE);
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(
        page.eval("actions"),
        json!(["target-menu", "target-delete", "target-confirm"])
    );
    assert_eq!(page.eval("!!document.getElementById('other')"), true);
    page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
    assert_eq!(page.step()["state"], "deleted");
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn missing_chat_account_switch_and_unknown_dialog_do_not_delete() {
    let mut page = Page::new();
    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='https://www.google.com/sorry/index'");
    assert_eq!(page.step()["state"], "verification_required");
    assert_eq!(page.eval("actions"), json!([]));
    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='about:blank'");
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(page.eval("actions"), json!([]));
    page.fixture(FIXTURE);
    page.eval("document.getElementById('target').remove()");
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(page.eval("actions"), json!([]));
    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='https://gemini.google.com/app/target?authuser=1'");
    assert_eq!(page.step()["state"], "error");
    assert_eq!(page.eval("actions"), json!([]));
    page.fixture(FIXTURE);
    page.step();
    page.step();
    page.eval("document.querySelector('#confirm button').className='unknown'");
    assert_eq!(page.step()["state"], "error");
    assert_eq!(
        page.eval("actions"),
        json!(["target-menu", "target-delete"])
    );
    assert_eq!(page.eval("!!document.getElementById('target')"), true);
    page.fixture(FIXTURE);
    page.step();
    page.eval("fixtureLocation.href='https://gemini.google.com/app/other'");
    assert_eq!(page.step()["state"], "error");
    assert_eq!(page.eval("actions"), json!(["target-menu"]));
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn current_web_confirmation_uses_action_identity_without_localized_labels() {
    let mut page = Page::new();
    page.fixture(FIXTURE);
    page.eval(r#"document.getElementById('confirm').innerHTML = '<message-dialog><gem-button><button onclick="actions.push(\'cancel\')">Cancel</button></gem-button><gem-button cdkfocusinitial><button jslog="186009;track:generic_click" onclick="actions.push(\'target-confirm\');document.getElementById(\'target\').remove();document.getElementById(\'confirm\').hidden=true">Delete</button></gem-button></message-dialog>'"#);
    page.step();
    page.step();
    page.step();
    page.step();
    assert_eq!(
        page.eval("actions"),
        json!(["target-menu", "target-delete", "target-confirm"])
    );
    page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
    assert_eq!(page.step()["state"], "deleted");
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn history_verification_is_not_empty_history_and_hidden_challenges_are_ignored() {
    let mut page = Page::new();
    page.fixture("<conversations-list data-test-id='all-conversations'></conversations-list>");
    page.eval("fixtureLocation.href='https://www.google.com/sorry/index'");
    assert_eq!(page.history_step()["state"], "verification_required");
    for path in [
        "https://www.google.com/recaptcha/api2/bframe",
        "https://accounts.google.com/challenge/fixture",
    ] {
        page.fixture("<conversations-list data-test-id='all-conversations'></conversations-list><iframe id='challenge' srcdoc='fixture'></iframe>");
        // Change only the DOM attribute used by the adapter; no network request
        // is issued because srcdoc supplies this synthetic iframe's content.
        page.eval(&format!(
            "document.getElementById('challenge').setAttribute('src',{})",
            json!(path)
        ));
        assert_eq!(page.history_step()["state"], "verification_required");
        page.eval("document.getElementById('challenge').hidden=true");
        assert_eq!(page.history_step()["state"], "ready");
    }
    assert_eq!(page.eval("actions"), json!([]));
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn home_page_missing_row_and_u_prefix_match_only_the_recorded_account() {
    let mut page = Page::new();
    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='https://gemini.google.com/u/0/app/target'");
    page.step();
    page.step();
    page.step();
    page.step();
    page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
    assert_eq!(
        page.step()["state"],
        "deleted",
        "/u/0/app/<id> matches recorded /app/<id>"
    );

    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='https://gemini.google.com/app'");
    page.eval("document.getElementById('target').remove()");
    let first = page.step();
    assert_eq!(first["state"], "waiting");
    assert_eq!(first["step"], "exact conversation link in sidebar");
    page.eval("window.__anticrumbChatLookup.bottomSince = Date.now() - 3100");
    assert_eq!(page.step()["state"], "missing_on_page");

    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='https://gemini.google.com/u/1/app/target'");
    assert_eq!(page.step()["state"], "error");
    assert_eq!(page.eval("actions"), json!([]));
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn successive_sidebar_deletions_reuse_one_document_without_navigation() {
    let mut page = Page::new();
    page.fixture(FIXTURE);
    page.eval("fixtureLocation.href='https://gemini.google.com/app'; window.documentMarker={}; window.originalDocument=document;");
    for id in ["target", "second"] {
        if id == "second" {
            page.eval(r#"document.querySelector('conversations-list').insertAdjacentHTML('beforeend',
              '<div data-test-id="conversation" id="second"><a href="/app/second">Second</a><button data-test-id="actions-menu-button" aria-controls="menu" onclick="actions.push(\'second-menu\');document.getElementById(\'menu\').hidden=false">...</button></div>');
              document.querySelector('#confirm button').onclick=()=>{actions.push('second-confirm');document.getElementById('second').remove();document.getElementById('confirm').hidden=true;};"#);
        }
        let url = format!("https://gemini.google.com/app/{id}");
        for _ in 0..4 {
            assert_eq!(page.delete_step(&url)["state"], "waiting");
        }
        page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
        assert_eq!(page.delete_step(&url)["state"], "deleted");
        assert_eq!(
            page.eval("document===originalDocument && !!documentMarker"),
            true
        );
        assert_eq!(
            page.eval("fixtureLocation.href"),
            "https://gemini.google.com/app"
        );
    }
    assert_eq!(
        page.eval("actions"),
        json!([
            "target-menu",
            "target-delete",
            "target-confirm",
            "second-menu",
            "target-delete",
            "second-confirm"
        ])
    );
    assert_eq!(page.eval("!!document.getElementById('other')"), true);
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn deletion_searches_virtualized_sidebar_from_top_before_claiming_missing() {
    let mut page = Page::new();
    page.fixture(FIXTURE);
    page.eval(r#"fixtureLocation.href='https://gemini.google.com/app';
        const list=document.querySelector('conversations-list');
        list.style='display:block;height:1000px';
        const scroll=document.createElement('infinite-scroller');
        scroll.style='display:block;overflow:auto;height:100px';
        list.before(scroll); scroll.append(list); scroll.scrollTop=900;
        const row=document.getElementById('target'); row.remove();
        scroll.addEventListener('scroll',()=>{if(scroll.scrollTop < 100 && !row.isConnected)list.prepend(row);});"#);
    assert_eq!(page.step()["state"], "waiting");
    assert_eq!(
        page.eval("document.querySelector('infinite-scroller').scrollTop"),
        0
    );
    assert_eq!(page.step()["step"], "conversation actions menu");
    assert_eq!(page.eval("actions"), json!(["target-menu"]));
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn provider_deletions_scope_actions_and_do_not_wait_on_success_toasts() {
    for (provider, origin, path) in [
        ("claude-web", "https://claude.ai", "/chat/"),
        ("chatgpt-web", "https://chatgpt.com", "/c/"),
    ] {
        let mut page = Page::new();
        let url = format!("{origin}{path}11111111-1111-1111-1111-111111111111");
        page.fixture(&format!(r#"
          <div role="dialog"><button onclick="actions.push('unrelated')">Old dialog</button></div>
          <div id="target"><a href="{url}">Test</a><button aria-haspopup="menu" data-row-action data-testid="history-item-options" aria-label="Unspecified" aria-controls="menu" onclick="actions.push('menu');document.getElementById('menu').hidden=false">Actions</button></div>
          <div><a href="{origin}{path}22222222-2222-2222-2222-222222222222">Other</a><button aria-haspopup="menu" data-row-action data-testid="history-item-options" aria-label="Unspecified" onclick="actions.push('wrong-chat')">Other actions</button></div>
          <div id="menu" role="menu" hidden><button role="menuitem" data-testid="delete-chat-trigger" class="text-danger" onclick="actions.push('delete');this.parentElement.hidden=true;document.getElementById('confirm').hidden=false">Delete</button></div>
          <div id="confirm" role="alertdialog" hidden><button data-cds="Button" onclick="actions.push('cancel')">Cancel</button><button type="submit" data-cds="Button" onclick="actions.push('confirm');this.parentElement.hidden=true;document.getElementById('target').remove();document.getElementById('toast').hidden=false"><span data-cds-part="paint" class="bg-fill-danger"></span>Unspecified</button></div>
          <div id="toast" role="dialog" hidden>Session deleted</div>
        "#));
        page.eval(&format!(
            "window.fixtureLocation=new URL({})",
            json!(origin.to_owned() + "/")
        ));
        for _ in 0..4 {
            assert_eq!(
                page.provider_delete_step(provider, &url)["state"],
                "waiting",
                "{provider}"
            );
        }
        page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
        assert_eq!(
            page.provider_delete_step(provider, &url)["state"],
            "deleted"
        );
        assert_eq!(page.eval("actions"), json!(["menu", "delete", "confirm"]));
        assert_eq!(page.eval("document.getElementById('toast').hidden"), false);
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn provider_delete_requires_one_identifiable_confirmation_button() {
    for (provider, origin, path) in [
        ("claude-web", "https://claude.ai", "/chat/"),
        ("chatgpt-web", "https://chatgpt.com", "/c/"),
    ] {
        for ambiguous in [false, true] {
            let mut page = Page::new();
            let url = format!("{origin}{path}11111111-1111-1111-1111-111111111111");
            page.fixture(&format!(r#"
              <div><a href="{url}">Test</a><button aria-haspopup="menu" data-row-action command="toggle-popover" aria-controls="menu" onclick="document.getElementById('menu').hidden=false">Unspecified</button></div>
              <div id="menu" role="menu" hidden><button data-testid="delete-chat-trigger" role="menuitem" class="text-danger" onclick="actions.push('delete');this.parentElement.hidden=true;document.getElementById('confirm').hidden=false">Unspecified</button></div>
              <div id="confirm" role="alertdialog" hidden>
                <button type="button" data-cds="Button" onclick="actions.push('cancel')">Cancel</button>
                <button id="confirm-button" type="submit" data-cds="Button" onclick="actions.push('confirm')"><span data-cds-part="paint" class="bg-fill-danger"></span>Delete</button>
              </div>
            "#));
            page.eval(&format!("fixtureLocation=new URL({})", json!(origin)));
            if ambiguous {
                page.eval(
                    "const b=document.getElementById('confirm-button');b.after(b.cloneNode(true))",
                );
            } else {
                page.eval("const b=document.getElementById('confirm-button');b.type='button';b.querySelector('span').remove()");
            }
            page.provider_delete_step(provider, &url);
            page.provider_delete_step(provider, &url);
            assert_eq!(
                page.provider_delete_step(provider, &url)["state"],
                "error",
                "{provider}, ambiguous={ambiguous}"
            );
            assert_eq!(page.eval("actions"), json!(["delete"]));
        }
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn provider_archived_chats_use_the_current_chat_title_menu() {
    for (provider, origin, path) in [
        ("claude-web", "https://claude.ai", "/chat/"),
        ("chatgpt-web", "https://chatgpt.com", "/c/"),
    ] {
        let mut page = Page::new();
        let url = format!("{origin}{path}11111111-1111-1111-1111-111111111111");
        page.fixture(r#"
      <header><div data-testid="session-title-split"><button id="radix-title" class="text-conversation-title" aria-haspopup="menu" aria-controls="menu" onclick="actions.push('menu');document.getElementById('menu').hidden=false" onpointerdown="actions.push('menu');document.getElementById('menu').hidden=false">Test title</button></div></header>
      <button aria-haspopup="menu" aria-label="More actions" onclick="actions.push('wrong-menu')">Response actions</button>
      <div id="menu" role="menu" hidden><button role="menuitem" data-testid="delete-session-trigger" class="text-danger" onclick="actions.push('delete');this.parentElement.hidden=true;document.getElementById('confirm').hidden=false">Delete</button></div>
      <div id="confirm" role="dialog" hidden><button type="submit" data-cds="Button" onclick="actions.push('confirm');this.parentElement.hidden=true"><span data-cds-part="paint" class="bg-fill-danger"></span>Unspecified</button></div>
    "#);
        page.eval(&format!(
            "fixtureLocation=new URL({})",
            json!(format!(
                "{origin}{path}22222222-2222-2222-2222-222222222222"
            ))
        ));
        assert_eq!(
            page.provider_delete_step(provider, &url)["step"],
            "exact conversation row"
        );
        assert_eq!(page.eval("actions"), json!([]));
        page.eval(&format!("fixtureLocation=new URL({})", json!(url)));
        for _ in 0..4 {
            assert_eq!(
                page.provider_delete_step(provider, &url)["state"],
                "waiting"
            );
        }
        page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
        assert_eq!(
            page.provider_delete_step(provider, &url)["state"],
            "deleted"
        );
        assert_eq!(page.eval("actions"), json!(["menu", "delete", "confirm"]));
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn provider_delete_recovers_when_hydration_replaces_the_menu_button() {
    for (provider, origin, path) in [
        ("claude-web", "https://claude.ai", "/chat/"),
        ("chatgpt-web", "https://chatgpt.com", "/c/"),
    ] {
        let mut page = Page::new();
        let url = format!("{origin}{path}11111111-1111-1111-1111-111111111111");
        page.fixture(&format!(r#"
          <div id="target"><a href="{url}">Test</a><button id="trigger" aria-haspopup="menu" data-row-action data-testid="history-item-options" aria-label="Unspecified" aria-controls="menu">Actions</button></div>
          <div id="menu" role="menu" hidden><button role="menuitem" data-testid="delete-chat-trigger" class="text-danger" onclick="actions.push('delete');this.parentElement.hidden=true;document.getElementById('confirm').hidden=false">Delete</button></div>
          <div id="confirm" role="alertdialog" hidden><button type="submit" data-cds="Button" onclick="actions.push('confirm');this.parentElement.hidden=true;document.getElementById('target').remove()"><span data-cds-part="paint" class="bg-fill-danger"></span>Unspecified</button></div>
        "#));
        page.eval(&format!("fixtureLocation=new URL({})", json!(origin)));
        page.eval(r#"document.getElementById('trigger').onclick=function(){
            const replacement=this.cloneNode(true);
            replacement.onclick=()=>{actions.push('menu');document.getElementById('menu').hidden=false;};
            this.replaceWith(replacement);
        };window.originalDocument=document;"#);
        assert_eq!(
            page.provider_delete_step(provider, &url)["step"],
            "conversation actions menu"
        );
        assert_eq!(
            page.provider_delete_step(provider, &url)["step"],
            "conversation actions button replaced"
        );
        assert_eq!(page.eval("actions"), json!([]));
        for _ in 0..4 {
            assert_eq!(
                page.provider_delete_step(provider, &url)["state"],
                "waiting"
            );
        }
        page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-1600");
        assert_eq!(
            page.provider_delete_step(provider, &url)["state"],
            "deleted"
        );
        assert_eq!(page.eval("actions"), json!(["menu", "delete", "confirm"]));
        assert_eq!(page.eval("document===originalDocument"), true);
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn provider_delete_never_skips_a_delayed_confirmation_or_accepts_other_origins() {
    for (provider, origin, path) in [
        ("claude-web", "https://claude.ai", "/chat/"),
        ("chatgpt-web", "https://chatgpt.com", "/c/"),
    ] {
        let mut page = Page::new();
        let url = format!("{origin}{path}11111111-1111-1111-1111-111111111111");
        page.fixture(&format!(r#"<div id="target"><a href="{url}">Test</a><button aria-haspopup="menu" data-row-action data-testid="history-item-options" aria-label="Unspecified" aria-controls="menu" onclick="document.getElementById('menu').hidden=false">Actions</button></div>
          <div id="menu" role="menu" hidden><button role="menuitem" data-testid="delete-chat-trigger" class="text-danger" onclick="actions.push('delete');this.parentElement.hidden=true;document.getElementById('target').remove()"><span data-cds-part="paint" class="bg-fill-danger"></span>Unspecified</button></div>"#));
        page.eval(&format!(
            "window.fixtureLocation=new URL({})",
            json!(origin.to_owned() + "/")
        ));
        assert_eq!(
            page.provider_delete_step(
                provider,
                "https://example.com/c/11111111-1111-1111-1111-111111111111"
            )["state"],
            "error"
        );
        page.provider_delete_step(provider, &url);
        page.provider_delete_step(provider, &url);
        page.eval("window.__anticrumbChatDeletion.absentSince=Date.now()-10000");
        assert_eq!(
            page.provider_delete_step(provider, &url)["step"],
            "delete confirmation"
        );
        assert_eq!(page.eval("actions"), json!(["delete"]));
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn gemini_home_without_sidebar_opens_only_the_exact_search_result() {
    let mut page = Page::new();
    page.fixture(r#"<button aria-label="Search chats" onclick="actions.push('search');document.getElementById('search').hidden=false">Search</button>
      <div role="dialog" id="search" hidden><input type="search"><a href="https://gemini.google.com/app/other" onclick="event.preventDefault();actions.push('wrong')">Other</a><a href="https://gemini.google.com/app/target" onclick="event.preventDefault();actions.push('target');this.parentElement.hidden=true">Target</a></div>"#);
    page.eval("fixtureLocation.href='https://gemini.google.com/app'");
    assert_eq!(page.step()["step"], "opening conversation search");
    assert_eq!(
        page.step()["step"],
        "opening exact conversation from search"
    );
    assert_eq!(page.eval("actions"), json!(["search", "target"]));
}
