//! stdio transport: one JSON-RPC message per line on stdin, one per line on stdout. Nothing else
//! is ever written to stdout; logs go to stderr.

use crate::protocol::McpServer;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn run_stdio(server: Arc<McpServer>) -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_bytes(line.as_bytes()).await {
            let mut bytes = serde_json::to_vec(&reply)?;
            bytes.push(b'\n');
            out.write_all(&bytes).await?;
            out.flush().await?;
        }
    }
    Ok(())
}
