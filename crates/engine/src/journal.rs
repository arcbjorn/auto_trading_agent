//! Write-ahead journal of commands, and replay.
//!
//! The book is deterministic: the same commands with the same timestamps produce the same orders,
//! trades, ids and sequence numbers. So durability needs only the *inputs*: every place and cancel
//! is appended here, as one JSON line, before its reply is sent. On restart the journal is replayed
//! through the same code and the engine is back exactly where it was, with the next order id and
//! sequence number continuing from there. Reads are never journaled.
//!
//! The matcher flushes the writer once per batch, so under load one write system call covers many
//! commands; `fsync` per batch is optional (crash durability at a latency cost, see the README).

use crate::book::{Book, BookState, ExposureLimits, OrderId, PlaceRequest, Qty};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Record {
    Place {
        /// Wall clock at acceptance, replayed verbatim so timestamps survive a restart.
        t: i64,
        req: PlaceRequest,
    },
    Cancel {
        t: i64,
        account: String,
        id: OrderId,
    },
    Deposit {
        t: i64,
        account: String,
        usdc: u128,
        eth: Qty,
    },
    Withdraw {
        t: i64,
        account: String,
        usdc: u128,
        eth: Qty,
    },
}

pub struct Journal {
    out: BufWriter<File>,
    fsync: bool,
    appended: u64,
}

