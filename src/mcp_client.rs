//! Explicitly configured stdio MCP tools on the local workspace host.
use crate::{
    agent::{AgentTool, ToolContext, ToolRegistry},
    domain::*,
};
use rmcp::{model::CallToolRequestParams, service::RunningService, RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
#[derive(Deserialize)]
struct Config {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, ServerConfig>,
}
#[derive(Deserialize)]
struct ServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    tools: Vec<String>,
}
pub struct Connections {
    _runtime: Arc<tokio::runtime::Runtime>,
    _services: Vec<Arc<RunningService<RoleClient, ()>>>,
}
struct RemoteTool {
    name: String,
    original: String,
    description: String,
    schema: Value,
    runtime: Arc<tokio::runtime::Runtime>,
    service: Arc<RunningService<RoleClient, ()>>,
}
impl AgentTool for RemoteTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }
    fn execute(&self, args: &Value, ctx: &ToolContext) -> std::result::Result<String, String> {
        let arguments = args
            .as_object()
            .cloned()
            .ok_or("MCP arguments must be an object")?;
        self.runtime.block_on(async {
            let call = self.service.call_tool(
                CallToolRequestParams::new(self.original.clone()).with_arguments(arguments),
            );
            tokio::pin!(call);
            tokio::select! {
                result = &mut call => {
                    let result = result.map_err(|e| format!("MCP tool outcome may be unknown: {e}"))?;
                    let failed = result.is_error.unwrap_or(false);
                    let text = serde_json::to_string(&result).map_err(|e| e.to_string())?;
                    if failed { Err(text) } else { Ok(text) }
                },
                _ = tokio::time::sleep(Duration::from_secs(120)) => Err("MCP tool deadline exceeded; outcome may be unknown; not retried".into()),
                _ = ctx.cancel.cancelled() => Err("cancelled; remote tool outcome may be unknown".into()),
            }
        })
    }
}
pub fn connect(
    path: &Path,
    workspace: &Path,
    environment: Option<&BTreeMap<String, String>>,
    registry: &mut ToolRegistry,
) -> Result<Connections> {
    let bytes = std::fs::read(path).map_err(ServiceError::storage)?;
    if bytes.len() > 256 * 1024 {
        return Err(ServiceError::invalid("MCP config exceeds 256 KiB"));
    }
    let config: Config = serde_json::from_slice(&bytes).map_err(ServiceError::storage)?;
    if config.servers.len() > 8 {
        return Err(ServiceError::invalid("maximum 8 MCP servers"));
    }
    let runtime = Arc::new(tokio::runtime::Runtime::new().map_err(ServiceError::storage)?);
    let mut services = Vec::new();
    let mut count = 0;
    for (name, config) in config.servers {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(ServiceError::invalid(
                "MCP server names must be alphanumeric or underscore",
            ));
        }
        if Path::new(&config.command)
            .file_stem()
            .is_some_and(|s| s == "anticrumb")
        {
            return Err(ServiceError::invalid(
                "recursive anticrumb MCP connection is disabled",
            ));
        }
        let (service, tools) = runtime.block_on(async {
            let mut command = tokio::process::Command::new(&config.command);
            command.current_dir(workspace);
            if let Some(environment) = environment {
                command.env_clear().envs(environment);
            }
            command
                .args(&config.args)
                .envs(config.env)
                .kill_on_drop(true);
            let transport =
                rmcp::transport::TokioChildProcess::new(command).map_err(ServiceError::storage)?;
            let service = tokio::time::timeout(Duration::from_secs(15), ().serve(transport))
                .await
                .map_err(ServiceError::storage)?
                .map_err(ServiceError::storage)?;
            let tools = tokio::time::timeout(Duration::from_secs(15), service.list_all_tools())
                .await
                .map_err(ServiceError::storage)?
                .map_err(ServiceError::storage)?;
            Ok::<_, ServiceError>((Arc::new(service), tools))
        })?;
        for tool in tools {
            if !config.tools.is_empty() && !config.tools.iter().any(|s| s == &*tool.name) {
                continue;
            }
            count += 1;
            if count > 128 {
                return Err(ServiceError::invalid(
                    "MCP catalog exceeds 128 tools; configure an allowlist",
                ));
            }
            let exposed = format!("mcp_{name}_{}", tool.name);
            if exposed.len() > 128 {
                return Err(ServiceError::invalid("MCP tool name exceeds 128 bytes"));
            }
            registry.register(Box::new(RemoteTool {
                name: exposed,
                original: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
                schema: Value::Object((*tool.input_schema).clone()),
                runtime: runtime.clone(),
                service: service.clone(),
            }));
        }
        services.push(service);
    }
    Ok(Connections {
        _runtime: runtime,
        _services: services,
    })
}
