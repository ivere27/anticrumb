use crate::{domain::*, service::Service, wire};
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event as SseEvent, KeepAlive},
        IntoResponse, Response, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone)]
pub struct HttpState {
    pub service: Arc<Service>,
    pub token: String,
    bodies: Arc<tokio::sync::Semaphore>,
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let status = match self.code {
            ErrorCode::InvalidRequest | ErrorCode::UnsupportedCapability => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::QueueFull | ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::AuthRequired | ErrorCode::VerificationRequired => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ErrorCode::Cancelled => StatusCode::CONFLICT,
            ErrorCode::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        };
        (status, Json(json!({"error":self}))).into_response()
    }
}

async fn authenticate(
    State(state): State<HttpState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if request.headers().contains_key("origin") {
        return (StatusCode::FORBIDDEN, "browser origins are not enabled").into_response();
    }
    let provided = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| {
            request
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
        })
        .or_else(|| {
            request
                .headers()
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
        })
        .unwrap_or("");
    let is_valid = crate::rpc::constant_time_eq(provided, &state.token);

    let ctx = request
        .extensions()
        .get::<crate::rpc::server::ConnectionContext>();
    if let Some(ctx) = ctx {
        if is_valid {
            if !ctx
                .logged_auth_success
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let peer_str = ctx
                    .peer
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "remote".into());
                let path = crate::rpc::server::sanitize_header_value(request.uri().path(), 128);
                let method =
                    crate::rpc::server::sanitize_header_value(request.method().as_str(), 16);
                let ua = request
                    .headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| crate::rpc::server::sanitize_header_value(s, 128))
                    .unwrap_or_else(|| "unknown".into());
                eprintln!(
                    "{} client authenticated: {} [API: {} {}, {}] (auth: verified)",
                    crate::rpc::server::log_timestamp(),
                    peer_str,
                    method,
                    path,
                    ua
                );
            }
        } else {
            let fails = ctx
                .auth_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if fails <= 6 {
                let peer_str = ctx
                    .peer
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "remote".into());
                let path = crate::rpc::server::sanitize_header_value(request.uri().path(), 128);
                let method =
                    crate::rpc::server::sanitize_header_value(request.method().as_str(), 16);
                let ua = request
                    .headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| crate::rpc::server::sanitize_header_value(s, 128))
                    .unwrap_or_else(|| "unknown".into());
                if fails <= 5 {
                    eprintln!(
                        "{} client auth failed: {} [API: {} {}, {}] (auth: invalid token)",
                        crate::rpc::server::log_timestamp(),
                        peer_str,
                        method,
                        path,
                        ua
                    );
                } else {
                    eprintln!(
                        "{} client auth failed: {} [API: {} {}, {}] (further failed attempts on this connection suppressed)",
                        crate::rpc::server::log_timestamp(),
                        peer_str,
                        method,
                        path,
                        ua
                    );
                }
            }
        }
    }

    if !is_valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {
                "message": "invalid daemon token",
                "code": "invalid_daemon_token",
                "stage": "daemon_auth",
                "hint": "Use the token printed by `anticrumb server`, or its --token value, in the provider client's API key. Check the server address. Signing in to Chrome does not change this token."
            }})),
        )
            .into_response();
    }
    // Acquire before JSON/base64 extraction, so simultaneous large bodies stay bounded.
    let _body = if request.method() == axum::http::Method::POST
        && request.uri().path().starts_with("/v1")
    {
        match state.bodies.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return ServiceError::new(
                    ErrorCode::QueueFull,
                    "http",
                    "too many concurrent request bodies",
                )
                .into_response()
            }
        }
    } else {
        None
    };
    next.run(request).await
}