impl Journal {
    /// Opens (or creates) the journal for appending. `fsync` makes every batch durable on disk
    /// before its replies go out; without it the data is handed to the OS on every batch.
    pub fn open(path: &Path, fsync: bool) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            out: BufWriter::with_capacity(1 << 16, file),
            fsync,
            appended: 0,
        })
    }

    pub fn append(&mut self, record: &Record) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.out, record)?;
        self.out.write_all(b"\n")?;
        self.appended += 1;
        Ok(())
    }

    /// Called once per matcher batch, before the batch's replies are sent.
    pub fn commit(&mut self) -> std::io::Result<()> {
        self.out.flush()?;
        if self.fsync {
            self.out.get_ref().sync_data()?;
        }
        Ok(())
    }

    pub fn appended(&self) -> u64 {
        self.appended
    }

    /// The snapshot that belongs to a journal: the book's state at the moment the journal was
    /// last compacted, next to the journal file.
    pub fn snapshot_path(journal: &Path) -> std::path::PathBuf {
        let mut name = journal.as_os_str().to_owned();
        name.push(".snapshot");
        std::path::PathBuf::from(name)
    }

    /// Rebuilds a book from the snapshot (if any) and the journal tail after it. Returns the book
    /// and how many journal records were replayed on top of the snapshot.
    pub fn recover(journal: &Path, enforce_balances: bool, limits: ExposureLimits) -> std::io::Result<(Book, u64)> {
        let snapshot = Self::snapshot_path(journal);
        let mut book = match File::open(&snapshot) {
            Ok(f) => {
                let state: BookState = serde_json::from_reader(BufReader::new(f)).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("snapshot {}: {e}", snapshot.display()),
                    )
                })?;
                if state.enforce_balances != enforce_balances {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "snapshot {} was taken with balance checks {}; the engine is configured with them {}",
                            snapshot.display(),
                            if state.enforce_balances { "on" } else { "off" },
                            if enforce_balances { "on" } else { "off" }
                        ),
                    ));
                }
                if state.exposure_limits != limits {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "snapshot {} was taken with exposure limits {:?}; the engine is configured with {:?}",
                            snapshot.display(),
                            state.exposure_limits,
                            limits
                        ),
                    ));
                }
                Book::from_state(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let book = if enforce_balances {
                    Book::with_balances()
                } else {
                    Book::new()
                };
                book.with_exposure_limits(limits)
            }
            Err(e) => return Err(e),
        };
        let replayed = Self::replay(journal, &mut book)?;
        Ok((book, replayed))
    }

    /// Writes the book's state as the snapshot (atomically: a temporary file renamed into place)
    /// and empties the journal, so recovery is the snapshot plus whatever is appended afterwards.
    /// Call before opening the journal for appending.
    pub fn compact(journal: &Path, book: &Book) -> std::io::Result<()> {
        let snapshot = Self::snapshot_path(journal);
        let mut tmp = snapshot.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            serde_json::to_writer(&mut w, &book.state())?;
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        std::fs::rename(&tmp, &snapshot)?;
        let truncated = File::create(journal)?;
        truncated.sync_all()?;
        Ok(())
    }

    /// Applies every record in `path` to `book`, in order, and returns how many were replayed.
    /// Errors the book returns (a duplicate client id, a cancel of a filled order) are the same
    /// errors it returned the first time and are ignored. A missing file is an empty journal.
    pub fn replay(path: &Path, book: &mut Book) -> std::io::Result<u64> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut n = 0;
        for (line_no, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record: Record = serde_json::from_str(&line).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("journal line {}: {e}", line_no + 1),
                )
            })?;
            match record {
                Record::Place { t, req } => {
                    let _ = book.place(req, t);
                }
                Record::Cancel { account, id, .. } => {
                    let _ = book.cancel(&account, id);
                }
                Record::Deposit { account, usdc, eth, .. } => {
                    let _ = book.deposit(&account, usdc, eth);
                }
                Record::Withdraw { account, usdc, eth, .. } => {
                    let _ = book.withdraw(&account, usdc, eth);
                }
            }
            n += 1;
            // Replayed events are history, not news: nobody is subscribed yet, so the buffer the
            // sequencer broadcasts from is emptied as we go instead of holding the whole journal.
            if n % 4096 == 0 {
                book.take_new_events();
            }
        }
        book.take_new_events();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::{Side, Tif};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("clob-journal-{name}-{}-{nanos}.jsonl", std::process::id()))
    }

    #[test]
    fn replay_rebuilds_the_same_book_and_continues_its_counters() {
        let path = temp_path("replay");
        let mut live = Book::new();
        let mut journal = Journal::open(&path, false).unwrap();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for i in 0..2_000u64 {
            let r = next();
            let side = if r & 1 == 0 { Side::Buy } else { Side::Sell };
            let account = format!("{}{}", if side == Side::Buy { "b" } else { "s" }, (r >> 1) & 1);
            let req = PlaceRequest {
                account: account.clone(),
                client_order_id: i.to_string(),
                side,
                price: 299_000 + (r >> 8) % 2_000,
                qty: 1 + (r >> 20) % 1_000,
                tif: match (r >> 40) % 3 {
                    0 => Tif::Gtc,
                    1 => Tif::Ioc,
                    _ => Tif::Fok,
                },
            };
            journal
                .append(&Record::Place {
                    t: i as i64,
                    req: req.clone(),
                })
                .unwrap();
            let _ = live.place(req, i as i64);
            if i % 3 == 2 {
                let victim = 1 + (r >> 32) % (i + 1);
                if let Some(o) = live.order(victim).cloned() {
                    journal
                        .append(&Record::Cancel {
                            t: i as i64,
                            account: o.account.to_string(),
                            id: victim,
                        })
                        .unwrap();
                    let _ = live.cancel(&o.account, victim);
                }
            }
            if i % 100 == 99 {
                journal.commit().unwrap();
            }
        }
        journal.commit().unwrap();
        assert!(journal.appended() > 2_000);

        let mut rebuilt = Book::new();
        let replayed = Journal::replay(&path, &mut rebuilt).unwrap();
        assert_eq!(replayed, journal.appended());
        assert_eq!(
            rebuilt.events(),
            live.events(),
            "replay must produce the identical event log"
        );
        assert_eq!(rebuilt.snapshot(1_000), live.snapshot(1_000));
        assert_eq!(rebuilt.seq(), live.seq());
        // The rebuilt book continues where the live one would: same next order id and sequence.
        let req = PlaceRequest {
            account: "b0".into(),
            client_order_id: "after".into(),
            side: Side::Buy,
            price: 1,
            qty: 1,
            tif: Tif::Gtc,
        };
        let (a, _) = live.place(req.clone(), 1).unwrap();
        let (b, _) = rebuilt.place(req, 1).unwrap();
        assert_eq!((a.id, a.seq), (b.id, b.seq));

        // Snapshot plus tail: compact, append more, recover, and it equals the never-restarted book.
        let compact_path = temp_path("compact");
        let _ = std::fs::remove_file(&compact_path);
        let _ = std::fs::remove_file(Journal::snapshot_path(&compact_path));
        {
            let mut j = Journal::open(&compact_path, false).unwrap();
            for i in 0..50u64 {
                let req = PlaceRequest {
                    account: if i % 2 == 0 { "b0".into() } else { "s0".into() },
                    client_order_id: format!("c{i}"),
                    side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                    price: 300_000 + (i % 5) * 7,
                    qty: 10 + i,
                    tif: Tif::Gtc,
                };
                j.append(&Record::Place { t: i as i64, req }).unwrap();
            }
            j.commit().unwrap();
        }
        let (mut compacted, replayed) = Journal::recover(&compact_path, false, ExposureLimits::default()).unwrap();
        assert_eq!(replayed, 50);
        Journal::compact(&compact_path, &compacted).unwrap();
        assert_eq!(std::fs::metadata(&compact_path).unwrap().len(), 0, "journal emptied");
        let mut tail = Journal::open(&compact_path, false).unwrap();
        let more = PlaceRequest {
            account: "b0".into(),
            client_order_id: "after-compaction".into(),
            side: Side::Buy,
            price: 300_100,
            qty: 500,
            tif: Tif::Gtc,
        };
        tail.append(&Record::Place {
            t: 60,
            req: more.clone(),
        })
        .unwrap();
        tail.commit().unwrap();
        let _ = compacted.place(more, 60);
        let (recovered, replayed) = Journal::recover(&compact_path, false, ExposureLimits::default()).unwrap();
        assert_eq!(replayed, 1, "only the tail after the snapshot is replayed");
        recovered.check_invariants().unwrap();
        assert_eq!(recovered.snapshot(100), compacted.snapshot(100));
        assert_eq!(recovered.seq(), compacted.seq());
        assert_eq!(
            recovered.orders_for("b0", |_| true, 100),
            compacted.orders_for("b0", |_| true, 100)
        );
        assert!(
            Journal::recover(&compact_path, true, ExposureLimits::default()).is_err(),
            "a snapshot's balance mode is binding"
        );
        let _ = std::fs::remove_file(&compact_path);
        let _ = std::fs::remove_file(Journal::snapshot_path(&compact_path));

        // A missing journal is empty; a corrupt line is an error, not silent data loss.
        assert_eq!(Journal::replay(&temp_path("missing"), &mut Book::new()).unwrap(), 0);
        std::fs::write(&path, "{ not json\n").unwrap();
        assert!(Journal::replay(&path, &mut Book::new()).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
