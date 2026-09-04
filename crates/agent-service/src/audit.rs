//! Append-only, hash-chained JSON-lines audit log.
//!
//! Every line is `{"prev": <hex>, "hash": <hex>, "entry": {...}}` where `hash` is SHA-256 over the
//! previous hash and the entry's bytes, so altering, removing or reordering a line in the middle
//! breaks every hash after it. Opening the log verifies the whole chain and refuses a broken one.
//!
//! What this does and does not prove. Unkeyed, it detects accidental corruption and any edit that
//! leaves later lines in place: a crash mid-write, a truncated copy, a line changed by hand. It is
//! not tamper evidence against someone who can rewrite the file, since they can recompute every
//! hash from the point they changed, and deleting a suffix leaves a shorter but self-consistent
//! chain. Set `AUDIT_KEY` to make the chain an HMAC instead: an edit then requires the key, and
//! the head hash can be copied somewhere the writer cannot reach (a log shipper, another host) to
//! detect suffix deletion too. That external copy is the operator's to arrange; this module does
//! not ship one.
//! Two kinds of entry are written: a `pre_action` record before every action tool call (the call
//! is refused if this record cannot be written, so no action ever reaches the engine unaudited)
//! and a `turn` record after every turn with the user text, every tool call and result, the
//! reply, token usage, latency and verifier flags. Each append is flushed to disk before it returns.
//!
//! Clones share one chain, so every writer in a process appends through the same lock. One log
//! belongs to one process: two processes appending to the same file would interleave two chains.

use serde_json::{Value, json};
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
    /// `AUDIT_KEY`, when set: the chain is then keyed, so a rewrite needs the key.
    key: Option<Vec<u8>>,
}

impl Audit {
    /// Opens (or creates) the log at `path`, verifying the chain already in it. `None` disables
    /// auditing, which is an explicit operator choice and never a fallback.
    pub fn new(path: Option<PathBuf>) -> std::io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self { inner: None });
        };
        let key = std::env::var("AUDIT_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(String::into_bytes);
        Self::with_key(Some(path), key)
    }

    /// [`Audit::new`] with the key given rather than read from `AUDIT_KEY`.
    pub fn with_key(path: Option<PathBuf>, key: Option<Vec<u8>>) -> std::io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self { inner: None });
        };
        let prev = Self::verify_with(&path, key.as_deref())?;
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(Chain { path, prev, key }))),
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
        let key = std::env::var("AUDIT_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(String::into_bytes);
        Self::verify_with(path, key.as_deref())
    }

    /// [`Audit::verify`] with the key given rather than read from `AUDIT_KEY`.
    pub fn verify_with(path: &Path, key: Option<&[u8]>) -> std::io::Result<[u8; 32]> {
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
            let expected = chain(prev, &entry, key);
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
        let key = c.key.clone();
        let key = key.as_deref();
        let bytes = serde_json::to_vec(entry)?;
        let hash = chain(c.prev, &bytes, key);
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

/// One link: SHA-256 over the previous hash, the entry, and the key when one is set. With a key
/// this is a keyed chain (an HMAC-style construction over a fixed-length prefix, which is not
/// length-extendable here because every input is fixed length or terminal), so a line cannot be
/// rewritten without it.
fn chain(prev: [u8; 32], entry: &[u8], key: Option<&[u8]>) -> [u8; 32] {
    let mut h = Sha256::new();
    if let Some(k) = key {
        h.update((k.len() as u64).to_be_bytes());
        h.update(k);
    }
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
    fn a_keyed_chain_cannot_be_recomputed_without_the_key() {
        let path = temp("keyed");
        let _ = std::fs::remove_file(&path);
        let key = Some(b"a-secret".to_vec());
        let audit = Audit::with_key(Some(path.clone()), key.clone()).unwrap();
        audit.append(&json!({ "event": "turn", "reply": "placed" })).unwrap();
        assert!(
            Audit::verify_with(&path, key.as_deref()).is_ok(),
            "the writer's own key verifies"
        );
        assert!(
            Audit::verify_with(&path, None).is_err(),
            "the chain does not verify without the key"
        );
        // Someone who edits the file and recomputes the chain without the key is caught.
        let entry = serde_json::json!({ "event": "turn", "reply": "PLACED" });
        let payload = serde_json::to_vec(&entry).unwrap();
        let forged = chain([0; 32], &payload, None);
        let line = serde_json::json!({ "prev": hex(&[0; 32]), "hash": hex(&forged), "entry": entry });
        std::fs::write(&path, format!("{line}\n")).unwrap();
        assert!(
            Audit::verify_with(&path, key.as_deref()).is_err(),
            "a chain recomputed without the key does not verify"
        );
        let _ = std::fs::remove_file(&path);
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
