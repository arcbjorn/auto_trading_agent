//! MCP (Model Context Protocol) server for the order book.
//!
//! The protocol layer is written by hand on top of `serde_json`: MCP is JSON-RPC 2.0 with a small
//! set of methods, and owning that layer keeps the dependency surface to tokio, hyper and serde.
//!
//! * [`jsonrpc`]  request/response envelopes and error codes
//! * [`protocol`] the MCP methods: initialize, ping, tools/*, resources/*, prompts/*
//! * [`tools`]    the eleven tools the model can call, translated to gRPC
//! * [`policy`]   deterministic risk rules applied before any order reaches the engine
//! * [`units`]    exact decimal <-> integer conversion (no floats anywhere)
//! * [`transport`] stdio (newline-delimited JSON) and Streamable HTTP (POST /mcp)

pub mod jsonrpc;
pub mod metrics;
pub mod policy;
pub mod protocol;
pub mod tools;
pub mod transport;
pub mod units;

pub use policy::{Policy, PolicyConfig, Rejection, reference_price};
pub use protocol::{LATEST_PROTOCOL_VERSION, McpServer, SUPPORTED_PROTOCOL_VERSIONS};
pub use tools::ToolSet;
pub use transport::http::{HttpServerHandle, serve_http};
pub use transport::stdio::run_stdio;
