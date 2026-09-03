//! Append-only JSON-lines audit log: one line per turn with the user text, every tool call and
//! result, the reply, token usage, latency and verifier flags. The evaluation harness reads the
//! same shape.

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Audit {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl Audit {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn disabled() -> Self {
        Self::new(None)
    }

    pub fn write(&self, entry: &Value) {
        let Some(path) = &self.path else { return };
        let _guard = self.lock.lock().expect("audit lock");
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| {
                let mut line = serde_json::to_vec(entry).unwrap_or_default();
                line.push(b'\n');
                f.write_all(&line)
            });
        if let Err(e) = result {
            tracing::error!(error = %e, path = %path.display(), "audit write failed");
        }
    }
}
