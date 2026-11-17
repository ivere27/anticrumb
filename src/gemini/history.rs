//! Read-only enumeration of the sidebar's complete pagination chain.
//! A stable DOM row count never proves completion. The observed UI request
//! cursor must lead from the initial page to an explicit null response cursor.
use super::url::{account_scope, is_gemini_origin, rpc_account_scope};
use crate::{
    agent::CancelToken,
    browser_host as browser,
    cdp::CdpClient,
    domain::{Account, ErrorCode, Result, ServiceError},
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::sync::broadcast::error::TryRecvError;

fn failure(message: impl Into<String>) -> ServiceError {
    ServiceError::backend(message.into(), false)
}

fn transient_failure(message: impl Into<String>) -> ServiceError {
    ServiceError::new(
        ErrorCode::TransportDisconnected,
        "web_history_transport",
        message,
    )
}

fn cdp_failure(message: String) -> ServiceError {
    let lower = message.to_ascii_lowercase();
    if [
        "cancelled",
        "schema",
        "account",
        "origin",
        "auth",
        "sign in",
        "login",
        "invalid parameter",
        "method not found",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        return failure(message);
    }
    if [
        "websocket",
        "disconnected",
        "connection reset",
        "connection refused",
        "connection aborted",
        "timed out",
        "deadline exceeded",
        "receiving on a closed channel",
        "sending on a closed channel",
        "no resource with given identifier found",
        "execution context was destroyed",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        transient_failure(message)
    } else {
        failure(message)
    }
}

fn loading_failure(params: &Value) -> ServiceError {
    // CDP errorText is normally net::ERR_*. Keep just that diagnostic code;
    // never pass through arbitrary network payloads, URLs, or account text.
    let code = params["errorText"]
        .as_str()
        .filter(|text| {
            text.starts_with("net::ERR_")
                && text.len() <= 100
                && text[9..]
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or("unreported");
    transient_failure(format!(
        "history pagination failed to load (network={code}, canceled={}); completeness is unknown",
        params["canceled"].as_bool().unwrap_or(false)
    ))
}

fn check_page_access(value: &Value) -> Result<()> {
    match value["state"].as_str() {
        Some("verification_required") => Err(ServiceError::new(
            ErrorCode::VerificationRequired,
            "web_history",
            "complete Google's human verification in Chrome, then retry reading web history",
        )),
        Some("auth_required") => Err(ServiceError::new(
            ErrorCode::AuthRequired,
            "web_history",
            "sign in before reading web history",
        )),
        _ => Ok(()),
    }
}

/// Only repeats read-only observation, never a deletion action. A failed
/// attempt contributes no partial IDs to the next independent snapshot.
fn enumerate_with_retry(
    cancel: &CancelToken,
    mut attempt: impl FnMut(Instant) -> Result<Vec<String>>,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(120);
    for index in 0..2 {
        cancel.check().map_err(failure)?;
        let attempt_deadline = deadline.min(Instant::now() + Duration::from_secs(60));
        let result = attempt(attempt_deadline);
        cancel.check().map_err(failure)?;
        match result {
            Ok(urls) => return Ok(urls),
            Err(mut error) => {
                let retryable = error.stage == "web_history_transport"
                    && error.code == ErrorCode::TransportDisconnected;
                if !retryable || index == 1 || Instant::now() >= deadline {
                    if retryable && index == 1 {
                        error.message.push_str(" (read-only enumeration failed after 2 attempts; completeness remains unknown)");
                    }
                    return Err(error);
                }
                cancel.sleep_or_cancelled(Duration::from_millis(200));
            }
        }
    }
    unreachable!("the second read-only attempt always returns")
}

#[derive(Clone, Debug)]
struct Page {
    cursor: Option<String>,
    next: Option<String>,
    ids: Vec<String>,
}

#[derive(Default)]
struct Pagination {
    started: bool,
    complete: bool,
    next: Option<String>,
    seen_cursors: HashSet<String>,
    ids: Vec<String>,
    seen_ids: HashSet<String>,
    waiting: Vec<Page>,
}

impl Pagination {
    fn push(&mut self, page: Page) -> std::result::Result<(), String> {
        if self.complete {
            return Ok(());
        }
        if self.waiting.len() >= 1000 {
            return Err("history pagination did not form a complete cursor chain".into());
        }
        self.waiting.push(page);
        while let Some(index) = self.waiting.iter().position(|p| p.cursor == self.next) {
            let page = self.waiting.remove(index);
            self.started = true;
            for id in page.ids {
                if self.seen_ids.insert(id.clone()) {
                    self.ids.push(id);
                }
            }
            if self.ids.len() > 100_000 {
                return Err("web history exceeds the safe enumeration limit".into());
            }
            match page.next {
                None => {
                    self.complete = true;
                    break;
                }
                Some(cursor) => {
                    if !self.seen_cursors.insert(cursor.clone()) {
                        return Err(
                            "web history pagination repeated a cursor; completeness is unknown"
                                .into(),
                        );
                    }
                    if self.seen_cursors.len() > 1000 {
                        return Err("web history exceeds the pagination limit".into());
                    }
                    self.next = Some(cursor);
                }
            }
        }
        Ok(())
    }
}

fn request_cursor(post: &str) -> std::result::Result<Option<String>, String> {
    let raw = form_urlencoded::parse(post.as_bytes())
        .find(|(k, _)| k == "f.req")
        .map(|(_, v)| v.into_owned())
        .ok_or("history request metadata is missing")?;
    let batches: Value =
        serde_json::from_str(&raw).map_err(|_| "history request metadata changed")?;
    let requests: Vec<_> = batches
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|batch| batch.as_array().into_iter().flatten())
        .filter(|request| request[0] == "MaZiqc")
        .collect();
    if requests.len() != 1 {
        return Err("history request is ambiguous".into());
    }
    let args: Value = serde_json::from_str(
        requests[0][1]
            .as_str()
            .ok_or("history request arguments changed")?,
    )
    .map_err(|_| "history request arguments changed")?;
    // This is the observed All conversations request. Fail closed if a
    // different filter or protocol is observed; collect pinned DOM entries too.
    let flags = json!([null, null, 1, null, null, null, null, null, null, 1]);
    if args.as_array().map(Vec::len) != Some(3)
        || !args[0].as_u64().is_some_and(|n| n > 0 && n <= 1000)
        || args[2] != flags
    {
        return Err(
            "unsupported history filter or pagination request; cannot verify all conversations"
                .into(),
        );
    }
    cursor(&args[1])
}

/// Kept only in the maintenance page's memory. Replay just the observed
/// read-only history RPC, never other operations from a batched UI request.
#[derive(Clone, Serialize, Deserialize)]
struct HistoryRequest {
    url: String,
    post: String,
}

impl HistoryRequest {
    fn page(&self, next: Option<&str>, scope: &str) -> Result<Self> {
        let mut url = crate::url_lite::Url::parse(&self.url)
            .map_err(|_| failure("invalid history request URL"))?;
        if rpc_account_scope(&url).as_deref() != Some(scope)
            || !url.path().ends_with("/batchexecute")
            || !url
                .query_pairs()
                .any(|(key, value)| key == "rpcids" && value.split(',').any(|rpc| rpc == "MaZiqc"))
        {
            return Err(failure(
                "history request changed origin, account, or endpoint",
            ));
        }
        request_cursor(&self.post).map_err(failure)?;
        let mut form: Vec<(String, String)> = form_urlencoded::parse(self.post.as_bytes())
            .into_owned()
            .collect();
        if form.iter().filter(|(key, _)| key == "f.req").count() != 1 {
            return Err(failure("history request is ambiguous"));
        }
        for (key, value) in &mut form {
            if key == "f.req" {
                let batches: Value = serde_json::from_str(value)
                    .map_err(|_| failure("history request metadata changed"))?;
                let mut request = batches
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|batch| batch.as_array().into_iter().flatten())
                    .find(|request| request[0] == "MaZiqc")
                    .cloned()
                    .ok_or_else(|| failure("history request missing"))?;
                let mut args: Value = serde_json::from_str(request[1].as_str().unwrap())
                    .map_err(|_| failure("history request arguments changed"))?;
                args[1] = json!(next);
                request[1] = json!(args.to_string());
                *value = json!([[request]]).to_string();
            }
        }
        let query: Vec<(String, String)> = url
            .query_pairs()
            .into_owned()
            .map(|(key, value)| {
                if key == "rpcids" {
                    (key, "MaZiqc".into())
                } else {
                    (key, value)
                }
            })
            .collect();
        url.set_query(None);
        for (key, value) in query {
            url.query_pairs_mut().append_pair(&key, &value);
        }
        let post = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        Ok(Self {
            url: url.to_string(),
            post,
        })
    }
}

fn cached_request(cdp: &mut CdpClient, scope: &str) -> Result<Option<HistoryRequest>> {
    let result = cdp.call("Runtime.evaluate", json!({
        "expression": "({url: location.href, request: window.__anticrumbHistoryRequest || null})",
        "returnByValue": true
    })).map_err(cdp_failure)?;
    let value = &result["result"]["value"];
    if value["request"].is_null() {
        return Ok(None);
    }
    value["url"]
        .as_str()
        .and_then(|raw| crate::url_lite::Url::parse(raw).ok())
        .filter(|url| is_gemini_origin(url) && account_scope(url).as_deref() == Some(scope))
        .ok_or_else(|| failure("history tab switched origin or Google accounts"))?;
    let request: HistoryRequest = serde_json::from_value(value["request"].clone())
        .map_err(|_| failure("cached history request metadata changed"))?;
    request.page(None, scope).map(Some)
}

/// Bootstrap once after opening/reloading a maintenance tab. Later queued
/// deletions keep both the document and its observed read-only request.
pub(super) fn prepare_web_history_in_target(
    port: u16,
    target: &browser::TargetInfo,
    cancel: CancelToken,
    start_url: &str,
) -> Result<()> {
    let scope = validate_start_url(start_url)?;
    let mut cdp = CdpClient::connect(
        port,
        &browser::ws_path_from_url(&target.ws_url).map_err(failure)?,
    )
    .map_err(cdp_failure)?;
    cdp.set_control(cancel.clone(), Instant::now() + Duration::from_secs(15));
    let ready = cached_request(&mut cdp, &scope)?.is_some();
    drop(cdp);
    if !ready {
        list_web_conversations_in_target(port, target, cancel, start_url)?;
    }
    Ok(())
}

fn history_urls(
    ids: &[String],
    base: &crate::url_lite::Url,
    visible: Vec<String>,
) -> Result<Vec<String>> {
    let prefix = base.path().split("/app").next().unwrap_or("");
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        let mut url = base.clone();
        url.set_path(&format!("{prefix}/app/{id}"));
        url.set_fragment(None);
        let authuser = base
            .query_pairs()
            .find(|(key, _)| key == "authuser")
            .map(|(_, value)| value.into_owned());
        url.set_query(None);
        if let Some(authuser) = authuser {
            url.query_pairs_mut().append_pair("authuser", &authuser);
        }
        seen.insert(id.clone());
        urls.push(url.to_string());
    }
    // Pinned conversations may be outside the All conversations filter.
    for raw in visible {
        let url = super::url::parse_gemini_chat_url(&raw)
            .map_err(|_| failure("invalid conversation URL in sidebar"))?;
        if account_scope(&url) != account_scope(base) {
            return Err(failure("conversation sidebar contains a different account"));
        }
        let id = url
            .path_segments()
            .and_then(|mut parts| parts.next_back())
            .unwrap();
        if seen.insert(id.to_owned()) {
            urls.push(raw);
        }
    }
    Ok(urls)
}

