//! stdio transport: one JSON-RPC message per line on stdin, one per line on stdout. Nothing else
//! is ever written to stdout; logs go to stderr.
//!
//! This transport has a server-to-client direction, so it also delivers
//! `notifications/resources/updated`.
//!
//! A pump subscribes to the engine's event stream and sends one notification per subscribed
//! resource as events arrive. Notifications are coalesced over a short window, so a burst of
//! fills becomes one update rather than hundreds.

use crate::protocol::McpServer;
use clob_proto::v1::SubscribeRequest;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::Mutex;

/// Events within this window collapse into one notification per subscribed resource.
pub const COALESCE: Duration = Duration::from_millis(100);

async fn write_line(out: &Mutex<Stdout>, message: &serde_json::Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    let mut out = out.lock().await;
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}

/// Watches the engine's event stream and sends resource-updated notifications for the current
/// subscriptions. Reconnects after a lag or a dropped stream, since the notifications carry no
/// data: a client that gets one re-reads the resource.
async fn pump(server: Arc<McpServer>, out: Arc<Mutex<Stdout>>) {
    loop {
        let mut client = server.tools().engine_client();
        let stream = match client.subscribe(SubscribeRequest::default()).await {
            Ok(s) => s.into_inner(),
            Err(e) => {
                tracing::warn!(error = %e, "event stream unavailable; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut stream = stream;
        loop {
            match stream.message().await {
                Ok(Some(_)) => {
                    // Drain whatever else lands in the window, then notify once per resource.
                    let _ = tokio::time::timeout(COALESCE, async { while let Ok(Some(_)) = stream.message().await {} })
                        .await;
                    for n in server.updated_notifications() {
                        if let Err(e) = write_line(&out, &n).await {
                            tracing::warn!(error = %e, "could not send notification");
                            return;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "event stream ended; resubscribing");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn run_stdio(server: Arc<McpServer>) -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let out = Arc::new(Mutex::new(tokio::io::stdout()));
    let pump_task = tokio::spawn(pump(Arc::clone(&server), Arc::clone(&out)));
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_bytes(line.as_bytes()).await {
            write_line(&out, &reply).await?;
        }
    }
    pump_task.abort();
    Ok(())
}
