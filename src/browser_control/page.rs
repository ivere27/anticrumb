use super::target_json;
use crate::{
    agent::CancelToken,
    browser_host::{self as browser, TargetInfo},
    cdp::CdpClient,
    rpc::pb::{self, browser_control_request::Command},
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const DOM: &str = include_str!("dom.js");
pub(super) struct ControlledTab {
    port: u16,
    target: TargetInfo,
    cdp: Option<CdpClient>,
    visual: super::visual::VisualMode,
}
impl Drop for ControlledTab {
    fn drop(&mut self) {
        self.cdp = None;
        let _ = browser::close_tab(self.port, &self.target.id);
    }
}
impl ControlledTab {
    pub fn new(port: u16, target: TargetInfo) -> Self {
        Self {
            port,
            target,
            cdp: None,
            visual: Default::default(),
        }
    }
    pub fn target_id(&self) -> &str {
        &self.target.id
    }
    pub fn title(&self) -> &str {
        &self.target.title
    }
    pub fn url(&self) -> &str {
        &self.target.url
    }
    fn connect(
        &mut self,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<&mut CdpClient, String> {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err("browser deadline exceeded".into());
        }
        if self.cdp.as_ref().is_none_or(CdpClient::is_invalid) {
            let mut cdp =
                CdpClient::connect(self.port, &browser::ws_path_from_url(&self.target.ws_url)?)?;
            cdp.set_control(cancel.clone(), deadline);
            cdp.call("Runtime.enable", json!({}))?;
            cdp.call("Page.enable", json!({}))?;
            cdp.call(
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled":true}),
            )?;
            if !crate::web::allow_private_env() {
                cdp.restrict_to_public_network()?;
            }
            self.cdp = Some(cdp);
        }
        let cdp = self.cdp.as_mut().unwrap();
        cdp.set_control(cancel.clone(), deadline);
        Ok(cdp)
    }
    pub fn navigate(
        &mut self,
        url: &str,
        max_chars: u32,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<pb::BrowserSnapshot, String> {
        self.execute(
            &Command::Navigate(pb::BrowserNavigate { url: url.into() }),
            max_chars,
            0,
            cancel,
            deadline,
        )
    }
    pub fn execute(
        &mut self,
        command: &Command,
        max_chars: u32,
        wait_ms: u32,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<pb::BrowserSnapshot, String> {
        let mut visual = std::mem::take(&mut self.visual);
        let result = (|| {
            let cdp = self.connect(cancel, deadline)?;
            if !matches!(
                command,
                Command::Snapshot(_)
                    | Command::UseTab(_)
                    | Command::Screenshot(_)
                    | Command::ClickPoint(_)
                    | Command::Wheel(_)
            ) {
                visual.invalidate();
            }
            match command {
                Command::Snapshot(_) | Command::UseTab(_) | Command::Screenshot(_) => {}
                Command::ClickPoint(point) => {
                    let (x, y) = visual.point(cdp, point)?;
                    visual.invalidate();
                    click_at(cdp, x, y)?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Wheel(p) => {
                    let (x, y) =
                        visual.point(cdp, p.point.as_ref().ok_or("missing wheel point")?)?;
                    visual.invalidate();
                    cdp.call(
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseMoved","x":x,"y":y}),
                    )?;
                    cdp.call("Input.dispatchMouseEvent",json!({"type":"mouseWheel","x":x,"y":y,"deltaX":p.delta_x,"deltaY":p.delta_y}))?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Navigate(p) => {
                    let url =
                        crate::web::validate_public_url(&p.url, crate::web::allow_private_env())?;
                    // Invalidate references even when navigating back to the same document.
                    eval(cdp, "window.__anticrumbElements = new Map(); return true;")?;
                    let result = cdp.call("Page.navigate", json!({"url":url.as_str()}))?;
                    if let Some(error) = result["errorText"].as_str().filter(|s| !s.is_empty()) {
                        if error == "net::ERR_BLOCKED_BY_CLIENT" {
                            if let Some(reason) = result["frameId"]
                                .as_str()
                                .and_then(|frame| cdp.blocked_navigation_reason(frame))
                            {
                                return Err(format!(
                                    "navigation failed: {error}; browser network policy: {reason}"
                                ));
                            }
                        }
                        return Err(format!("navigation failed: {error}"));
                    }
                    settle(cdp, cancel, deadline)?;
                }
                Command::Back(_) | Command::Forward(_) => {
                    let history = cdp.call("Page.getNavigationHistory", json!({}))?;
                    let index = history["currentIndex"]
                        .as_i64()
                        .ok_or("missing navigation history")?
                        + if matches!(command, Command::Back(_)) {
                            -1
                        } else {
                            1
                        };
                    let entry = usize::try_from(index)
                        .ok()
                        .and_then(|i| history["entries"].get(i))
                        .ok_or("no page in that history direction")?;
                    crate::web::validate_public_url(
                        entry["url"].as_str().ok_or("missing history URL")?,
                        crate::web::allow_private_env(),
                    )?;
                    eval(cdp, "window.__anticrumbElements = new Map(); return true;")?;
                    cdp.call(
                        "Page.navigateToHistoryEntry",
                        json!({"entryId":entry["id"]}),
                    )?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Reload(_) => {
                    eval(cdp, "window.__anticrumbElements = new Map(); return true;")?;
                    cdp.call("Page.reload", json!({}))?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Click(target) => {
                    let point = dom_eval(
                        cdp,
                        &format!(
                            r#"
                        const el=resolve({}); actionable(el);
                        const r=el.getBoundingClientRect();
                        const x=Math.max(0,Math.min(innerWidth-1,r.left+r.width/2)), y=Math.max(0,Math.min(innerHeight-1,r.top+r.height/2));
                        const hit=el.getRootNode().elementFromPoint(x,y);
                        if (!hit || !(hit===el || el.contains(hit))) throw new Error('target is covered by another element');
                        return {{x,y}};
                    "#,
                            target_json(target)
                        ),
                    )?;
                    click_at(
                        cdp,
                        point["x"].as_f64().ok_or("missing click x")?,
                        point["y"].as_f64().ok_or("missing click y")?,
                    )?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Fill(p) | Command::Type(p) => {
                    let clear = matches!(command, Command::Fill(_));
                    let element = if let Some(target) = &p.target {
                        format!(
                            "let el=resolve({}); actionable(el); el.focus();",
                            target_json(target)
                        )
                    } else {
                        "let el=document.activeElement; while(el?.shadowRoot?.activeElement) el=el.shadowRoot.activeElement; if(el?.tagName==='IFRAME') return true;".into()
                    };
                    dom_eval(
                        cdp,
                        &format!(
                            r#"
                        {element}
                        if (!el) throw new Error('no focused editable field');
                        const textInput=el.tagName==='INPUT' && ['text','search','email','url','tel','password','number'].includes(el.type);
                        if (!(textInput || el.tagName==='TEXTAREA' || el.isContentEditable)) throw new Error('target is not an editable text field');
                        if (el.readOnly || el.getAttribute('aria-readonly')==='true') throw new Error('target is read-only');
                        if ({clear}) {{
                          if (el.isContentEditable) {{
                            const range=document.createRange();range.selectNodeContents(el);
                            const selection=getSelection();selection.removeAllRanges();selection.addRange(range);
                          }} else el.select();
                        }}
                        return true;
                    "#
                        ),
                    )?;
                    // Empty insertText does not delete a selection in all Chrome versions.
                    if clear && p.text.is_empty() {
                        key(cdp, "Backspace", &[])?;
                    } else if !p.text.is_empty() {
                        cdp.call("Input.insertText", json!({"text":p.text}))?;
                    }
                    settle(cdp, cancel, deadline)?;
                }
                Command::Press(p) => {
                    if let Some(target) = &p.target {
                        dom_eval(
                            cdp,
                            &format!(
                                "const el=resolve({}); actionable(el); el.focus(); return true;",
                                target_json(target)
                            ),
                        )?;
                    }
                    key(cdp, &p.key, &p.modifiers)?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Select(p) => {
                    dom_eval(
                        cdp,
                        &format!(
                            r#"
                        const el=resolve({}); actionable(el);
                        if (el.tagName!=='SELECT' || el.multiple) throw new Error('target must be a single-value select');
                        const options=[...el.options].filter(o=>o.value==={});
                        if (options.length!==1) throw new Error('select value must match exactly one option');
                        const option=options[0];
                        if (option.disabled || option.parentElement.disabled) throw new Error('option is disabled');
                        el.value=option.value;
                        el.dispatchEvent(new Event('input',{{bubbles:true}}));
                        el.dispatchEvent(new Event('change',{{bubbles:true}})); return true;
                    "#,
                            target_json(p.target.as_ref().ok_or("missing target")?),
                            json!(p.value)
                        ),
                    )?;
                    settle(cdp, cancel, deadline)?;
                }
                Command::Scroll(p) => {
                    let target = p
                        .target
                        .as_ref()
                        .map(|t| format!("resolve({})", target_json(t)))
                        .unwrap_or_else(|| "window".into());
                    dom_eval(cdp,&format!("const el={target}; el.scrollBy({{left:{},top:{},behavior:'instant'}}); return true;",p.x,p.y))?;
                }
                Command::Wait(p) => {
                    let target = target_json(p.target.as_ref().ok_or("missing target")?);
                    loop {
                        cancel.check()?;
                        if Instant::now() >= deadline {
                            return Err("timed out waiting for target state".into());
                        }
                        let result = dom_eval(
                            cdp,
                            &format!(
                                "const el=resolve({target},true); return {}visible(el);",
                                if p.state == 1 { "!" } else { "" }
                            ),
                        );
                        match result {
                            Ok(v) if v == true => break,
                            Ok(_) => {}
                            Err(e) if context_changed(&e) => {}
                            Err(e) => return Err(e),
                        }
                        if cancel.sleep_or_cancelled(Duration::from_millis(80)) {
                            return Err("cancelled".into());
                        }
                    }
                }
                _ => return Err("command must be handled by the browser session registry".into()),
            }
            let mut observe = || -> Result<pb::BrowserSnapshot, String> {
                if wait_ms > 0 {
                    let delay = Duration::from_millis(wait_ms.into());
                    if Instant::now() + delay >= deadline {
                        return Err("deadline exceeded before observation delay".into());
                    }
                    if cancel.sleep_or_cancelled(delay) {
                        return Err("cancelled".into());
                    }
                }
                let mut page = snapshot(cdp, max_chars, matches!(command, Command::Screenshot(_)))?;
                if visual.enabled || matches!(command, Command::Screenshot(_)) {
                    let region = if let Command::Screenshot(p) = command {
                        p.region.as_ref()
                    } else {
                        None
                    };
                    page.screenshot = Some(visual.capture(cdp, region)?);
                }
                Ok(page)
            };
            let page=observe().map_err(|e| {
                if matches!(command,Command::Snapshot(_)|Command::UseTab(_)|Command::Wait(_)|Command::Screenshot(_)) {e}
                else {format!("action was dispatched, but its result could not be observed: {e}; use snapshot before deciding whether to retry")}
            })?;
            Ok(page)
        })();
        self.visual = visual;
        if let Ok(page) = &result {
            self.target.url = page.url.clone();
            self.target.title = page.title.clone();
        }
        if let Err(error) = &result {
            if error.contains("CDP") || error.contains("WebSocket") {
                self.cdp = None;
            }
        }
        result
    }
}
pub(super) fn eval(cdp: &mut CdpClient, body: &str) -> Result<Value, String> {
    let result=cdp.call("Runtime.evaluate",json!({"expression":format!("(() => {{ {body} }})()"),"returnByValue":true,"awaitPromise":false}))?;
    if let Some(e) = result.get("exceptionDetails") {
        return Err(e["exception"]["description"]
            .as_str()
            .or_else(|| e["text"].as_str())
            .unwrap_or("browser script failed")
            .lines()
            .next()
            .unwrap_or("browser script failed")
            .into());
    }
    result["result"]
        .get("value")
        .cloned()
        .ok_or_else(|| "browser script returned no value".into())
}
fn dom_eval(cdp: &mut CdpClient, body: &str) -> Result<Value, String> {
    eval(cdp, &format!("{DOM}\n{body}"))
}
fn context_changed(error: &str) -> bool {
    error.contains("Execution context was destroyed") || error.contains("Cannot find context")
}
fn settle(cdp: &mut CdpClient, cancel: &CancelToken, deadline: Instant) -> Result<(), String> {
    if cancel.sleep_or_cancelled(
        Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
    ) {
        return Err("cancelled".into());
    }
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err("deadline exceeded waiting for page readiness; action may already have completed, use snapshot before retrying".into());
        }
        match eval(cdp, "return document.readyState !== 'loading';") {
            Ok(v) if v == true => return Ok(()),
            Ok(_) => {}
            Err(e) if context_changed(&e) => {}
            Err(e) => return Err(e),
        }
        if cancel.sleep_or_cancelled(Duration::from_millis(80)) {
            return Err("cancelled".into());
        }
    }
}
fn snapshot(
    cdp: &mut CdpClient,
    max_chars: u32,
    allow_error_page: bool,
) -> Result<pb::BrowserSnapshot, String> {
    let prefix = format!("e{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let value = dom_eval(
        cdp,
        &format!(
            r#"
        const candidates=all(interactive).filter(visible);
        const inView=el=>{{const r=el.getBoundingClientRect();return r.bottom>0 && r.right>0 && r.top<innerHeight && r.left<innerWidth;}};
        const picked=candidates.filter(inView).concat(candidates.filter(el=>!inView(el))).slice(0,80);
        window.__anticrumbElements=new Map();
        const elements=picked.map((el,i)=>{{
            const ref_id={prefix}+'-'+(i+1);
            window.__anticrumbElements.set(ref_id,{{el,signature:signature(el)}});
            return {{ref_id,tag:el.tagName.toLowerCase(),role:role(el),label:name(el).slice(0,160),
                value:el.type==='password' ? '[redacted]' : String(el.value || '').slice(0,160),
                href:el.tagName==='A' ? el.href.slice(0,1000) : '', disabled:disabled(el),
                checked:el.matches('input[type=checkbox],input[type=radio]') ? el.checked : (el.hasAttribute('aria-checked') ? el.getAttribute('aria-checked')==='true' : null),
                options:el.tagName==='SELECT' ? [...el.options].slice(0,80).map(o=>({{value:o.value.slice(0,200),label:normalize(o.label).slice(0,160),selected:o.selected}})) : []}};
        }});
        const text=Array.from(document.body?.innerText || '');
        return {{url:location.href,title:document.title.slice(0,500),text:text.slice(0,{max_chars}).join(''),elements,text_truncated:text.length>{max_chars},elements_truncated:candidates.length>80}};
    "#,
            prefix = json!(prefix)
        ),
    )?;
    let page: pb::BrowserSnapshot =
        serde_json::from_value(value).map_err(|e| format!("invalid snapshot: {e}"))?;
    // A failed navigation still leaves useful pixels to inspect. This does not
    // permit navigating to internal pages or interacting with them.
    if allow_error_page
        && matches!(
            page.url.as_str(),
            "about:blank" | "chrome-error://chromewebdata/"
        )
    {
        return Ok(page);
    }
    if page.url.starts_with("chrome-error://") {
        return Err("navigation failed to load the page".into());
    }
    crate::web::validate_public_url(&page.url, crate::web::allow_private_env())
        .map_err(|e| format!("blocked browser redirect: {e}"))?;
    Ok(page)
}
fn click_at(cdp: &mut CdpClient, x: f64, y: f64) -> Result<(), String> {
    for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
        let mut params = json!({"type":kind,"x":x,"y":y});
        if kind != "mouseMoved" {
            params["button"] = json!("left");
            params["clickCount"] = json!(1);
        }
        cdp.call("Input.dispatchMouseEvent", params)?;
    }
    Ok(())
}

fn key(cdp: &mut CdpClient, key: &str, modifiers: &[String]) -> Result<(), String> {
    let (name, code, vk, text) = match key {
        "Enter" => ("Enter", "Enter", 13, "\r"),
        "Tab" => ("Tab", "Tab", 9, ""),
        "Escape" => ("Escape", "Escape", 27, ""),
        "Backspace" => ("Backspace", "Backspace", 8, ""),
        "Delete" => ("Delete", "Delete", 46, ""),
        "Home" => ("Home", "Home", 36, ""),
        "End" => ("End", "End", 35, ""),
        "PageUp" => ("PageUp", "PageUp", 33, ""),
        "PageDown" => ("PageDown", "PageDown", 34, ""),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, ""),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, ""),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, ""),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, ""),
        "Space" | " " => (" ", "Space", 32, " "),
        s if s.chars().count() == 1 => (s, "", 0, s),
        _ => return Err("unsupported key".into()),
    };
    let mask = modifiers.iter().fold(0, |mask, m| {
        mask | match m.as_str() {
            "Alt" => 1,
            "Control" => 2,
            "Meta" => 4,
            "Shift" => 8,
            _ => 0,
        }
    });
    let (code, vk) = if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        let ch = key.as_bytes()[0].to_ascii_uppercase();
        (
            format!(
                "{}{}",
                if ch.is_ascii_alphabetic() {
                    "Key"
                } else {
                    "Digit"
                },
                char::from(ch)
            ),
            i32::from(ch),
        )
    } else {
        (code.to_owned(), vk)
    };
    let mut down = json!({"type":"keyDown","key":name,"code":code,"windowsVirtualKeyCode":vk,"modifiers":mask});
    if !text.is_empty() && mask & 7 == 0 {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    cdp.call("Input.dispatchKeyEvent", down)?;
    cdp.call(
        "Input.dispatchKeyEvent",
        json!({"type":"keyUp","key":name,"code":code,"windowsVirtualKeyCode":vk,"modifiers":mask}),
    )?;
    Ok(())
}