fn list_from_current_page(
    cdp: &mut CdpClient,
    request: HistoryRequest,
    scope: &str,
    cancel: &CancelToken,
    deadline: Instant,
) -> Result<Vec<String>> {
    let script = crate::adapter::script("read-history.js").map_err(ServiceError::storage)?;
    let sidebar = crate::adapter::script("list-conversations.js").map_err(ServiceError::storage)?;
    let mut pagination = Pagination::default();
    loop {
        cancel.check().map_err(failure)?;
        if Instant::now() >= deadline {
            return Err(transient_failure(
                "web history enumeration timed out; completeness is unknown",
            ));
        }
        let page = request.page(pagination.next.as_deref(), scope)?;
        let expression = format!(
            "({script})({})",
            json!({
                "url":page.url, "post":page.post, "scope":scope,
                "timeoutMs":deadline.saturating_duration_since(Instant::now()).as_millis().min(15000)
            })
        );
        let result = cdp
            .call(
                "Runtime.evaluate",
                json!({
                    "expression":expression,"returnByValue":true,"awaitPromise":true
                }),
            )
            .map_err(cdp_failure)?;
        if result.get("exceptionDetails").is_some() {
            return Err(failure("history read adapter failed"));
        }
        let value = &result["result"]["value"];
        check_page_access(value)?;
        if value["state"] == "transport_error" {
            return Err(transient_failure(
                "history request failed; completeness is unknown",
            ));
        }
        let response_scope = value["url"]
            .as_str()
            .and_then(|raw| crate::url_lite::Url::parse(raw).ok())
            .and_then(|url| rpc_account_scope(&url));
        if value["state"] != "ready" || response_scope.as_deref() != Some(scope) {
            return Err(failure(
                "history read changed origin, account, or response schema",
            ));
        }
        let body = value["body"]
            .as_str()
            .ok_or_else(|| failure("history response body missing"))?;
        pagination
            .push(parse_page(body, pagination.next.clone()).map_err(failure)?)
            .map_err(failure)?;
        if pagination.complete {
            let result = cdp
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression":format!("({sidebar})({{scroll:false}})"),"returnByValue":true
                    }),
                )
                .map_err(cdp_failure)?;
            let value = &result["result"]["value"];
            check_page_access(value)?;
            let base = value["url"]
                .as_str()
                .and_then(|raw| crate::url_lite::Url::parse(raw).ok())
                .filter(|url| is_gemini_origin(url) && account_scope(url).as_deref() == Some(scope))
                .ok_or_else(|| failure("history page switched origin or Google accounts"))?;
            let visible = value["urls"]
                .as_array()
                .ok_or_else(|| failure("history sidebar adapter failed"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| failure("invalid conversation URL in sidebar"))
                })
                .collect::<Result<Vec<_>>>()?;
            return history_urls(&pagination.ids, &base, visible);
        }
        cancel.sleep_or_cancelled(Duration::from_millis(150));
    }
}

