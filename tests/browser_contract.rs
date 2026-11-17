use anticrumb::{adapter, browser_host as browser, cdp::CdpClient};
use serde_json::{json, Value};
fn eval(cdp: &mut CdpClient, script: &str) -> Value {
    let v = cdp
        .call(
            "Runtime.evaluate",
            json!({"expression":script,"returnByValue":true}),
        )
        .unwrap();
    assert!(v.get("exceptionDetails").is_none(), "{v}");
    v["result"]["value"].clone()
}
fn action(cdp: &mut CdpClient, op: &str) -> Value {
    eval(
        cdp,
        &format!(
            "({})({})",
            adapter::script("driver.js").unwrap(),
            json!({"op":op})
        ),
    )
}

#[test]
#[ignore = "requires local Chrome CDP; response fixtures only, no provider requests"]
fn tool_urls_survive_provider_autolinks_without_changing_prose_or_code_examples() {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    fn set_response(cdp: &mut CdpClient, content: &str) {
        let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
        cdp.call("Page.setDocumentContent", json!({
            "frameId":tree["frameTree"]["frame"]["id"],
            "html":format!("<!doctype html><html><body><model-response><message-content data-testid='assistant-message' data-message-author-role='assistant'>{content}</message-content></model-response></body></html>")
        })).unwrap();
    }
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let urls = [
        "https://search.example.com/search?query=french+restaurants&page=1",
        "https://places.example.org/restaurants?query=french%20restaurants&name=caf%C3%A9",
        "https://maps.example.net/search?q=french%20restaurants#results",
    ];
    let payload = json!({"tool_calls":[
        {"tool":"web_fetch","args":{"url":urls[0]}},
        {"tool":"browser_fetch","args":{"url":urls[1],"wait_ms":2500}},
        {"tool":"browser_control","args":{"action":"open","url":urls[2],"wait_ms":3000}}
    ]});
    let original = format!("[TOOL_CALL]\n{payload}\n[/TOOL_CALL]");
    let expected = anticrumb::tool_codec::parse(&original).unwrap().0;
    let mut linked = escape(&payload.to_string());
    for url in urls {
        // Gemini rewrites some plain URLs into Google Search links. Other
        // providers also render URLs as anchors, including unchanged hrefs.
        linked = linked.replace(&escape(url), &format!(
            "<response-element><link-block><a href='https://www.google.com/search?q=redirected'>{}</a></link-block></response-element>",
            escape(url)
        ));
    }
    let link = "<a href='https://example.com/destination'>https://example.com/label</a>";
    let prose_link = "[https://example.com/label](https://example.com/destination)";
    let collision = "\u{e000}anticrumb-link:0\u{e001}";
    for provider in ["gemini-web", "claude-web", "chatgpt-web"] {
        let script = adapter::script_for(provider, "extract-response.js").unwrap();
        let content = format!(
            "<p>Before {link} {collision}</p><pre><code>{}</code></pre>\
             <p><span>[TOOL_</span><span>CALL]</span>\n{linked}\n[/TOOL_CALL]</p>\
             <p>After {link}</p>\
             <p>[TOOL_CALL]</p><p>{{&quot;tool&quot;:&quot;write_file&quot;,&quot;args&quot;:{{&quot;path&quot;:&quot;notes.md&quot;,&quot;content&quot;:&quot;<a href='https://example.com/docs'>Guide</a>&quot;}}}}</p><p>[/TOOL_CALL]</p>",
            escape(&original)
        );
        set_response(&mut cdp, &content);
        let result = eval(&mut cdp, &script);
        let text = result["text"].as_str().unwrap();
        let (calls, prose) = anticrumb::tool_codec::parse(text).unwrap();
        assert_eq!(&calls[..3], expected.as_slice(), "{provider}: {text}");
        assert_eq!(calls.len(), 4, "code examples must not execute");
        assert_eq!(
            calls[3].args["content"],
            "[Guide](https://example.com/docs)"
        );
        assert!(prose.contains(&format!("Before {prose_link} {collision}")));
        assert!(prose.contains(&format!("After {prose_link}")));
        for (call, url) in calls.iter().zip(urls) {
            let actual = call.args["url"].as_str().unwrap();
            assert_eq!(actual, url);
            assert!(anticrumb::web::validate_http_url(actual, false).is_ok());
        }

        // A still-streaming envelope must not emit Markdown wrappers either.
        set_response(&mut cdp, &format!("<p>[TOOL_CALL]\n{linked}</p>"));
        assert_eq!(
            eval(&mut cdp, &script)["text"],
            format!("[TOOL_CALL]\n{payload}")
        );

        // Literal fenced examples retain normal link formatting, even when a
        // renderer presents the fence as text rather than a <pre> element.
        for fence in ["```", "~~~"] {
            set_response(&mut cdp, &format!("<p>{fence}text\n[TOOL_CALL]\n{{&quot;url&quot;:&quot;{link}&quot;}}\n[/TOOL_CALL]\n{fence}</p>"));
            let result = eval(&mut cdp, &script);
            let text = result["text"].as_str().unwrap();
            assert!(text.contains(prose_link), "{provider}: {text}");
            assert!(anticrumb::tool_codec::parse(text).unwrap().0.is_empty());
        }

        // Deferred links still undergo normal Markdown table escaping.
        set_response(&mut cdp, "<table><tr><th>Link</th></tr><tr><td><a href='https://example.com/destination'>https://example.com/a|b</a></td></tr></table>");
        let result = eval(&mut cdp, &script);
        assert!(result["text"]
            .as_str()
            .unwrap()
            .contains("[https://example.com/a\\|b](https://example.com/destination)"));
    }
}

