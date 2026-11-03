use std::collections::HashSet;

use tokio_tungstenite::tungstenite::Message;

use super::*;

const ADDRESS: &str = "addr1qx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzer3n0d3vllmyqwsx5wktcd8cc3sq835lu7drv2xwl2wywfgse35a3x";
const POINT: &str =
    r#"{"slot":110289017,"id":"1d0b7b1a9cbb2f1ea4b9c9f1c0a3a0d5a7d4d5b0a6b0c0e3e2b1f8c2c3a8b4d1"}"#;

fn request(method: &str, params: Option<&str>) -> String {
    match params {
        Some(params) => {
            format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":"fixture"}}"#)
        }
        None => format!(r#"{{"jsonrpc":"2.0","method":"{method}","id":"fixture"}}"#),
    }
}

fn labels(message: &str) -> (&'static str, &'static str) {
    let classification = classify(message.as_bytes());
    (
        classification.method.as_str(),
        classification.shape.as_str(),
    )
}

fn list(item: &str, count: usize) -> String {
    format!("[{}]", vec![item; count].join(","))
}

/// A request for `method` whose serialized length is exactly `len` bytes,
/// padded inside `params.transaction.cbor`.
fn sized(method: &str, extra_params: &str, len: usize) -> String {
    let head = format!(
        r#"{{"jsonrpc":"2.0","method":"{method}","params":{{{extra_params}"transaction":{{"cbor":""#
    );
    let tail = r#""}},"id":1}"#;
    let padding = len - head.len() - tail.len();
    let message = format!("{head}{}{tail}", "a".repeat(padding));
    assert_eq!(message.len(), len);
    message
}

#[test]
fn one_fixture_per_listed_method() {
    let utxo_by_ref = r#"{"outputReferences":[{"transaction":{"id":"ab"},"index":0}]}"#;
    let at_point = format!(r#"{{"point":{POINT}}}"#);
    let fixtures: &[(&str, Option<&str>, &str)] = &[
        (
            "findIntersection",
            Some(r#"{"points":["origin"]}"#),
            "origin",
        ),
        ("nextBlock", None, "none"),
        ("acquireLedgerState", Some(&at_point), "point"),
        ("releaseLedgerState", None, "none"),
        ("queryLedgerState/constitution", None, "none"),
        (
            "queryLedgerState/constitutionalCommittee",
            Some(r#"{"members":[]}"#),
            "params",
        ),
        (
            "queryLedgerState/delegateRepresentatives",
            Some(r#"{"keys":["ab"]}"#),
            "filtered",
        ),
        ("queryLedgerState/dump", None, "none"),
        ("queryLedgerState/epoch", None, "none"),
        ("queryLedgerState/eraStart", None, "none"),
        ("queryLedgerState/eraSummaries", None, "none"),
        (
            "queryLedgerState/governanceProposals",
            Some(r#"{"proposals":[]}"#),
            "params",
        ),
        ("queryLedgerState/liveStakeDistribution", None, "none"),
        ("queryLedgerState/nonces", None, "none"),
        ("queryLedgerState/operationalCertificates", None, "none"),
        (
            "queryLedgerState/projectedRewards",
            Some(r#"{"stake":[1000000]}"#),
            "params",
        ),
        ("queryLedgerState/proposedProtocolParameters", None, "none"),
        ("queryLedgerState/protocolParameters", Some("{}"), "none"),
        (
            "queryLedgerState/rewardAccountSummaries",
            Some(r#"{"keys":["ab"],"scripts":["cd"]}"#),
            "keys/2-10",
        ),
        ("queryLedgerState/rewardsProvenance", None, "none"),
        ("queryLedgerState/stakePools", None, "all"),
        ("queryLedgerState/stakePoolsPerformances", None, "none"),
        ("queryLedgerState/tip", None, "none"),
        ("queryLedgerState/treasuryAndReserves", None, "none"),
        (
            "queryLedgerState/utxo",
            Some(utxo_by_ref),
            "outputReferences/1",
        ),
        ("queryNetwork/blockHeight", None, "none"),
        (
            "queryNetwork/genesisConfiguration",
            Some(r#"{"era":"shelley"}"#),
            "params",
        ),
        ("queryNetwork/startTime", None, "none"),
        ("queryNetwork/tip", None, "none"),
        ("acquireMempool", None, "none"),
        ("nextTransaction", Some(r#"{"fields":"all"}"#), "full"),
        ("hasTransaction", Some(r#"{"id":"ab"}"#), "params"),
        ("sizeOfMempool", None, "none"),
        ("releaseMempool", None, "none"),
        (
            "submitTransaction",
            Some(r#"{"transaction":{"cbor":"84a3"}}"#),
            "bytes/<4k",
        ),
        (
            "evaluateTransaction",
            Some(r#"{"transaction":{"cbor":"84a3"}}"#),
            "bytes/<4k",
        ),
    ];

    assert_eq!(fixtures.len(), Method::LISTED.len());
    let covered: HashSet<_> = fixtures.iter().map(|(method, _, _)| *method).collect();
    for method in Method::LISTED {
        assert!(
            covered.contains(method.as_str()),
            "no fixture for {method:?}"
        );
    }

    for (method, params, shape) in fixtures {
        let message = request(method, *params);
        assert_eq!(labels(&message), (*method, *shape), "{message}");
    }
}

#[test]
fn utxo_shapes() {
    let utxo = |params: Option<&str>| labels(&request("queryLedgerState/utxo", params)).1;
    assert_eq!(utxo(None), "whole");
    assert_eq!(utxo(Some("{}")), "whole");
    assert_eq!(utxo(Some("{ \n }")), "whole");
    assert_eq!(utxo(Some(r#"{"limit":10}"#)), "params");
    assert_eq!(utxo(Some(r#"{"addresses":"addr1"}"#)), "params");
    assert_eq!(utxo(Some(r#"["addr1"]"#)), "params");

    let address = format!("\"{ADDRESS}\"");
    let reference = r#"{"transaction":{"id":"ab"},"index":0}"#;
    for (count, bucket) in [
        (0, "0"),
        (1, "1"),
        (2, "2-10"),
        (10, "2-10"),
        (11, "11-100"),
        (100, "11-100"),
        (101, "101+"),
    ] {
        let addresses = format!(r#"{{"addresses":{}}}"#, list(&address, count));
        assert_eq!(utxo(Some(&addresses)), format!("addresses/{bucket}"));
        let references = format!(r#"{{"outputReferences":{}}}"#, list(reference, count));
        assert_eq!(
            utxo(Some(&references)),
            format!("outputReferences/{bucket}")
        );
    }
}

#[test]
fn find_intersection_shapes() {
    let find = |params: Option<&str>| labels(&request("findIntersection", params)).1;
    assert_eq!(find(None), "none");
    assert_eq!(find(Some("{}")), "none");
    assert_eq!(find(Some(r#"{"points":["origin"]}"#)), "origin");
    assert_eq!(find(Some(r#"{"points":["orig\u0069n"]}"#)), "origin");
    assert_eq!(
        find(Some(&format!(r#"{{"points":[{POINT}]}}"#))),
        "points/1"
    );
    assert_eq!(
        find(Some(&format!(r#"{{"points":[{POINT},"origin"]}}"#))),
        "points/2-10"
    );
    assert_eq!(
        find(Some(r#"{"points":["origin","origin"]}"#)),
        "points/2-10"
    );
    assert_eq!(find(Some(r#"{"points":"origin"}"#)), "params");
    for (count, bucket) in [
        (0, "0"),
        (1, "1"),
        (2, "2-10"),
        (10, "2-10"),
        (11, "11-100"),
        (100, "11-100"),
        (101, "101+"),
    ] {
        let params = format!(r#"{{"points":{}}}"#, list(POINT, count));
        assert_eq!(find(Some(&params)), format!("points/{bucket}"));
    }
}

#[test]
fn acquire_and_next_transaction_shapes() {
    let acquire = |params: Option<&str>| labels(&request("acquireLedgerState", params)).1;
    assert_eq!(acquire(Some(r#"{"point":"origin"}"#)), "origin");
    assert_eq!(acquire(Some(&format!(r#"{{"point":{POINT}}}"#))), "point");
    assert_eq!(acquire(None), "point");

    let next = |params: Option<&str>| labels(&request("nextTransaction", params)).1;
    assert_eq!(next(Some(r#"{"fields":"all"}"#)), "full");
    assert_eq!(next(Some(r#"{"fields":"none"}"#)), "ids");
    assert_eq!(next(Some("{}")), "ids");
    assert_eq!(next(None), "ids");
}

#[test]
fn filter_and_key_shapes() {
    let pools = |params: Option<&str>| labels(&request("queryLedgerState/stakePools", params)).1;
    assert_eq!(pools(None), "all");
    assert_eq!(pools(Some(r#"{"stakePools":[]}"#)), "all");
    assert_eq!(
        pools(Some(r#"{"stakePools":[{"id":"pool1"}]}"#)),
        "filtered"
    );

    let dreps = |params: Option<&str>| {
        labels(&request("queryLedgerState/delegateRepresentatives", params)).1
    };
    assert_eq!(dreps(None), "all");
    assert_eq!(dreps(Some(r#"{"keys":[],"scripts":[]}"#)), "all");
    assert_eq!(dreps(Some(r#"{"scripts":["ab"]}"#)), "filtered");

    let rewards = |params: Option<&str>| {
        labels(&request("queryLedgerState/rewardAccountSummaries", params)).1
    };
    assert_eq!(rewards(None), "keys/0");
    for (keys, scripts, bucket) in [
        (0, 0, "0"),
        (1, 0, "1"),
        (1, 1, "2-10"),
        (5, 5, "2-10"),
        (11, 0, "11-100"),
        (50, 50, "11-100"),
        (100, 1, "101+"),
    ] {
        let params = format!(
            r#"{{"keys":{},"scripts":{}}}"#,
            list("\"ab\"", keys),
            list("\"cd\"", scripts)
        );
        assert_eq!(rewards(Some(&params)), format!("keys/{bucket}"));
    }
}

#[test]
fn byte_shapes_at_size_edges() {
    let edges = [
        (4095, "<4k"),
        (4096, "4k-16k"),
        (16383, "4k-16k"),
        (16384, "16k-64k"),
        (65535, "16k-64k"),
        (65536, "64k+"),
        (65537, "64k+"),
    ];
    for (len, bucket) in edges {
        let submit = sized("submitTransaction", "", len);
        assert_eq!(
            labels(&submit),
            ("submitTransaction", &*format!("bytes/{bucket}"))
        );

        let evaluate = sized("evaluateTransaction", "", len);
        assert_eq!(
            labels(&evaluate),
            ("evaluateTransaction", &*format!("bytes/{bucket}"))
        );

        let with_utxo = sized("evaluateTransaction", r#""additionalUtxo":[{"a":1}],"#, len);
        // The oversize path does not parse, so it cannot see additionalUtxo.
        let expected = if len > OVERSIZE_BYTES {
            format!("bytes/{bucket}")
        } else {
            format!("bytes/{bucket}+utxo")
        };
        assert_eq!(labels(&with_utxo), ("evaluateTransaction", &*expected));
    }

    let empty_utxo = sized("evaluateTransaction", r#""additionalUtxo":[],"#, 200);
    assert_eq!(labels(&empty_utxo).1, "bytes/<4k");
}

#[test]
fn oversize_messages_are_not_parsed() {
    let utxo = format!(
        r#"{{"jsonrpc":"2.0","method":"queryLedgerState/utxo","params":{{"addresses":{}}}}}"#,
        list(&format!("\"{ADDRESS}\""), 700)
    );
    assert!(utxo.len() > OVERSIZE_BYTES);
    let classification = classify(utxo.as_bytes());
    assert_eq!(classification.method, Method::QueryUtxo);
    assert_eq!(classification.shape, Shape::Oversize);
    assert_eq!(classification.id, RequestId::Other);

    // At exactly 64 KiB the message is still parsed.
    let padded = |len: usize| {
        let head = r#"{"jsonrpc":"2.0","method":"nextBlock","params":{"pad":""#;
        let tail = r#""}}"#;
        format!("{head}{}{tail}", "a".repeat(len - head.len() - tail.len()))
    };
    assert_eq!(labels(&padded(OVERSIZE_BYTES)), ("nextBlock", "params"));
    assert_eq!(
        labels(&padded(OVERSIZE_BYTES + 1)),
        ("nextBlock", "oversize")
    );

    // Whitespace around the colon is allowed; an escaped method is not read.
    let spaced = sized("submitTransaction", "", 70_000).replace(
        r#""method":"submitTransaction""#,
        "\"method\" :\n \"submitTransaction\"",
    );
    assert_eq!(labels(&spaced), ("submitTransaction", "bytes/64k+"));
    let escaped = sized("submitTransaction", "", 70_000).replace(
        r#""method":"submitTransaction""#,
        r#""method":"submit\u0054ransaction""#,
    );
    assert_eq!(labels(&escaped), ("unknown", "none"));

    let array = format!("[{}]", sized("submitTransaction", "", 70_000));
    assert_eq!(labels(&array), ("invalid", "none"));
}

#[test]
fn escaped_method_strings_are_decoded() {
    assert_eq!(
        labels(r#"{"jsonrpc":"2.0","method":"next\u0042lock"}"#),
        ("nextBlock", "none")
    );
    assert_eq!(
        labels(r#"{"jsonrpc":"2.0","method":"queryNetwork\/tip"}"#),
        ("queryNetwork/tip", "none")
    );
}

#[test]
fn unlisted_and_malformed_messages() {
    assert_eq!(
        labels(r#"{"jsonrpc":"2.0","method":"rm -rf"}"#),
        ("unknown", "none")
    );
    assert_eq!(
        labels(r#"{"jsonrpc":"2.0","method":"NextBlock"}"#),
        ("unknown", "none")
    );

    assert_eq!(labels(r#"{"jsonrpc":"2.0","id":1}"#), ("invalid", "none"));
    assert_eq!(
        labels(r#"[{"jsonrpc":"2.0","method":"nextBlock"}]"#),
        ("invalid", "none")
    );
    assert_eq!(labels(r#""method""#), ("invalid", "none"));
    assert_eq!(labels(r#"{"method":42}"#), ("invalid", "none"));
    assert_eq!(labels(r#"{"method":null}"#), ("invalid", "none"));
    assert_eq!(
        labels(r#"{"params":{"method":"nextBlock"}}"#),
        ("invalid", "none")
    );
    assert_eq!(labels(r#"{"method":"nextBlock""#), ("invalid", "none"));
    assert_eq!(labels(""), ("invalid", "none"));
}

#[test]
fn random_method_strings_only_ever_label_unknown() {
    // xorshift64*, so the test needs no RNG dependency and is reproducible.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };

    let mut seen = HashSet::new();
    let mut methods = HashSet::new();
    while methods.len() < 10_000 {
        let len = 1 + (next() % 48) as usize;
        let method: String = (0..len)
            .map(|_| char::from_u32(0x20 + (next() % 0x5F) as u32).unwrap())
            .collect();
        if Method::from_name(&method) != Method::Unknown || !methods.insert(method.clone()) {
            continue;
        }
        let message = format!(
            r#"{{"jsonrpc":"2.0","method":{},"id":1}}"#,
            serde_json::to_string(&method).unwrap()
        );
        let classification = classify(message.as_bytes());
        seen.insert((
            classification.method.as_str(),
            classification.shape.as_str(),
        ));
    }
    assert_eq!(seen, HashSet::from([("unknown", "none")]));
}

#[test]
fn request_ids() {
    let id = |id: &str| classify(format!(r#"{{"method":"nextBlock","id":{id}}}"#).as_bytes()).id;
    assert_eq!(classify(br#"{"method":"nextBlock"}"#).id, RequestId::Absent);
    assert_eq!(id("null"), RequestId::Null);
    assert_eq!(id("-7"), RequestId::Int(-7));
    assert_eq!(id("18446744073709551615"), RequestId::Other);
    assert_eq!(id("1.5"), RequestId::Other);
    assert_eq!(id(r#"{"a":[1]}"#), RequestId::Other);
    assert!(matches!(id(r#""abc""#), RequestId::Str(_)));
    assert_eq!(id(r#""abc""#), id(r#""\u0061bc""#));
    assert_ne!(id(r#""abc""#), id(r#""abd""#));
}

#[test]
fn heavy_classes() {
    let heavy = |method: &str, params: Option<&str>| {
        is_heavy(&classify(request(method, params).as_bytes()))
    };
    let utxo = "queryLedgerState/utxo";
    let references = |count| {
        format!(
            r#"{{"outputReferences":{}}}"#,
            list(r#"{"transaction":{"id":"ab"},"index":0}"#, count)
        )
    };
    assert!(heavy(utxo, None));
    assert!(heavy(utxo, Some(r#"{"addresses":["addr1"]}"#)));
    assert!(heavy(utxo, Some(&references(101))));
    assert!(!heavy(utxo, Some(&references(100))));
    assert!(heavy("queryLedgerState/stakePools", None));
    assert!(!heavy(
        "queryLedgerState/stakePools",
        Some(r#"{"stakePools":[{"id":"p"}]}"#)
    ));
    assert!(heavy("queryLedgerState/delegateRepresentatives", None));
    assert!(!heavy(
        "queryLedgerState/delegateRepresentatives",
        Some(r#"{"keys":["ab"]}"#)
    ));
    for method in [
        "queryLedgerState/liveStakeDistribution",
        "queryLedgerState/rewardsProvenance",
        "queryLedgerState/dump",
        "queryLedgerState/stakePoolsPerformances",
    ] {
        assert!(heavy(method, None));
        assert!(heavy(method, Some(r#"{"x":1}"#)));
    }
    assert!(!heavy("nextBlock", None));
    assert!(!heavy("queryLedgerState/tip", None));
}

#[test]
fn websocket_messages() {
    let text = Message::Text(r#"{"method":"nextBlock"}"#.into());
    assert_eq!(classify_message(&text).unwrap().method, Method::NextBlock);

    let binary = classify_message(&Message::Binary(vec![1, 2, 3])).unwrap();
    assert_eq!(
        (binary.method, binary.shape, binary.bytes),
        (Method::Binary, Shape::None, 3)
    );

    assert!(classify_message(&Message::Ping(vec![])).is_none());
    assert!(classify_message(&Message::Pong(vec![])).is_none());
    assert!(classify_message(&Message::Close(None)).is_none());
}