fn cursor(value: &Value) -> std::result::Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    match value.as_str() {
        Some(s) if !s.is_empty() && s.len() <= 16_384 => Ok(Some(s.into())),
        _ => Err("history pagination cursor schema changed".into()),
    }
}

fn parse_page(body: &str, requested: Option<String>) -> std::result::Result<Page, String> {
    if body.len() > 8 * 1024 * 1024 {
        return Err("history response exceeds the safe size limit".into());
    }
    let mut payload = None;
    for line in body.lines().filter(|line| line.starts_with("[[")) {
        let envelope: Value =
            serde_json::from_str(line).map_err(|_| "invalid history response framing")?;
        for row in envelope
            .as_array()
            .ok_or("history response framing changed")?
        {
            if row[0] == "wrb.fr" && row[1] == "MaZiqc" {
                if payload.is_some() {
                    return Err("ambiguous history response".into());
                }
                payload = Some(
                    serde_json::from_str::<Value>(
                        row[2].as_str().ok_or("history response payload missing")?,
                    )
                    .map_err(|_| "invalid history response payload")?,
                );
            }
        }
    }
    let payload = payload.ok_or("history response payload missing")?;
    // The UI's RPC omits every default field for an empty terminal page. This
    // occurs when a full page advertises a cursor but that cursor has no more
    // records, and also represents an entirely empty history. Require a valid
    // matched MaZiqc envelope: missing payloads and JSON null are not emptiness.
    if payload.as_array().is_some_and(Vec::is_empty) {
        return Ok(Page {
            cursor: requested,
            next: None,
            ids: Vec::new(),
        });
    }
    if payload.as_array().map(Vec::len) != Some(3) || !payload[0].is_null() {
        return Err("history response schema changed; cannot establish completeness".into());
    }
    let next = cursor(&payload[1])?;
    let empty = Vec::new();
    let rows = if payload[2].is_null() && next.is_none() {
        &empty
    } else {
        payload[2]
            .as_array()
            .ok_or("history records schema changed")?
    };
    let mut ids = Vec::new();
    for row in rows {
        let id = row[0]
            .as_str()
            .and_then(|id| id.strip_prefix("c_"))
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            .ok_or("history conversation identifier schema changed")?;
        ids.push(id.to_owned());
    }
    Ok(Page {
        cursor: requested,
        next,
        ids,
    })
}

