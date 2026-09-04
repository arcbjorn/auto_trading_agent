//! Append-only, hash-chained JSON-lines audit log.
//!
//! Every line is `{"prev": <hex>, "hash": <hex>, "entry": {...}}` where `hash` is SHA-256 over the
//! previous hash and the entry's bytes, so a line cannot be altered, removed or reordered without
//! breaking every hash after it. Opening the log verifies the whole chain and refuses a broken one.
//! Two kinds of entry are written: a `pre_action` record before every action tool call (the call
//! is refused if this record cannot be written, so no action ever reaches the engine unaudited)
//! and a `turn` record after every turn with the user text, every tool call and result, the
//! reply, token usage, latency and verifier flags. Each append is flushed to disk before it returns.
//!
//! Clones share one chain, so every writer in a process appends through the same lock. One log
//! belongs to one process: two processes appending to the same file would interleave two chains.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Audit {
    inner: Option<Arc<Mutex<Chain>>>,
}

struct Chain {
    path: PathBuf,
    prev: [u8; 32],
}

impl Audit {
    /// Opens (or creates) the log at `path`, verifying the chain already in it. `None` disables
    /// auditing, which is an explicit operator choice and never a fallback.
    pub fn new(path: Option<PathBuf>) -> std::io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self { inner: None });
        };
        let prev = Self::verify(&path)?;
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(Chain { path, prev }))),
        })
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Walks an existing log and returns the hash of its last line, or an error naming the first
    /// line whose chain does not hold. A missing file is an empty chain.
    pub fn verify(path: &Path) -> std::io::Result<[u8; 32]> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok([0; 32]),
            Err(e) => return Err(e),
        };
        let mut prev = [0u8; 32];
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let bad = |what: &str| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("audit log {} line {}: {what}", path.display(), i + 1),
                )
            };
            let v: Value = serde_json::from_str(line).map_err(|e| bad(&format!("not JSON: {e}")))?;
            if v["prev"].as_str() != Some(hex(&prev).as_str()) {
                return Err(bad("previous hash does not match the line before"));
            }
            let entry = serde_json::to_vec(&v["entry"]).map_err(|e| bad(&e.to_string()))?;
            let expected = chain(prev, &entry);
            if v["hash"].as_str() != Some(hex(&expected).as_str()) {
                return Err(bad("hash does not match the entry"));
            }
            prev = expected;
        }
        Ok(prev)
    }

    /// Appends one entry and flushes it to disk. Errors are the caller's to act on: an action is
    /// refused when its pre-action record fails.
    pub fn append(&self, entry: &Value) -> std::io::Result<()> {
        let Some(inner) = &self.inner else { return Ok(()) };
        let mut c = inner.lock().expect("audit lock");
        let bytes = serde_json::to_vec(entry)?;
        let hash = chain(c.prev, &bytes);
        let mut line = serde_json::to_vec(&json!({ "prev": hex(&c.prev), "hash": hex(&hash), "entry": entry }))?;
        line.push(b'\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&c.path)?;
        f.write_all(&line)?;
        f.sync_data()?;
        c.prev = hash;
        Ok(())
    }

    /// Appends and only logs a failure: for records whose loss must not fail the turn itself.
    pub fn write(&self, entry: &Value) {
        if let Err(e) = self.append(entry) {
            tracing::error!(error = %e, "audit write failed");
        }
    }
}

fn chain(prev: [u8; 32], entry: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(entry);
    h.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("audit-{name}-{}-{nanos}.jsonl", std::process::id()))
    }

    #[test]
    fn the_chain_holds_across_reopen_and_breaks_on_tampering() {
        let path = temp("chain");
        let audit = Audit::new(Some(path.clone())).unwrap();
        audit
            .append(&json!({ "event": "pre_action", "tool": "place_limit_order" }))
            .unwrap();
        audit.append(&json!({ "event": "turn", "reply": "placed" })).unwrap();
        let last = Audit::verify(&path).unwrap();
        // Reopening continues the chain from the last hash.
        let again = Audit::new(Some(path.clone())).unwrap();
        again.append(&json!({ "event": "turn", "reply": "third" })).unwrap();
        assert_ne!(Audit::verify(&path).unwrap(), last);
        // Changing one character of an entry breaks the chain at that line.
        let text = std::fs::read_to_string(&path).unwrap().replace("placed", "PLACED");
        std::fs::write(&path, text).unwrap();
        let err = Audit::new(Some(path.clone())).err().expect("tampered log is refused");
        assert!(err.to_string().contains("line 2"), "{err}");
        let _ = std::fs::remove_file(&path);
        // Disabled auditing accepts everything and writes nothing.
        Audit::disabled().append(&json!({})).unwrap();
    }
}
