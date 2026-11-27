//! Standards negotiation, framing, cancellation and notifications belong to rmcp.
use crate::{
    domain::{Result, ServiceError, SessionMode, SessionRetention, TurnRequest},
    rpc::{codec, pb},
};
use futures_util::StreamExt;
use rmcp::{model::*, service::RequestContext, ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Server {
    rpc: crate::rpc::client::RpcClient,
}
impl Server {
    pub async fn new(base: String, token: String) -> Result<Self> {
        Ok(Self {
            rpc: crate::rpc::client::RpcClient::network(&base, &token, None, None).await?,
        })
    }
    async fn invoke(
        &self,
        name: &str,
        args: Value,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<Value> {
        match name {
            "gemini_models" => {
                let account_id = match args["account_id"].as_str() {
                    Some(id) => {
                        validate_id(id)?;
                        id.to_owned()
                    }
                    None => String::new(),
                };
                let catalog = self.rpc.get_catalog(pb::AccountRef { account_id }).await?;
                let provider = if catalog.provider.is_empty() {
                    crate::domain::default_provider()
                } else {
                    catalog.provider.clone()
                };
                let default_model = format!("{provider}-default");
                let mut data =
                    vec![json!({"id": default_model, "object": "model", "owned_by": provider})];
                data.extend(
                    catalog
                        .models
                        .into_iter()
                        .map(|m| json!({"id": m.mode_id, "object": "model", "owned_by": provider})),
                );
                return Ok(json!({"object":"list","data":data}));
            }
            "gemini_session_get" | "gemini_session_close" | "gemini_media_download" => {
                let id = args["session_id"]
                    .as_str()
                    .ok_or_else(|| ServiceError::invalid("missing session_id"))?;
                validate_id(id)?;
                let session = pb::SessionRef { id: id.into() };
                if name == "gemini_session_get" {
                    let info = self.rpc.get_session(session).await?;
                    return Ok(codec::session_json(info));
                } else if name == "gemini_media_download" {
                    let list = self.rpc.recover_media(session).await?;
                    let artifacts: Vec<_> = list.artifacts.into_iter().map(|a| a.id).collect();
                    return Ok(json!({"artifacts": artifacts}));
                } else {
                    self.rpc.close_session(session).await?;
                    return Ok(json!({"closed": true}));
                }
            }
            "gemini_knowledge_query" => {
                let results = self
                    .rpc
                    .query_knowledge(pb::KnowledgeQuery {
                        query: args["query"].as_str().unwrap_or("").into(),
                        source: args["source"].as_str().map(str::to_owned),
                        limit: args["limit"].as_u64().unwrap_or(5).min(50) as u32,
                    })
                    .await?;
                return serde_json::to_value(results).map_err(ServiceError::storage);
            }
            "gemini_ask" | "gemini_image_generate" => {}
            _ => return Err(ServiceError::invalid("unknown tool")),
        }
        let mut request = TurnRequest::text(
            args["prompt"]
                .as_str()
                .ok_or_else(|| ServiceError::invalid("missing prompt"))?,
        );
        request.session_id = args["session_id"].as_str().map(str::to_owned);
        request.account_id = args["account_id"].as_str().unwrap_or("").into();
        request.retention = match args["session_retention"].as_str() {
            None | Some("ephemeral") => SessionRetention::Ephemeral,
            Some("retain") => SessionRetention::Retain,
            Some(other) => {
                return Err(ServiceError::invalid(format!(
                    "invalid session_retention '{other}': expected 'ephemeral' or 'retain'"
                )));
            }
        };
        request.model = args["model"].as_str().map(str::to_owned);
        request.thinking = args["thinking"].as_str().map(str::to_owned);
        request.mode = match args["session_mode"].as_str() {
            None | Some("chat") => SessionMode::Chat,
            Some("temporary") => SessionMode::Temporary,
            Some(other) => {
                return Err(ServiceError::invalid(format!(
                    "invalid session_mode '{other}': expected 'chat' or 'temporary'"
                )));
            }
        };
        request.timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);
        request.artifacts =
            serde_json::from_value(args.get("artifacts").cloned().unwrap_or(json!([])))
                .map_err(ServiceError::storage)?;
        request.generate_image = name == "gemini_image_generate";
        let id = self.rpc.create_turn(codec::request_to(request)).await?.id;
        let mut guard = CancelOnDrop {
            server: self.clone(),
            id: Some(id.clone()),
        };
        let mut stream = self
            .rpc
            .watch_events(crate::rpc::pb::EventCursor {
                run_id: id.clone(),
                after: 0,
            })
            .await?;
        let mut progress = 0.;
        loop {
            let item = tokio::select! {
                _ = ctx.ct.cancelled() => {
                    return Err(ServiceError::new(
                        crate::domain::ErrorCode::Cancelled,
                        "mcp",
                        "request cancelled",
                    ));
                }
                item = stream.next() => item,
            };
            match item {
                Some(Ok(event)) => {
                    if let Some(token) = ctx.meta.get_progress_token() {
                        progress += 1.;
                        let mut update = ProgressNotificationParam::new(token, progress);
                        update.message = Some(event.kind.clone());
                        let _ = ctx.peer.notify_progress(update).await;
                    }
                    if matches!(
                        event.kind.as_str(),
                        "completed" | "failed" | "cancelled" | "outcome_unknown" | "awaiting_human"
                    ) {
                        break;
                    }
                }
                Some(Err(err)) => return Err(err),
                None => break,
            }
        }
        let run = codec::turn_from(self.rpc.get_turn(pb::TurnRef { id: id.clone() }).await?)?;
        if let Some(error) = run.error {
            guard.id = None;
            return Err(error);
        }
        if let Some(result) = run.result {
            guard.id = None;
            return serde_json::to_value(result).map_err(ServiceError::storage);
        }
        Err(ServiceError::new(
            crate::domain::ErrorCode::SubmissionOutcomeUnknown,
            "mcp",
            "event stream ended before a terminal outcome was recorded",
        ))
    }
}
struct CancelOnDrop {
    server: Server,
    id: Option<String>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let server = self.server.clone();
            tokio::spawn(async move {
                let _ = server.rpc.cancel_turn(pb::TurnRef { id }).await;
            });
        }
    }
}
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Err(ServiceError::invalid("invalid identifier"))
    } else {
        Ok(())
    }
}
fn definitions() -> Vec<Tool> {
    let query = json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string"},
            "account_id": {"type": "string"},
            "session_id": {"type": "string"},
            "session_mode": {"type": "string", "enum": ["chat", "temporary"]},
            "session_retention": {"type": "string", "enum": ["ephemeral", "retain"]},
            "model": {"type": "string"},
            "thinking": {"type": "string"},
            "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 900},
            "artifacts": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["prompt"],
        "additionalProperties": false
    });
    vec![
        Tool::new("gemini_ask","Ask Gemini Web; optional explicitly owned conversation. Does not execute coding tools.",query.as_object().unwrap().clone()),
        Tool::new("gemini_image_generate","Generate/edit images; attach input image artifact IDs; returns artifact IDs.",query.as_object().unwrap().clone()),
        Tool::new("gemini_models","Read locally cached model capabilities without opening a browser.",json!({"type":"object","properties":{"account_id":{"type":"string"}},"additionalProperties":false}).as_object().unwrap().clone()),
        Tool::new("gemini_session_get","Inspect the typed mode, state, and continuability of an owned browser session.",json!({"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}).as_object().unwrap().clone()),
        Tool::new("gemini_session_close","Close an owned browser conversation tab.",json!({"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}).as_object().unwrap().clone()),
        Tool::new("gemini_media_download","Download images already generated in an owned session; never resubmits a prompt.",json!({"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}).as_object().unwrap().clone()),
        Tool::new("gemini_knowledge_query","Retrieve local source excerpts with citations; no model call.",json!({"type":"object","properties":{"query":{"type":"string"},"source":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}},"required":["query"],"additionalProperties":false}).as_object().unwrap().clone()),
    ]
}
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        );
        info.server_info = Implementation::new("anticrumb", env!("CARGO_PKG_VERSION"));
        info.instructions=Some("Local Gemini Web bridge. Human verification pauses the account. Generated files use gemini://artifacts/{account}/{id}. Token usage is unavailable.".into());
        info
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        definitions().into_iter().find(|t| t.name == name)
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: definitions(),
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let value = Value::Object(request.arguments.unwrap_or_default());
        let result = match self.invoke(&request.name, value, &ctx).await {
            Ok(v) => CallToolResult::structured(v),
            Err(e) => CallToolResult::structured_error(json!({"error":e})),
        };
        Ok(result.into())
    }
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        serde_json::from_value(json!({"resourceTemplates":[{"uriTemplate":"gemini://artifacts/{account}/{id}","name":"Gemini artifact","description":format!("Scoped generated or uploaded artifact, max {}.", crate::domain::MAX_ARTIFACT_LABEL)}]}))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        let parts: Vec<_> = request
            .uri
            .strip_prefix("gemini://artifacts/")
            .unwrap_or("")
            .split('/')
            .collect();
        if parts.len() != 2 || parts.iter().any(|id| validate_id(id).is_err()) {
            return Err(ErrorData::invalid_params("invalid artifact URI", None));
        }
        let downloaded = self
            .rpc
            .get_artifact_bytes(crate::rpc::pb::ArtifactRef {
                account_id: parts[0].into(),
                id: parts[1].into(),
            })
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        use base64::Engine;
        let mime = downloaded.artifact.mime;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&downloaded.data);
        drop(downloaded.data);
        let result = ReadResourceResult::new(vec![
            ResourceContents::blob(encoded, request.uri).with_mime_type(mime)
        ]);
        Ok(result.into())
    }
}
pub async fn serve(rpc: crate::rpc::client::RpcClient) -> Result<()> {
    let server = Server { rpc };
    server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(ServiceError::storage)?
        .waiting()
        .await
        .map_err(ServiceError::storage)?;
    Ok(())
}