/// Collect every conversation from an independently opened account tab. Caller
/// must hold the account gate. This function never invokes a deletion control.
pub fn list_web_conversations(account: &Account, cancel: CancelToken) -> Result<Vec<String>> {
    list_web_conversations_at(account, cancel, browser::GEMINI_APP_URL)
}

pub(super) fn list_web_conversations_at(
    account: &Account,
    cancel: CancelToken,
    start_url: &str,
) -> Result<Vec<String>> {
    validate_start_url(start_url)?;
    cancel.check().map_err(failure)?;
    let host = browser::ensure_browser(&browser::LaunchOptions::headless_account(
        account, start_url,
    ))
    .map_err(failure)?;
    let reusable = browser::list_targets(host.port).ok().and_then(|targets| {
        targets.into_iter().find(|t| {
            t.target_type == "page"
                && !super::url_has_chat_id(&t.url)
                && (super::is_reusable_gemini_url(&t.url)
                    || t.url.starts_with("https://gemini.google.com/usage"))
        })
    });
    let tab = super::SystemGeminiTab::acquire_persistent(host.port, reusable, "about:blank")
        .map_err(failure)?;
    list_web_conversations_in_target(host.port, &tab.target, cancel, start_url)
}

fn validate_start_url(start_url: &str) -> Result<String> {
    let start = crate::url_lite::Url::parse(start_url)
        .map_err(|_| failure("invalid history account URL"))?;
    let initial_scope =
        account_scope(&start).ok_or_else(|| failure("unsupported history account URL"))?;
    if !is_gemini_origin(&start) || !start.path().ends_with("/app") {
        return Err(failure(
            "history enumeration requires an exact Gemini account home URL",
        ));
    }
    Ok(initial_scope)
}

pub(super) fn list_web_conversations_in_target(
    host_port: u16,
    target: &browser::TargetInfo,
    cancel: CancelToken,
    start_url: &str,
) -> Result<Vec<String>> {
    enumerate_with_retry(&cancel, |deadline| {
        list_web_conversations_once_in_target(
            host_port,
            target,
            cancel.clone(),
            start_url,
            deadline,
        )
    })
}

