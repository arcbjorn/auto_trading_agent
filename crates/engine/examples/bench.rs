//! Throughput of the pure book: `cargo run --release -p engine --example bench`.
use engine::{Book, PlaceRequest, Side, Tif};
use std::time::Instant;

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let mut book = Book::with_balances();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let accounts = ["b0", "b1", "s0", "s1"];
    for a in accounts {
        book.deposit(a, 1_000_000_000_000_000, 10_000_000_000).unwrap();
    }
    let start = Instant::now();
    let mut trades = 0usize;
    for i in 0..n {
        let r = rng.next();
        let side = if r & 1 == 0 { Side::Buy } else { Side::Sell };
        let account = accounts[(if side == Side::Buy { 0 } else { 2 }) + ((r >> 1) & 1) as usize];
        let price = 299_000 + (r >> 8) % 2_000;
        let qty = 1 + (r >> 20) % 1_000;
        let req = PlaceRequest {
            account: account.into(),
            client_order_id: i.to_string(),
            side,
            price,
            qty,
            tif: Tif::Gtc,
        };
        if let Ok((_, fills)) = book.place(req, 0) {
            trades += fills.len();
        }
        if i % 4 == 3 {
            let victim = 1 + (r >> 32) % (i + 1);
            if let Some(o) = book.order(victim).cloned() {
                let _ = book.cancel(&o.account, victim);
            }
        }
    }
    let elapsed = start.elapsed();
    let ops = n + n / 4;
    println!(
        "{ops} book operations ({n} places, {} cancels) in {:.3}s: {:.0} ops/s, {:.0} ns/op, {trades} trades, seq {}",
        n / 4,
        elapsed.as_secs_f64(),
        ops as f64 / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / ops as f64,
        book.seq()
    );
    // Listings run on the matcher thread, so their cost is latency for every other command.
    let iters = 200u32;
    let t = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(book.orders_for("b0", |o| o.status.is_live(), 10));
    }
    let list_orders = t.elapsed() / iters;
    let t = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(book.trades(Some("b0"), 10));
    }
    let list_trades = t.elapsed() / iters;
    println!(
        "with {n} orders in the book: list 10 open orders of one account {:.1} us/call, list 10 trades of one account {:.1} us/call",
        list_orders.as_secs_f64() * 1e6,
        list_trades.as_secs_f64() * 1e6
    );
}
