//! Response observation cost. Budgets: every scan under 2 µs, and
//! the 8 MiB frame within 2× of the 200 B one, because a scan reads only the
//! head and the tail of a frame.

use std::time::Instant;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

// The proxy is a binary crate, so the bench compiles the module directly.
#[allow(dead_code, unused_imports)]
#[path = "../src/rpc/mod.rs"]
mod rpc;

use rpc::{Correlator, Method, RequestId, QUEUE_CAPACITY};

/// A `nextBlock` response of exactly `len` bytes, padded inside the block's
/// CBOR, with the nested ids a real block carries.
fn next_block(len: usize) -> String {
    let head = r#"{"jsonrpc":"2.0","method":"nextBlock","result":{"direction":"forward","block":{"id":"1d0b7b1a","cbor":""#;
    let tail = r#""},"tip":{"slot":110289017,"id":"9cbb2f1e"}},"id":"bench-1"}"#;
    let frame = format!("{head}{}{tail}", "a".repeat(len - head.len() - tail.len()));
    assert_eq!(frame.len(), len);
    frame
}

fn request(id: i64) -> rpc::Classification {
    rpc::classify(format!(r#"{{"jsonrpc":"2.0","method":"nextBlock","id":{id}}}"#).as_bytes())
}

/// A correlator whose chain-sync queue holds `QUEUE_CAPACITY` entries.
fn full_correlator(with_ids: bool) -> Correlator {
    let mut correlator = Correlator::default();
    let now = Instant::now();
    for id in 0..QUEUE_CAPACITY as i64 {
        let mut classification = request(id);
        if !with_ids {
            classification.id = RequestId::Absent;
        }
        correlator.push(&classification, now);
    }
    correlator
}

fn bench(c: &mut Criterion) {
    let mut scan = c.benchmark_group("scan");
    for (name, len) in [
        ("200B", 200),
        ("64KiB", 64 * 1024),
        ("8MiB", 8 * 1024 * 1024),
    ] {
        let frame = next_block(len);
        println!("{name}: {:?}", rpc::scan_response(frame.as_bytes()));
        scan.bench_function(name, |b| {
            b.iter(|| rpc::scan_response(black_box(frame.as_bytes())))
        });
    }
    scan.finish();

    let mut correlate = c.benchmark_group("correlate");

    // Worst case for an id match: the answered entry is the last of 256. It
    // is pushed back after each match, so the queue stays full.
    let mut by_id = full_correlator(true);
    let last = request(QUEUE_CAPACITY as i64 - 1);
    correlate.bench_function("match-last-of-256", |b| {
        b.iter(|| {
            let pending = by_id.take(black_box(last.id), Some(Method::NextBlock));
            by_id.push(&last, pending.unwrap().sent_at);
        })
    });

    let mut by_queue = full_correlator(false);
    let anonymous = rpc::Classification {
        id: RequestId::Absent,
        ..last
    };
    correlate.bench_function("pop-front-of-256", |b| {
        b.iter(|| {
            let pending = by_queue.take(black_box(RequestId::Null), Some(Method::NextBlock));
            by_queue.push(&anonymous, pending.unwrap().sent_at);
        })
    });
    correlate.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
