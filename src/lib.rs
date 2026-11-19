//! Web coding agent, provider gateway, runtime providers, and MCP service.
pub mod account;
pub mod adapter;
pub mod agent;
pub mod agent_service;
pub mod browser_control;
pub mod browser_host;
mod browser_network;
pub mod build_info;
pub mod cdp;
pub mod cli;
pub mod domain;
pub mod gemini;
pub mod http;
pub mod input;
pub mod knowledge;
pub mod mcp_client;
pub mod mcp_sdk;
pub mod protocol;
pub mod provider;
pub mod remote_backend;
pub mod rpc;
pub(crate) mod schema;
pub mod service;
pub mod store;
pub mod tab_pool;
pub mod tool_codec;
pub mod tui;
pub mod update;
pub(crate) mod url_lite;
pub mod web;
pub mod web_chat;
pub mod wire;

pub fn run_cli() {
    if let Err(error) = cli::run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("{}", serde_json::json!({"error":error}));
        std::process::exit(1);
    }
}