#[test]
#[ignore = "requires local Chrome CDP; synthetic fixtures only"]
fn provider_controls_and_completion_do_not_depend_on_display_labels() {
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    for (provider, send, stop, copy) in [
        (
            "claude-web",
            "chat-input-send",
            "chat-input-stop",
            "action-bar-retry",
        ),
        (
            "claude-web",
            "send-button",
            "stop-button",
            "action-bar-copy",
        ),
        (
            "chatgpt-web",
            "send-button",
            "stop-button",
            "copy-turn-action-button",
        ),
        ("chatgpt-web", "", "", "good-response-turn-action-button"),
    ] {
        let html = format!(
            r##"<!doctype html><html><body>
          <button aria-label="Stop" onclick="actions.push('wrong-stop')">Stop</button>
          <button aria-label="Send" onclick="actions.push('wrong-send')">Send</button>
          <form><button type="submit" onclick="actions.push('wrong-submit')">Send</button></form>
          <form data-chatgpt-composer onsubmit="return false"><fieldset>
            <div data-testid="chat-input" data-composer-markdown contenteditable="true"></div>
            <button id="send" data-testid="{send}" type="submit" aria-label="Unspecified" onclick="actions.push('send')">Unspecified</button>
            <button id="stop" data-testid="{stop}" type="button" hidden aria-label="Unspecified" onclick="actions.push('stop')"><svg class="icon-primary-action"><path d="M4.5 5.75C4.5 5.05964 5.05964 4.5 5.75 4.5H14.25C14.9404 4.5 15.5 5.05964 15.5 5.75V14.25C15.5 14.9404 14.9404 15.5 14.25 15.5H5.75C5.05964 15.5 4.5 14.9404 4.5 14.25V5.75Z"/></svg></button>
            <button type="button" onclick="actions.push('wrong-voice')"><svg class="icon-primary-action"><use href="#voice-icon"/></svg></button>
          </fieldset></form>
          <div data-testid="conversation">
            <article><div data-testid="assistant-message" data-message-author-role="assistant">Previous<button data-testid="{copy}">Unspecified</button></div></article>
            <article id="turn"><div id="response" data-testid="assistant-message" data-message-author-role="assistant"><p>Latest</p><button data-testid="{copy}" aria-label="Unspecified">Unspecified</button></div></article>
          </div>
        </body></html>"##
        );
        let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
        cdp.call(
            "Page.setDocumentContent",
            json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
        )
        .unwrap();
        eval(&mut cdp, "window.actions=[]");
        let driver = adapter::script_for(provider, "driver.js").unwrap();
        let extract = adapter::script_for(provider, "extract-response.js").unwrap();
        let run =
            |cdp: &mut CdpClient, op: &str| eval(cdp, &format!("({driver})({})", json!({"op":op})));
        assert_eq!(run(&mut cdp, "probe")["generating"], false, "{provider}");
        assert_eq!(run(&mut cdp, "prepare")["ok"], true);
        assert_eq!(run(&mut cdp, "send")["ok"], true);
        assert_eq!(eval(&mut cdp, "actions"), json!(["send"]));
        let response = eval(&mut cdp, &extract);
        assert_eq!(response["hasStop"], false);
        assert_eq!(response["hasActionBar"], true);
        eval(&mut cdp, "document.getElementById('send').disabled=true");
        assert_eq!(run(&mut cdp, "send")["ok"], false);

        eval(&mut cdp, "document.getElementById('stop').hidden=false");
        assert_eq!(run(&mut cdp, "probe")["generating"], true);
        assert_eq!(run(&mut cdp, "prepare")["ok"], false);
        let response = eval(&mut cdp, &extract);
        assert_eq!(response["hasStop"], true);
        assert_eq!(response["hasActionBar"], false);
        assert_eq!(run(&mut cdp, "stop")["ok"], true);
        assert_eq!(eval(&mut cdp, "actions"), json!(["send", "stop"]));

        // An earlier response's actions must not mark the latest one complete.
        eval(&mut cdp, "document.getElementById('stop').remove();document.querySelector('#response button').remove()");
        assert_eq!(eval(&mut cdp, &extract)["hasActionBar"], false);
        let state = if provider == "claude-web" {
            "document.getElementById('response').setAttribute('data-is-streaming','true')"
        } else {
            "document.getElementById('turn').setAttribute('data-talvt-turn-state','streaming')"
        };
        eval(&mut cdp, state);
        assert_eq!(eval(&mut cdp, &extract)["hasStop"], true);
        if provider == "chatgpt-web" {
            eval(
                &mut cdp,
                "document.getElementById('turn').setAttribute('data-talvt-turn-state','complete')",
            );
            assert_eq!(eval(&mut cdp, &extract)["hasActionBar"], true);
        }
    }
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no Google requests"]
fn locale_independent_ui_and_markdown_contract() {
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or("9222".into())
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    for label in ["Stop", "停止", "Arrêter", "Anhalten", "توقف"] {
        let html = format!(
            r#"<!doctype html><html><body>
        <rich-textarea><div contenteditable="true"></div></rich-textarea>
        <button data-test-id="send-button" aria-label="{label}" onclick="window.sent=(window.sent||0)+1">{label}</button>
        <temp-chat-button><gem-icon-button class="temp-chat-on"><button aria-label="{label}"></button></gem-icon-button></temp-chat-button>
        <model-response><message-content><p>Text</p><code-block><div class="code-block-decoration"><span>任意の表示</span></div><pre><code data-test-id="code-content">{{"tool":"read_file","args":{{"path":"a"}}}}</code></pre></code-block></message-content><button><mat-icon fonticon="content_copy"></mat-icon></button></model-response>
        <button class="unrelated" aria-label="Stop">unrelated</button>
        </body></html>"#
        );
        let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
        cdp.call(
            "Page.setDocumentContent",
            json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
        )
        .unwrap();
        eval(&mut cdp, "window.sent=0");
        assert_eq!(action(&mut cdp, "probe")["temporary"], true);
        assert_eq!(action(&mut cdp, "prepare")["ok"], true);
        assert_eq!(action(&mut cdp, "send")["ok"], true);
        assert_eq!(eval(&mut cdp, "window.sent"), 1);
        let extracted = eval(&mut cdp, &adapter::script("extract-response.js").unwrap());
        assert_eq!(extracted["hasStop"], false);
        assert_eq!(extracted["hasActionBar"], true);
        let (calls, text) =
            anticrumb::tool_codec::parse(extracted["text"].as_str().unwrap()).unwrap();
        assert!(calls.is_empty());
        assert!(text.contains("read_file"));
        eval(
            &mut cdp,
            r#"document.querySelector('message-content').innerHTML='<p>[TOOL_CALL]</p><p>{&quot;tool&quot;:&quot;read_file&quot;,&quot;args&quot;:{&quot;path&quot;:&quot;a&quot;}}</p><p>[/TOOL_CALL]</p>'"#,
        );
        let marked = eval(&mut cdp, &adapter::script("extract-response.js").unwrap());
        assert_eq!(
            anticrumb::tool_codec::parse(marked["text"].as_str().unwrap())
                .unwrap()
                .0
                .len(),
            1
        );
        eval(&mut cdp,"document.body.insertAdjacentHTML('beforeend','<button><mat-icon fonticon=stop></mat-icon></button>')");
        assert_eq!(action(&mut cdp, "probe")["generating"], true);
        assert_eq!(
            eval(&mut cdp, &adapter::script("extract-response.js").unwrap())["hasStop"],
            true
        );
        // Verify real Gemini Web <table-block> DOM extraction and streaming <p> raw-pipe holdback
        eval(
            &mut cdp,
            r#"document.querySelector('message-content').setAttribute('aria-busy', 'true');
            document.querySelector('message-content').innerHTML = '<p>Intro\n| Language | Type system |\n| :---</p>';"#,
        );
        let streaming_p = eval(&mut cdp, &adapter::script("extract-response.js").unwrap());
        assert_eq!(streaming_p["text"], "Intro");
        eval(
            &mut cdp,
            r#"document.querySelector('message-content').setAttribute('aria-busy', 'false');
            document.querySelector('message-content').innerHTML = '<div class="horizontal-scroll-wrapper"><div class="table-block-component"><response-element><table-block><div class="table-block"><div class="table-content"><table data-path-to-node="0"><thead><tr><th><span>Language</span></th><th><span>Type system</span></th></tr></thead><tbody><tr><td><span><b>Rust</b></span></td><td><span><code>Static</code>, strong</span></td></tr></tbody></table></div><div class="table-footer hide-from-message-actions"><button>More options</button></div></div></table-block></response-element></div></div>';"#,
        );
        let extracted_tbl = eval(&mut cdp, &adapter::script("extract-response.js").unwrap());
        assert_eq!(
            extracted_tbl["text"],
            "| Language | Type system |\n| --- | --- |\n| **Rust** | `Static`, strong |"
        );
    }
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no Google requests"]
fn new_model_four_levels_and_replaced_css_work_through_adapter_only() {
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or("9222".into())
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let levels=["Low","Medium","High","Ultra"].iter().map(|label|format!(r#"<gem-menu-item onclick="this.parentElement.querySelectorAll('gem-menu-item').forEach(e=>e.setAttribute('aria-checked','false'));this.setAttribute('aria-checked','true')"><span class="new-title">{label}</span></gem-menu-item>"#)).collect::<String>();
    for controlled in [
        format!(r#"<gem-menu id="thinking">{levels}</gem-menu>"#),
        format!(
            r#"<div id="thinking"><gem-menu hidden><gem-menu-item>Stale option</gem-menu-item></gem-menu><gem-menu>{levels}</gem-menu></div>"#
        ),
    ] {
        let html = format!(
            r#"<!doctype html><html><body>
        <gem-menu data-test-id="gem-mode-menu">
          <gem-menu-item data-mode-id="future-hydrogen"><span class="new-title">Everyday help</span><span class="new-description">4.0 Hydrogen</span></gem-menu-item>
          <gem-menu-item value="effort_menu" aria-controls="thinking"></gem-menu-item>
        </gem-menu>
        {controlled}
        <gem-menu><gem-menu-item value="unrelated">Unrelated menu</gem-menu-item></gem-menu>
        </body></html>"#
        );
        let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
        cdp.call(
            "Page.setDocumentContent",
            json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bundled = adapter::load("driver.js", None).unwrap();
        std::fs::write(dir.path().join("driver.js"), bundled.as_ref()).unwrap();
        let run = |cdp: &mut CdpClient, args: Value| {
            eval(
                cdp,
                &format!(
                    "({})({args})",
                    adapter::load("driver.js", Some(dir.path())).unwrap()
                ),
            )
        };
        assert_eq!(run(&mut cdp, json!({"op":"models"}))[0]["sublabel"], "");
        // Replace only adapter selectors. Rust and the terminal client are unchanged.
        let changed = bundled
            .replace(".label,.mode-title", ".new-title")
            .replace(".sublabel,.mode-desc", ".new-description")
            .replace("thinking_level", "effort_menu");
        std::fs::write(dir.path().join("driver.js"), changed).unwrap();
        let models = run(&mut cdp, json!({"op":"models"}));
        assert_eq!(models.as_array().unwrap().len(), 1);
        assert_eq!(models[0]["mode_id"], "future-hydrogen");
        assert_eq!(models[0]["sublabel"], "4.0 Hydrogen");
        assert_eq!(
            run(&mut cdp, json!({"op":"thinking_open"}))["available"],
            true
        );
        let levels = run(&mut cdp, json!({"op":"models","thinking":true}));
        assert_eq!(levels.as_array().unwrap().len(), 4);
        assert_eq!(levels[3]["label"], "Ultra");
        assert_eq!(
            run(
                &mut cdp,
                json!({"op":"select","thinking":true,"id":levels[3]["mode_id"]})
            )["ok"],
            true
        );
        assert_eq!(
            run(&mut cdp, json!({"op":"models","thinking":true}))[3]["selected"],
            true
        );
    }
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no Google requests"]
fn catalog_discovers_each_model_waits_for_levels_and_restores_selection() {
    use anticrumb::{
        agent::CancelToken,
        gemini::{ChatSessionMode, GeminiTab},
    };
    use std::time::{Duration, Instant};
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or("9222".into())
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let html = r#"<!doctype html><html><body>
      <rich-textarea><div contenteditable="true"> </div></rich-textarea>
      <button data-test-id="bard-mode-menu-button">Menu</button>
      <gem-menu data-test-id="gem-mode-menu">
        <gem-menu-item data-mode-id="original" aria-checked="true" onclick="selectModel(this)"><span class="label">Quick</span><span class="sublabel">Original</span></gem-menu-item>
        <gem-menu-item data-mode-id="future-hydrogen" onclick="selectModel(this)"><span class="label">Help</span><span class="sublabel">4.0 Hydrogen</span></gem-menu-item>
        <gem-menu-item value="thinking_level" aria-haspopup="false" aria-expanded="false" onclick="showLevels()">Effort
          <div class="expand-content" id="thinking" hidden></div>
        </gem-menu-item>
      </gem-menu>
      <gem-menu><gem-menu-item value="unrelated">Unrelated menu</gem-menu-item></gem-menu>
      <script>
        let selectedLevel='Low';
        let thinkingClicks=0;
        function selectModel(el) {
          document.querySelectorAll('[data-mode-id]').forEach(e=>e.setAttribute('aria-checked','false'));
          el.setAttribute('aria-checked','true'); selectedLevel='Low';
          document.querySelector('[value="thinking_level"]').setAttribute('aria-expanded','false');
          document.getElementById('thinking').hidden=true;
          document.getElementById('thinking').innerHTML='';
        }
        function showLevels() {
          thinkingClicks++;
          const trigger=document.querySelector('[value="thinking_level"]');
          const menu=document.getElementById('thinking');
          const open=trigger.getAttribute('aria-expanded')!=='true';
          trigger.setAttribute('aria-expanded',String(open)); menu.hidden=!open;
          if(!open)return;
          menu.innerHTML='';
          setTimeout(()=>{
            const future=document.querySelector('[data-mode-id="future-hydrogen"]').getAttribute('aria-checked')==='true';
            for(const label of (future?['Low','Medium','High','Ultra']:['Low','High'])) {
              const item=document.createElement('gem-menu-item');
              item.innerHTML='<span class="label">'+label+'</span>';
              item.setAttribute('role','menuitemradio');
              item.setAttribute('aria-checked',String(selectedLevel===label));
              item.onclick=event=>{event.stopPropagation();selectedLevel=label; menu.querySelectorAll('gem-menu-item').forEach(e=>e.setAttribute('aria-checked','false'));item.setAttribute('aria-checked','true');};
              menu.appendChild(item);
            }
          },75);
        }
      </script>
    </body></html>"#;
    let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
    cdp.call(
        "Page.setDocumentContent",
        json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
    )
    .unwrap();
    let mut tab = GeminiTab::attach(
        port,
        target,
        ChatSessionMode::Continue,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(15),
        false,
    )
    .unwrap();
    let catalog = tab.model_catalog().unwrap();
    assert_eq!(
        catalog.models.len(),
        2,
        "inline levels must not become models"
    );
    assert_eq!(catalog.model_thinking["original"].len(), 2);
    assert_eq!(catalog.model_thinking["future-hydrogen"].len(), 4);
    assert_eq!(
        eval(
            &mut cdp,
            "document.querySelector('[data-mode-id=original]').getAttribute('aria-checked')"
        ),
        "true"
    );
    tab.select_model("future-hydrogen").unwrap();
    tab.select_thinking("Ultra").unwrap();
    assert_eq!(eval(&mut cdp, "selectedLevel"), "Ultra");
    let clicks = eval(&mut cdp, "thinkingClicks");
    assert_eq!(action(&mut cdp, "thinking_open")["available"], true);
    assert_eq!(
        eval(&mut cdp, "thinkingClicks"),
        clicks,
        "reopening must not collapse an expanded inline menu"
    );
    assert_eq!(action(&mut cdp, "models").as_array().unwrap().len(), 2);
    eval(&mut cdp,"document.querySelectorAll('[data-mode-id]').forEach(e=>e.setAttribute('aria-checked','false'))");
    assert!(tab
        .model_catalog()
        .unwrap_err()
        .contains("selected model is unknown"));
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no Google requests"]
fn model_menu_reopens_after_escape_finishes_removing_the_previous_menu() {
    use anticrumb::{
        agent::CancelToken,
        gemini::{ChatSessionMode, GeminiTab},
    };
    use std::time::{Duration, Instant};
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or("9222".into())
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let html = r#"<!doctype html><html><body>
      <rich-textarea><div contenteditable="true"></div></rich-textarea>
      <button data-test-id="bard-mode-menu-button" onclick="openMenu()">Menu</button>
      <script>
        let selectedModel='original';
        let menuOpens=0;
        let delayedClosures=0;
        function openMenu() {
          menuOpens++;
          const menu=document.createElement('gem-menu');
          menu.setAttribute('data-test-id','gem-mode-menu');
          menu.style.cssText='display:block;min-height:20px';
          for(const id of ['original','future-model']) {
            const item=document.createElement('gem-menu-item');
            item.setAttribute('data-mode-id',id);
            item.setAttribute('aria-checked',String(selectedModel===id));
            item.innerHTML='<span class="label">'+id+'</span>';
            item.onclick=()=>{
              selectedModel=id;
              menu.querySelectorAll('gem-menu-item').forEach(el=>el.setAttribute('aria-checked',String(el===item)));
            };
            menu.appendChild(item);
          }
          document.body.appendChild(menu);
        }
        document.addEventListener('keydown',event=>{
          if(event.key!=='Escape')return;
          const menu=document.querySelector('gem-menu[data-test-id="gem-mode-menu"]');
          if(!menu)return;
          // The visible shell remains during closing, but usable rows are gone.
          menu.replaceChildren();
          setTimeout(()=>{menu.remove();delayedClosures++;},80);
        });
        openMenu();
      </script>
    </body></html>"#;
    let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
    cdp.call(
        "Page.setDocumentContent",
        json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
    )
    .unwrap();
    let mut tab = GeminiTab::attach(
        port,
        target,
        ChatSessionMode::Continue,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(15),
        false,
    )
    .unwrap();
    assert_eq!(tab.list_models_and_thinking().unwrap().0.len(), 2);
    tab.select_model("future-model").unwrap();
    let (models, thinking) = tab.list_models_and_thinking().unwrap();
    assert_eq!(models.len(), 2);
    assert!(thinking.is_empty());
    assert!(models
        .iter()
        .any(|model| model.mode_id == "future-model" && model.selected));
    assert!(eval(&mut cdp, "delayedClosures").as_u64().unwrap() >= 2);
    assert_eq!(
        eval(&mut cdp, "menuOpens"),
        3,
        "one reopen per dismissed menu"
    );
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no Google requests"]
fn attach_files_and_thinking_overlay_tolerate_async_mount_and_shadow_dom() {
    use anticrumb::{
        agent::CancelToken,
        gemini::{ChatSessionMode, GeminiTab},
    };
    use std::time::{Duration, Instant};
    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or("9222".into())
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let html = r#"<!doctype html><html><body>
      <rich-textarea><div contenteditable="true"></div></rich-textarea>
      <button data-test-id="bard-mode-menu-button">Menu</button>
      <gem-menu data-test-id="gem-mode-menu">
        <gem-menu-item data-mode-id="pro" aria-checked="true"><span class="label">Pro</span></gem-menu-item>
        <gem-menu-item value="thinking_level" aria-expanded="false" onclick="openThinkingOverlay()">Thinking</gem-menu-item>
      </gem-menu>
      <div class="cdk-overlay-container"></div>
      <button data-test-id="upload-button" aria-expanded="false" onclick="mountShadowInput()">+</button>
      <div id="shadow-host"></div>
      <div id="chips"></div>
      <script>
        function openThinkingOverlay() {
          const trigger = document.querySelector('[value="thinking_level"]');
          trigger.setAttribute('aria-expanded', 'true');
          setTimeout(() => {
            const container = document.querySelector('.cdk-overlay-container');
            const menu = document.createElement('div');
            menu.setAttribute('role', 'menu');
            menu.style.cssText = 'display:block;min-height:20px';
            menu.innerHTML = '<gem-menu-item value="high" aria-checked="true"><span class="label">High</span></gem-menu-item>';
            container.replaceChildren(menu);
          }, 150);
        }
        function mountShadowInput() {
          const btn = document.querySelector('[data-test-id="upload-button"]');
          if (btn.getAttribute('aria-expanded') === 'true') {
            btn.setAttribute('aria-expanded', 'false');
            return;
          }
          btn.setAttribute('aria-expanded', 'true');
          setTimeout(() => {
            const host = document.getElementById('shadow-host');
            const shadow = host.shadowRoot || host.attachShadow({mode: 'open'});
            const input = document.createElement('input');
            input.type = 'file';
            input.onchange = () => {
              const name = input.files?.[0]?.name || '';
              document.getElementById('chips').innerHTML =
                `<div class="file-preview" title="${name}"><span>${name}</span></div>`;
            };
            shadow.replaceChildren(input);
          }, 250);
        }
      </script>
    </body></html>"#;
    let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
    cdp.call(
        "Page.setDocumentContent",
        json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let sample = dir.path().join("sample.txt");
    std::fs::write(&sample, "hello attachment").unwrap();

    let mut tab = GeminiTab::attach(
        port,
        target,
        ChatSessionMode::Continue,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(15),
        false,
    )
    .unwrap();
    let (models, thinking) = tab.list_models_and_thinking().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(thinking.len(), 1);
    assert_eq!(thinking[0].level, "high");
    tab.attach_files(
        std::slice::from_ref(&sample),
        None::<&mut fn(&anticrumb::gemini::AttachmentProgress)>,
    )
    .unwrap();
    // Simulate a stale aria-expanded="true" menu with no mounted input; attach_files resets & reopens it.
    eval(
        &mut cdp,
        "document.getElementById('shadow-host').shadowRoot.replaceChildren(); document.getElementById('chips').innerHTML=''; document.querySelector('[data-test-id=upload-button]').setAttribute('aria-expanded','true')",
    );
    tab.attach_files(
        &[sample],
        None::<&mut fn(&anticrumb::gemini::AttachmentProgress)>,
    )
    .unwrap();
}

#[test]
#[ignore = "requires a local Chrome CDP endpoint; fixture only, no Google requests"]
fn reused_tab_starts_new_chat_and_temp_chat_in_place() {
    use anticrumb::agent::CancelToken;
    use anticrumb::gemini::{ChatSessionMode, GeminiTab};
    use std::time::{Duration, Instant};

    let port = std::env::var("ANTICRUMB_TEST_PORT")
        .unwrap_or("9222".into())
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
    let mut cdp =
        CdpClient::connect(port, &browser::ws_path_from_url(&target.ws_url).unwrap()).unwrap();
    let html = r#"<!doctype html><html><body>
      <gem-nav-list-item data-test-id="new-chat-button">
        <a href="/app" onclick="event.preventDefault(); startNewChat()">New chat</a>
      </gem-nav-list-item>
      <temp-chat-button>
        <button data-test-id="temp-chat-button" aria-pressed="false" onclick="toggleTemp()">Temp</button>
      </temp-chat-button>
      <button data-test-id="bard-mode-menu-button">Fast</button>
      <rich-textarea><div contenteditable="true">stale draft</div></rich-textarea>
      <div id="responses"><model-response>previous turn</model-response></div>
      <script>
        window.__newChatClicks = 0;
        function startNewChat() {
          window.__newChatClicks += 1;
          setTimeout(() => {
            document.getElementById('responses').innerHTML = '';
            const tc = document.querySelector('[data-test-id="temp-chat-button"]');
            tc.setAttribute('aria-pressed', 'false');
          }, 120);
        }
        function toggleTemp() {
          const tc = document.querySelector('[data-test-id="temp-chat-button"]');
          tc.setAttribute('aria-pressed', tc.getAttribute('aria-pressed') === 'true' ? 'false' : 'true');
        }
      </script>
    </body></html>"#;
    let tree = cdp.call("Page.getFrameTree", json!({})).unwrap();
    cdp.call(
        "Page.setDocumentContent",
        json!({"frameId":tree["frameTree"]["frame"]["id"],"html":html}),
    )
    .unwrap();

    // 1. Reusing dirty tab in NewChat mode clicks new-chat-button in-place and clears old responses.
    let _tab = GeminiTab::attach_reused(
        port,
        target.clone(),
        ChatSessionMode::NewChat,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(10),
        false,
    )
    .unwrap();
    assert_eq!(eval(&mut cdp, "window.__newChatClicks"), json!(1));
    assert_eq!(action(&mut cdp, "probe")["response_count"], json!(0));
    assert_eq!(action(&mut cdp, "probe")["temporary"], json!(false));

    // 2. Reusing again with dirty responses in TempChat resets chat first, then enables temporary mode.
    eval(
        &mut cdp,
        "document.getElementById('responses').innerHTML = '<model-response>turn 2</model-response>'",
    );
    let _temp_tab = GeminiTab::attach_reused(
        port,
        target,
        ChatSessionMode::TempChat,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(10),
        false,
    )
    .unwrap();
    assert_eq!(eval(&mut cdp, "window.__newChatClicks"), json!(2));
    assert_eq!(action(&mut cdp, "probe")["response_count"], json!(0));
    assert_eq!(action(&mut cdp, "probe")["temporary"], json!(true));
}

#[test]
#[ignore = "live Chrome E2E test against Wikipedia and Google"]
fn live_browser_control_searches_wikipedia_and_probes_google() {
    let temp = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("ANTICRUMB_HOME");
    std::env::set_var("ANTICRUMB_HOME", temp.path());

    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    struct BrowserGuard(u16, Option<std::ffi::OsString>);
    impl Drop for BrowserGuard {
        fn drop(&mut self) {
            let _ = browser::close_browser_port(self.0);
            if let Some(prev) = &self.1 {
                std::env::set_var("ANTICRUMB_HOME", prev);
            } else {
                std::env::remove_var("ANTICRUMB_HOME");
            }
        }
    }
    let _guard = BrowserGuard(port, prev_home);

    let account = anticrumb::domain::Account {
        id: "live-test".into(),
        provider: "gemini-web".into(),
        profile: temp.path().join("accounts/live-test/profile"),
        subprofile: None,
        port,
        headless: true,
        state: "ready".into(),
    };
    let registry = anticrumb::browser_control::BrowserControlRegistry::default();
    let call = |args: Value| {
        let mut args = args;
        args["timeout_ms"] = json!(60000);
        let request = anticrumb::browser_control::request(&args, &account.id, "live-test").unwrap();
        registry.execute(&account, &request, anticrumb::agent::CancelToken::new())
    };
    let opened =
        call(json!({"action":"open","url":"https://en.wikipedia.org/wiki/Main_Page"})).unwrap();
    let tab = opened.active_tab_id;
    assert!(opened.page.unwrap().url.contains("wikipedia.org"));
    call(json!({"action":"fill","target":{"css":"input[name=search]"},"text":"Rust (programming language)"})).unwrap();
    let searched = call(json!({"action":"press","key":"Enter"}))
        .unwrap()
        .page
        .unwrap();
    assert!(searched.url.contains("Rust") || searched.title.contains("Rust"));
    let scrolled = call(json!({"action":"scroll","y":600}))
        .unwrap()
        .page
        .unwrap();
    assert!(!scrolled.text.is_empty());
    let link = scrolled
        .elements
        .iter()
        .find(|el| {
            let Some(path) = el.href.split("/wiki/").nth(1) else {
                return false;
            };
            el.tag == "a"
                && !path.is_empty()
                && !path.contains(':')
                && !path.contains("Main_Page")
                && !path.contains("Rust_(programming_language)")
        })
        .expect("navigable wiki link");
    let clicked = call(json!({"action":"click","target":{"ref":link.ref_id}}))
        .unwrap()
        .page
        .unwrap();
    assert_ne!(clicked.url, searched.url);
    let listed = call(json!({"action":"tabs"})).unwrap();
    assert_eq!(listed.tabs.len(), 1);
    assert_eq!(listed.active_tab_id, tab);
    let closed = call(json!({"action":"close"})).unwrap();
    assert!(closed.tabs.is_empty());
    if let Ok(google) = call(json!({"action":"open","url":"https://www.google.com/"})) {
        println!(
            "[GOOGLE PAGE] {}",
            anticrumb::browser_control::format_response(&google)
        );
        if call(json!({"action":"fill","target":{"css":"textarea[name=q], input[name=q]"},"text":"anticrumb rust"})).is_ok() {
            println!("[GOOGLE SEARCH] {:?}", call(json!({"action":"press","key":"Enter"})));
        }
    }
    call(json!({"action":"reset"})).unwrap();
}
