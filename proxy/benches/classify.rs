//! Classification cost per message. The contract's budgets: under 2 µs for the
//! small requests, under 50 µs for the 60-address query, and under 5 µs for the
//! 200 KiB oversize message, which must not scale with message size.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

// The proxy is a binary crate, so the bench compiles the module directly.
#[allow(dead_code, unused_imports)]
#[path = "../src/rpc/mod.rs"]
mod rpc;

const ADDRESS: &str = "addr1qx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzer3n0d3vllmyqwsx5wktcd8cc3sq835lu7drv2xwl2wywfgse35a3x";

/// A transaction-carrying request of exactly `len` bytes.
fn transaction(method: &str, len: usize) -> String {
    let head =
        format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{{"transaction":{{"cbor":""#);
    let tail = r#""}},"id":"bench"}"#;
    format!("{head}{}{tail}", "a".repeat(len - head.len() - tail.len()))
}

fn fixtures() -> Vec<(&'static str, String)> {
    let addresses = vec![format!("\"{ADDRESS}\""); 60].join(",");
    vec![
        (
            "nextBlock",
            r#"{"jsonrpc":"2.0","method":"nextBlock","id":42}"#.to_string(),
        ),
        (
            "queryNetwork/tip",
            r#"{"jsonrpc":"2.0","method":"queryNetwork/tip","id":"tip-1"}"#.to_string(),
        ),
        (
            "utxo-60-addresses",
            format!(
                r#"{{"jsonrpc":"2.0","method":"queryLedgerState/utxo","params":{{"addresses":[{addresses}]}},"id":7}}"#
            ),
        ),
        ("submit-16KiB", transaction("submitTransaction", 16 * 1024)),
        (
            "evaluate-200KiB",
            transaction("evaluateTransaction", 200 * 1024),
        ),
    ]
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify");
    for (name, message) in fixtures() {
        let shape = rpc::classify(message.as_bytes()).shape.as_str();
        println!("{name}: {} bytes, shape {shape}", message.len());
        group.bench_function(name, |b| {
            b.iter(|| rpc::classify(black_box(message.as_bytes())))
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