fn list_web_conversations_once_in_target(
    host_port: u16,
    target: &browser::TargetInfo,
    cancel: CancelToken,
    start_url: &str,
    deadline: Instant,
) -> Result<Vec<String>> {
    let initial_scope = validate_start_url(start_url)?;
    cancel.check().map_err(failure)?;
    let mut cdp = CdpClient::connect(
        host_port,
        &browser::ws_path_from_url(&target.ws_url).map_err(failure)?,
    )
    .map_err(cdp_failure)?;
    cdp.set_control(cancel.clone(), deadline);
    if let Some(request) = cached_request(&mut cdp, &initial_scope)? {
        return list_from_current_page(&mut cdp, request, &initial_scope, &cancel, deadline);
    }
    let _ = cdp.call("Runtime.enable", json!({}));
    let mut waiting_for_nav = super::system_tab::mark_pending_nav(&mut cdp);
    let mut events = cdp.subscribe();
    cdp.call("Network.enable", json!({"maxPostDataSize":1048576}))
        .map_err(cdp_failure)?;
    cdp.call("Page.navigate", json!({"url":start_url}))
        .map_err(cdp_failure)?;
    let script = crate::adapter::script("list-conversations.js").map_err(ServiceError::storage)?;
    let mut pending = HashMap::<String, Option<String>>::new();
    let mut observed_request = None::<HistoryRequest>;
    let mut network_scopes = HashSet::<String>::new();
    let mut pagination = Pagination::default();
    let mut page_url = None::<crate::url_lite::Url>;
    let mut visible_urls = Vec::<crate::url_lite::Url>::new();
    let mut visible_seen = HashSet::<String>::new();
    let mut last_scroll = Instant::now();
    loop {
        cancel.check().map_err(failure)?;
        if Instant::now() >= deadline {
            return Err(transient_failure(format!("web history enumeration timed out after observing {} conversations; completeness was not established", pagination.ids.len())));
        }
        loop {
            let event =
                match events.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => break,
                    Err(_) => return Err(transient_failure(
                        "web history network observation was interrupted; completeness is unknown",
                    )),
                };
            let params = &event["params"];
            let Some(id) = params["requestId"].as_str() else {
                continue;
            };
            match event["method"].as_str() {
                Some("Network.requestWillBeSent") => {
                    let request = &params["request"];
                    let Some(url) = request["url"]
                        .as_str()
                        .and_then(|raw| crate::url_lite::Url::parse(raw).ok())
                    else {
                        continue;
                    };
                    if url.host_str() != Some("gemini.google.com")
                        || !url.query_pairs().any(|(key, value)| {
                            key == "rpcids" && value.split(',').any(|rpc| rpc == "MaZiqc")
                        })
                    {
                        continue;
                    }
                    if !url.path().ends_with("/batchexecute") {
                        return Err(failure(
                            "history request endpoint changed; completeness is unknown",
                        ));
                    }
                    let scope = rpc_account_scope(&url)
                        .ok_or_else(|| failure("invalid history request origin or account"))?;
                    network_scopes.insert(scope.clone());
                    if pagination.complete {
                        continue;
                    }
                    let post = request["postData"]
                        .as_str()
                        .ok_or_else(|| failure("history request metadata was not captured"))?;
                    let requested = request_cursor(post).map_err(failure)?;
                    if requested.is_none() {
                        observed_request = Some(
                            HistoryRequest {
                                url: url.to_string(),
                                post: post.to_owned(),
                            }
                            .page(None, &scope)?,
                        );
                    }
                    pending.insert(id.to_owned(), requested);
                }
                Some("Network.responseReceived") if pending.contains_key(id) => {
                    let status = params["response"]["status"].as_u64();
                    if matches!(status, Some(401 | 403)) {
                        return Err(ServiceError::new(
                            ErrorCode::AuthRequired,
                            "web_history",
                            format!(
                                "history pagination requires account authentication (HTTP {})",
                                status.unwrap()
                            ),
                        ));
                    }
                    if matches!(status, Some(408 | 500 | 502 | 503 | 504)) {
                        return Err(transient_failure(format!("history pagination request temporarily failed (HTTP {}); complete history could not be read", status.unwrap())));
                    }
                    if status != Some(200) {
                        return Err(failure(format!("history pagination request failed (HTTP {}); complete history could not be read", status.map(|value| value.to_string()).unwrap_or_else(|| "unknown".into()))));
                    }
                    let response_url = params["response"]["url"]
                        .as_str()
                        .and_then(|raw| crate::url_lite::Url::parse(raw).ok())
                        .and_then(|url| rpc_account_scope(&url))
                        .ok_or_else(|| failure("history response changed origin or account"))?;
                    if !network_scopes.contains(&response_url) {
                        return Err(failure("history response switched Google accounts"));
                    }
                }
                Some("Network.loadingFailed") if pending.contains_key(id) => {
                    if params.get("blockedReason").is_some()
                        || params.get("corsErrorStatus").is_some()
                    {
                        return Err(failure("history pagination was blocked by browser policy; completeness is unknown"));
                    }
                    return Err(loading_failure(params));
                }
                Some("Network.loadingFinished") => {
                    if let Some(requested) = pending.remove(id) {
                        let response = cdp
                            .call("Network.getResponseBody", json!({"requestId":id}))
                            .map_err(cdp_failure)?;
                        let body = response["body"]
                            .as_str()
                            .ok_or_else(|| failure("history response body was not captured"))?;
                        let decoded;
                        let body = if response["base64Encoded"] == true {
                            decoded = String::from_utf8(
                                base64::engine::general_purpose::STANDARD
                                    .decode(body)
                                    .map_err(|_| failure("invalid encoded history response"))?,
                            )
                            .map_err(|_| failure("invalid history response text"))?;
                            &decoded
                        } else {
                            body
                        };
                        pagination
                            .push(parse_page(body, requested).map_err(failure)?)
                            .map_err(failure)?;
                    }
                }
                _ => {}
            }
        }
        if waiting_for_nav {
            match super::system_tab::poll_pending_nav(&mut cdp) {
                Ok(true) => {
                    cancel.sleep_or_cancelled(Duration::from_millis(50));
                    continue;
                }
                Ok(false) => waiting_for_nav = false,
                Err(e) => return Err(cdp_failure(e)),
            }
        }
        let scroll = pagination.started
            && !pagination.complete
            && pending.is_empty()
            && last_scroll.elapsed() >= Duration::from_millis(700);
        if scroll {
            last_scroll = Instant::now();
        }
        let expression = format!("({script})({})", json!({"scroll":scroll}));
        let result = cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true}),
            )
            .map_err(cdp_failure)?;
        if result.get("exceptionDetails").is_some() {
            return Err(failure("history sidebar adapter failed"));
        }
        let value = &result["result"]["value"];
        check_page_access(value)?;
        if let Some(raw) = value["url"].as_str() {
            let current = crate::url_lite::Url::parse(raw)
                .map_err(|_| failure("invalid history page URL"))?;
            let scope = account_scope(&current)
                .ok_or_else(|| failure("unsupported history account URL"))?;
            if scope != initial_scope {
                return Err(failure(
                    "history page switched from the requested Google account",
                ));
            }
            if network_scopes
                .iter()
                .any(|request_scope| request_scope != &scope)
            {
                return Err(failure(
                    "history network request belongs to a different Google account",
                ));
            }
            if current.scheme() != "https" || current.host_str() != Some("gemini.google.com") {
                return Err(failure("history page left the expected origin"));
            }
            if page_url
                .as_ref()
                .is_some_and(|first| account_scope(first).as_deref() != Some(&scope))
            {
                return Err(failure("history tab switched Google accounts"));
            }
            page_url = Some(current);
        }
        for raw in value["urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let url = crate::url_lite::Url::parse(raw)
                .map_err(|_| failure("invalid conversation URL in sidebar"))?;
            if url.scheme() != "https"
                || url.host_str() != Some("gemini.google.com")
                || page_url
                    .as_ref()
                    .is_none_or(|base| account_scope(&url) != account_scope(base))
            {
                return Err(failure(
                    "conversation sidebar contains a different account or origin",
                ));
            }
            if visible_seen.insert(url.to_string()) {
                visible_urls.push(url);
            }
        }
        if pagination.complete {
            if let Some(base) = page_url.as_ref() {
                let urls = history_urls(
                    &pagination.ids,
                    base,
                    visible_urls
                        .into_iter()
                        .map(|url| url.to_string())
                        .collect(),
                )?;
                let request = observed_request
                    .ok_or_else(|| failure("initial history request was not captured"))?;
                cdp.call("Runtime.evaluate", json!({
                    "expression":format!("window.__anticrumbHistoryRequest = {}; true", serde_json::to_string(&request).map_err(ServiceError::storage)?),
                    "returnByValue":true
                })).map_err(cdp_failure)?;
                return Ok(urls);
            }
        }
        cancel.sleep_or_cancelled(Duration::from_millis(super::jitter_u64(90, 140)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_observation_retries_transient_failure_once_with_fresh_deadline() {
        let cancel = CancelToken::new();
        let mut calls = 0;
        let started = Instant::now();
        let urls = enumerate_with_retry(&cancel, |deadline| {
            calls += 1;
            assert!(deadline <= Instant::now() + Duration::from_secs(60));
            assert!(deadline <= started + Duration::from_secs(120));
            if calls == 1 {
                Err(loading_failure(
                    &json!({"errorText":"net::ERR_ABORTED","canceled":true}),
                ))
            } else {
                Ok(vec!["synthetic-complete-snapshot".into()])
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(urls, ["synthetic-complete-snapshot"]);
        calls = 0;
        let error = enumerate_with_retry(&cancel, |_| {
            calls += 1;
            Err(cdp_failure("CDP WebSocket disconnected".into()))
        })
        .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.message.contains("2 attempts"));
    }

    #[test]
    fn schema_account_auth_and_cancellation_never_trigger_observation_retry() {
        for error in [
            failure("history response schema changed"),
            failure("history page switched Google accounts"),
            ServiceError::new(
                ErrorCode::AuthRequired,
                "web_history",
                "sign in before reading history",
            ),
            cdp_failure("CDP WebSocket authentication failed".into()),
            failure("cancelled"),
        ] {
            let mut calls = 0;
            let result = enumerate_with_retry(&CancelToken::new(), |_| {
                calls += 1;
                Err(error.clone())
            });
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
        let cancel = CancelToken::new();
        let mut calls = 0;
        let error = enumerate_with_retry(&cancel, |_| {
            calls += 1;
            cancel.cancel();
            Err(transient_failure("CDP disconnected during cancellation"))
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.code, ErrorCode::Cancelled);
        enumerate_with_retry(&cancel, |_| -> Result<Vec<String>> {
            panic!("cancelled observation must not open another tab")
        })
        .unwrap_err();
    }

    #[test]
    fn failed_loading_reports_only_safe_network_diagnostics() {
        let error = loading_failure(&json!({"errorText":"net::ERR_ABORTED","canceled":true}));
        assert!(error
            .message
            .contains("network=net::ERR_ABORTED, canceled=true"));
        let error = loading_failure(
            &json!({"errorText":"https://private.example/secret?token=private","canceled":false}),
        );
        assert!(!error.message.contains("private"));
        assert!(error.message.contains("network=unreported, canceled=false"));
    }

    #[test]
    fn human_verification_stops_read_only_retry_and_never_means_empty_history() {
        let mut attempts = 0;
        let error = enumerate_with_retry(&CancelToken::new(), |_| {
            attempts += 1;
            check_page_access(&json!({"state":"verification_required", "urls":[]}))?;
            Ok(Vec::new())
        })
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error.code, ErrorCode::VerificationRequired);
        assert_eq!(error.retry, "after_human");
    }

    fn body(ids: &[&str], next: Value) -> String {
        let records: Vec<_> = ids
            .iter()
            .map(|id| json!([format!("c_{id}"), "fixture title"]))
            .collect();
        let payload = json!([null, next, records]).to_string();
        format!(
            ")]}}'\n\n100\n{}\n",
            json!([["wrb.fr", "MaZiqc", payload, null, null, null, "generic"]])
        )
    }

    #[test]
    fn explicit_cursor_chain_includes_older_pages_even_when_rows_repeat() {
        let mut pages = Pagination::default();
        pages
            .push(parse_page(&body(&["recent"], json!("older")), None).unwrap())
            .unwrap();
        assert!(!pages.complete);
        // A virtualized list can show the same count or even duplicate records.
        pages
            .push(parse_page(&body(&["recent"], json!("oldest")), Some("older".into())).unwrap())
            .unwrap();
        assert!(!pages.complete);
        pages
            .push(parse_page(&body(&["old"], Value::Null), Some("oldest".into())).unwrap())
            .unwrap();
        assert!(pages.complete);
        assert_eq!(pages.ids, ["recent", "old"]);
    }

    #[test]
    fn out_of_order_responses_wait_for_the_initial_page() {
        let mut pages = Pagination::default();
        pages
            .push(parse_page(&body(&["old"], Value::Null), Some("cursor".into())).unwrap())
            .unwrap();
        assert!(!pages.started && !pages.complete);
        pages
            .push(parse_page(&body(&["new"], json!("cursor")), None).unwrap())
            .unwrap();
        assert!(pages.complete);
        assert_eq!(pages.ids, ["new", "old"]);
    }

    #[test]
    fn empty_first_page_is_complete_but_missing_pages_are_not() {
        let mut empty = Pagination::default();
        empty
            .push(parse_page(&body(&[], Value::Null), None).unwrap())
            .unwrap();
        assert!(empty.complete && empty.ids.is_empty());
        let mut missing = Pagination::default();
        missing
            .push(parse_page(&body(&["new"], json!("missing")), None).unwrap())
            .unwrap();
        missing
            .push(parse_page(&body(&["old"], Value::Null), Some("unrelated".into())).unwrap())
            .unwrap();
        assert!(!missing.complete);
    }

    #[test]
    fn compact_empty_rpc_payload_terminates_full_page_and_empty_history() {
        let empty_response =
            json!([["wrb.fr", "MaZiqc", "[]", null, null, null, "generic"]]).to_string();
        let mut empty = Pagination::default();
        empty
            .push(parse_page(&empty_response, None).unwrap())
            .unwrap();
        assert!(empty.complete && empty.ids.is_empty());

        let ids: Vec<_> = (0..28).map(|i| format!("fixture{i}")).collect();
        let names: Vec<_> = ids.iter().map(String::as_str).collect();
        let mut full = Pagination::default();
        full.push(parse_page(&body(&names, json!("terminal")), None).unwrap())
            .unwrap();
        assert!(!full.complete);
        full.push(parse_page(&empty_response, Some("terminal".into())).unwrap())
            .unwrap();
        assert!(full.complete);
        assert_eq!(full.ids, ids);

        // An empty page from a different cursor cannot fill a missing page.
        let mut missing = Pagination::default();
        missing
            .push(parse_page(&body(&["first"], json!("expected")), None).unwrap())
            .unwrap();
        missing
            .push(parse_page(&empty_response, Some("unrelated".into())).unwrap())
            .unwrap();
        assert!(!missing.complete);
    }

    #[test]
    fn cursor_cycles_fail_instead_of_claiming_complete() {
        let mut pages = Pagination::default();
        pages
            .push(parse_page(&body(&["new"], json!("repeat")), None).unwrap())
            .unwrap();
        assert!(pages
            .push(parse_page(&body(&["old"], json!("repeat")), Some("repeat".into())).unwrap())
            .is_err());
        assert!(!pages.complete);
    }

    #[test]
    fn malformed_or_ambiguous_responses_do_not_mean_empty_history() {
        for invalid in [
            "",
            "[]",
            r#"[["wrb.fr","MaZiqc","null"]]"#,
            r#"[["wrb.fr","MaZiqc","[null,null]"]]"#,
            r#"[["wrb.fr","MaZiqc","[null,\"\",[]]"]]"#,
            r#"[["wrb.fr","MaZiqc","[null,null,[[\"bad\"]]]"]]"#,
        ] {
            assert!(parse_page(invalid, None).is_err());
        }
        let valid = body(&[], Value::Null);
        assert!(parse_page(&format!("{valid}\n{valid}"), None).is_err());
    }

    #[test]
    fn only_supported_all_conversations_request_is_accepted() {
        let post = |args: Value| {
            form_urlencoded::Serializer::new(String::new())
                .append_pair(
                    "f.req",
                    &json!([[["MaZiqc", args.to_string(), null, "generic"]]]).to_string(),
                )
                .finish()
        };
        let flags = json!([null, null, 1, null, null, null, null, null, null, 1]);
        assert_eq!(
            request_cursor(&post(json!([28, null, flags]))).unwrap(),
            None
        );
        assert_eq!(
            request_cursor(&post(json!([20, "next", flags]))).unwrap(),
            Some("next".into())
        );
        assert!(request_cursor(&post(json!([20, "", flags]))).is_err());
        assert!(request_cursor(&post(json!([20, null, [1]]))).is_err());
        assert!(request_cursor("at=unrelated").is_err());
    }

    fn fixture_request() -> HistoryRequest {
        let args = json!([
            28,
            null,
            [null, null, 1, null, null, null, null, null, null, 1]
        ]);
        HistoryRequest {
            url: "https://gemini.google.com/_/BardChatUi/data/batchexecute?rpcids=MaZiqc,OtherOperation&authuser=0".into(),
            post: form_urlencoded::Serializer::new(String::new())
                .append_pair("at", "fixture-token")
                .append_pair("f.req", &json!([[
                    ["OtherOperation", "[]", null, "generic"],
                    ["MaZiqc", args.to_string(), null, "generic"]
                ]]).to_string()).finish(),
        }
    }

    #[test]
    fn replay_strips_other_operations_and_changes_only_history_cursor() {
        let request = fixture_request();
        let next = request.page(Some("older-page"), "0").unwrap();
        assert_eq!(
            request_cursor(&next.post).unwrap().as_deref(),
            Some("older-page")
        );
        assert!(!next.post.contains("OtherOperation"));
        assert!(!next.url.contains("OtherOperation"));
        assert!(next.post.contains("at=fixture-token"));
        assert_eq!(
            request_cursor(&next.page(None, "0").unwrap().post).unwrap(),
            None
        );
        assert!(request.page(None, "1").is_err());
        assert!(HistoryRequest {
            url: "https://example.com/batchexecute?rpcids=MaZiqc".into(),
            ..request.clone()
        }
        .page(None, "0")
        .is_err());
        assert!(HistoryRequest {
            post: format!("{}&f.req=ambiguous", request.post),
            ..request
        }
        .page(None, "0")
        .is_err());
    }

    #[test]
    fn warm_tab_reads_fresh_complete_pages_without_navigating_or_caching_results() {
        use crate::gemini::mock_cdp::MockCdpServer;
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let mut page_count = 0;
        let server = MockCdpServer::start_results(
            2,
            Duration::from_secs(2),
            move |request, conn| {
                assert_eq!(
                    request["method"], "Runtime.evaluate",
                    "warm cleanup must never navigate"
                );
                let expr = request["params"]["expression"].as_str().unwrap();
                if expr.contains("request: window.__anticrumbHistoryRequest") {
                    return json!({"result":{"value":{"url":browser::GEMINI_APP_URL,"request":fixture_request()}}});
                }
                if request["params"]["awaitPromise"] == true {
                    captured.lock().unwrap().push(expr.to_owned());
                    let body = if conn == 0 {
                        page_count += 1;
                        if page_count == 1 {
                            body(&["recent"], json!("next-fixture"))
                        } else {
                            body(&["older"], Value::Null)
                        }
                    } else {
                        body(&[], Value::Null)
                    };
                    return json!({"result":{"value":{"state":"ready","url":fixture_request().url,"body":body}}});
                }
                json!({"result":{"value":{"state":"ready","url":browser::GEMINI_APP_URL,"urls":[]}}})
            },
        );
        let target = server.gemini_target();
        let first = list_web_conversations_in_target(
            server.port,
            &target,
            CancelToken::new(),
            browser::GEMINI_APP_URL,
        )
        .unwrap();
        assert_eq!(
            first,
            [
                "https://gemini.google.com/app/recent",
                "https://gemini.google.com/app/older"
            ]
        );
        let after = list_web_conversations_in_target(
            server.port,
            &target,
            CancelToken::new(),
            browser::GEMINI_APP_URL,
        )
        .unwrap();
        assert!(
            after.is_empty(),
            "must read fresh server history after deletion"
        );
        server.join();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].contains("next-fixture"));
        assert!(!calls[0].contains("OtherOperation"));
    }

    #[test]
    fn warm_history_rejects_account_switch_before_any_request() {
        use crate::gemini::mock_cdp::MockCdpServer;
        let server = MockCdpServer::start_results(1, Duration::from_secs(2), |request, _| {
            assert_eq!(request["method"], "Runtime.evaluate");
            assert!(request["params"]["expression"]
                .as_str()
                .unwrap()
                .contains("request: window.__anticrumbHistoryRequest"));
            json!({"result":{"value":{"url":"https://gemini.google.com/u/1/app", "request":fixture_request()}}})
        });
        let error = list_web_conversations_in_target(
            server.port,
            &server.gemini_target(),
            CancelToken::new(),
            browser::GEMINI_APP_URL,
        )
        .unwrap_err();
        assert!(error.message.contains("switched"));
        server.join();
    }

    #[test]
    fn account_identity_includes_path_and_authuser_query() {
        let scope = |raw| account_scope(&crate::url_lite::Url::parse(raw).unwrap());
        assert_eq!(scope("https://gemini.google.com/app"), Some("0".into()));
        assert_eq!(
            scope("https://gemini.google.com/u/2/app?authuser=2"),
            Some("2".into())
        );
        assert_eq!(scope("https://gemini.google.com/u/2/app?authuser=1"), None);
        assert!(rpc_account_scope(
            &crate::url_lite::Url::parse("https://example.com/?authuser=0").unwrap()
        )
        .is_none());
        assert!(rpc_account_scope(
            &crate::url_lite::Url::parse("https://gemini.google.com:123/?authuser=0").unwrap()
        )
        .is_none());
        assert_eq!(
            rpc_account_scope(
                &crate::url_lite::Url::parse(
                    "https://gemini.google.com/u/1/_/batchexecute?authuser=1"
                )
                .unwrap()
            ),
            Some("1".into())
        );
    }

    #[test]
    #[ignore = "read-only live account enumeration; requires explicitly selected Chrome CDP"]
    fn live_history_enumeration_is_complete_and_read_only() {
        let port = std::env::var("ANTICRUMB_TEST_PORT")
            .unwrap_or_else(|_| "9222".into())
            .parse()
            .unwrap();
        // Never reuse a user's idle composer: it may contain an unsent draft.
        let target = browser::create_tab(port, "about:blank").expect("isolated history tab");
        let _close = browser::CloseTabGuard::new(port, target.id.clone());
        let urls = list_web_conversations_in_target(
            port,
            &target,
            CancelToken::new(),
            browser::GEMINI_APP_URL,
        )
        .unwrap();
        eprintln!(
            "complete read-only enumeration: {} conversations",
            urls.len()
        );
        if let Ok(absent) = std::env::var("ANTICRUMB_TEST_ABSENT_URL") {
            let absent = crate::url_lite::Url::parse(absent.trim()).unwrap();
            let found = urls
                .iter()
                .any(|raw| crate::url_lite::Url::parse(raw).unwrap().path() == absent.path());
            eprintln!("disposable deleted fixture absent: {}", !found);
            assert!(
                !found,
                "disposable fixture still exists in complete history"
            );
        }
    }
}
