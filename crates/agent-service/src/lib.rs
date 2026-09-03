//! Natural-language trading service.
//!
//! `POST /chat` runs Claude in a tool loop over the MCP server's tools. The service owns the
//! conversation-level guardrails: which tools are offered on a turn, confirmation of large
//! orders, idempotency keys for retries, a post-turn verifier, and an audit log.
//!
//! * [`anthropic`]  Messages API client over raw HTTPS (there is no official Rust SDK)
//! * [`mcp_client`] JSON-RPC client for the MCP server's Streamable HTTP endpoint
//! * [`gate`]       intent detection, confirmation tokens, and the post-turn verifier
//! * [`agent`]      the tool loop and sessions
//! * [`audit`]      JSON-lines audit log, reused by the evaluation harness
//! * [`http`]       the hyper API server

pub mod agent;
pub mod anthropic;
pub mod audit;
pub mod gate;
pub mod http;
pub mod mcp_client;

pub use agent::{Agent, AgentConfig, AgentError, NoteChannel, Session, ToolCallRecord, TurnResult};
pub use anthropic::{AnthropicClient, AnthropicConfig};
pub use audit::Audit;
pub use mcp_client::McpClient;

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