pub fn router(service: Arc<Service>, token: String) -> Result<Router> {
    let state = HttpState {
        service,
        token,
        bodies: Arc::new(tokio::sync::Semaphore::new(4)),
    };
    Ok(Router::new()
        .route(
            "/health",
            get(|| async { Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")})) }),
        )
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1beta/models/{action}", post(gemini))
        .layer(DefaultBodyLimit::max(48 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state))
}

async fn models(State(s): State<HttpState>) -> Result<Json<Value>> {
    let catalog = s.service.store.model_catalog("default")?;
    let provider = catalog["provider"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            s.service
                .store
                .get::<Account>("local", "account", "default")
                .ok()
                .map(|a| a.provider)
                .filter(|p| !p.is_empty())
        })
        .unwrap_or_else(default_provider);
    let default_model = format!("{provider}-default");
    let mut data = vec![json!({"id": default_model, "object": "model", "owned_by": provider})];
    if let Some(models) = catalog["models"].as_array() {
        for m in models {
            if let Some(name) = m["mode_id"].as_str().or_else(|| m["name"].as_str()) {
                data.push(json!({"id": name, "object": "model", "owned_by": provider}));
            }
        }
    }
    Ok(Json(json!({"object":"list","data":data})))
}
async fn chat(
    State(s): State<HttpState>,
    headers: HeaderMap,
    Json(mut v): Json<Value>,
) -> Result<Response> {
    account_header(&headers, &mut v)?;
    let input = v.clone();
    let request = s
        .service
        .store
        .blocking(move |store| wire::chat_request(&input, &store))
        .await?;
    complete(s, headers, v, request, wire::WireFormat::Chat).await
}
async fn responses(
    State(s): State<HttpState>,
    headers: HeaderMap,
    Json(mut v): Json<Value>,
) -> Result<Response> {
    account_header(&headers, &mut v)?;
    let input = v.clone();
    let request = s
        .service
        .store
        .blocking(move |store| wire::responses_request(&input, &store))
        .await?;
    complete(s, headers, v, request, wire::WireFormat::Responses).await
}
async fn messages(
    State(s): State<HttpState>,
    headers: HeaderMap,
    Json(mut v): Json<Value>,
) -> Result<Response> {
    account_header(&headers, &mut v)?;
    let input = v.clone();
    let request = s
        .service
        .store
        .blocking(move |store| wire::messages_request(&input, &store))
        .await?;
    complete(s, headers, v, request, wire::WireFormat::Messages).await
}
async fn gemini(
    State(s): State<HttpState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    Json(mut v): Json<Value>,
) -> Result<Response> {
    let (model, method) = action
        .rsplit_once(':')
        .ok_or_else(|| ServiceError::invalid("expected model:generateContent"))?;
    if !["generateContent", "streamGenerateContent"].contains(&method) {
        return Err(ServiceError::invalid("unsupported method"));
    }
    account_header(&headers, &mut v)?;
    v["stream"] = json!(method == "streamGenerateContent");
    let input = v.clone();
    let model = model.to_owned();
    let request = s
        .service
        .store
        .blocking(move |store| wire::gemini_request(&input, &model, &store))
        .await?;
    complete(s, headers, v, request, wire::WireFormat::Gemini).await
}
fn result(wire: wire::WireFormat, id: &str, result: &TurnResult, input_estimate: usize) -> Value {
    match wire {
        wire::WireFormat::Chat => crate::protocol::openai_result(id, result),
        wire::WireFormat::Responses => wire::responses_result(id, result),
        wire::WireFormat::Messages => wire::messages_result(id, result, input_estimate),
        wire::WireFormat::Gemini => wire::gemini_result(result),
    }
}

async fn complete(
    s: HttpState,
    headers: HeaderMap,
    v: Value,
    mut request: TurnRequest,
    wire: wire::WireFormat,
) -> Result<Response> {
    request.idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(id) = headers
        .get("x-anticrumb-session")
        .and_then(|v| v.to_str().ok())
    {
        request.session_id = Some(id.into());
    }
    request.reuse_scope = headers
        .get("x-anticrumb-reuse-scope")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let input_estimate = serde_json::to_vec(&request)
        .map_err(ServiceError::storage)?
        .len()
        .div_ceil(3);
    let has_tools = !request.tools.is_empty();
    let id = s.service.submit_async(request).await?;
    if v["stream"] != true {
        let run = s.service.wait(&id).await?;
        let outcome = run.result.ok_or_else(|| {
            run.error.unwrap_or_else(|| {
                ServiceError::new(ErrorCode::Internal, "result", "missing result")
            })
        })?;
        let mut response = Json(result(wire, &id, &outcome, input_estimate)).into_response();
        if wire == wire::WireFormat::Messages {
            response.headers_mut().insert(
                "x-anticrumb-usage",
                axum::http::HeaderValue::from_static("estimated-bytes-divided-by-3"),
            );
        }
        return Ok(response);
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<SseEvent, Infallible>>(32);
    tokio::spawn(async move {
        let mut sse = wire::SseStream::new(wire, id.clone());
        let mut after = 0i64;
        let mut sub = s.service.subscribe_run(&id);
        if !send_frames(&tx, sse.prelude()).await {
            return;
        }
        loop {
            if tx.is_closed() {
                let _ = s.service.cancel(&id);
                return;
            }
            sub.mark_seen();
            loop {
                let (events, has_more) = match s.service.next_event_page(&id, &mut after).await {
                    Ok(page) => page,
                    Err(e) => {
                        let _ = send_frames(&tx, vec![sse.fail(&e)]).await;
                        return;
                    }
                };
                for event in events {
                    if event.kind == "text_delta" {
                        let text = event.data["text"].as_str().unwrap_or("");
                        if !send_frames(&tx, sse.push_text_delta(text, has_tools)).await {
                            let _ = s.service.cancel(&id);
                            return;
                        }
                    }
                }
                if !has_more {
                    break;
                }
            }
            let Ok(run) = s.service.snapshot_async(&id).await else {
                return;
            };
            if let Some(error) = run.error {
                let _ = send_frames(&tx, vec![sse.fail(&error)]).await;
                return;
            }
            if let Some(result) = run.result {
                let _ = send_frames(&tx, sse.finish(&result, input_estimate)).await;
                return;
            }
            tokio::select! {
                _ = tx.closed() => {
                    let _ = s.service.cancel(&id);
                    return;
                }
                _ = sub.wait() => {}
            }
        }
    });
    let mut response = Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response();
    if wire == wire::WireFormat::Messages {
        response.headers_mut().insert(
            "x-anticrumb-usage",
            axum::http::HeaderValue::from_static("estimated-bytes-divided-by-3"),
        );
    }
    Ok(response)
}
type Sender = tokio::sync::mpsc::Sender<std::result::Result<SseEvent, Infallible>>;
async fn send_frames(tx: &Sender, frames: Vec<wire::SseFrame>) -> bool {
    for frame in frames {
        let mut output = SseEvent::default().data(frame.data);
        if let Some(name) = frame.event {
            output = output.event(name);
        }
        if tx.send(Ok(output)).await.is_err() {
            return false;
        }
    }
    true
}

async fn count_tokens(Json(v): Json<Value>) -> Response {
    (
        [(
            axum::http::HeaderName::from_static("x-anticrumb-usage"),
            axum::http::HeaderValue::from_static("estimated-bytes-divided-by-3"),
        )],
        Json(json!({"input_tokens":v.to_string().len().div_ceil(3)})),
    )
        .into_response()
}

fn account_header(headers: &HeaderMap, v: &mut Value) -> Result<()> {
    if let Some(id) = headers.get("x-anticrumb-account") {
        let id = id
            .to_str()
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ServiceError::invalid("invalid account header"));
        }
        v["account_id"] = json!(id);
    }
    Ok(())
}
