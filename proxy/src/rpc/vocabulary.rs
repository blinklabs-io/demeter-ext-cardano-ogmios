//! The bounded label vocabulary of the request classifier.
//!
//! Every value here is a `&'static str` chosen by the proxy. A client-supplied
//! method string only ever selects one of these through an exhaustive `match`;
//! it never becomes a label value itself.

/// How the message reached the proxy. Label values match the existing
/// `protocol` label of `ogmios_proxy_http_total_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    Websocket,
    Http,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Websocket => "websocket",
            Transport::Http => "http",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    ChainSync,
    LedgerState,
    Network,
    Mempool,
    Submit,
    Evaluate,
    Other,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::ChainSync => "chain-sync",
            Family::LedgerState => "ledger-state",
            Family::Network => "network",
            Family::Mempool => "mempool",
            Family::Submit => "submit",
            Family::Evaluate => "evaluate",
            Family::Other => "other",
        }
    }
}

/// The Ogmios per-connection request queue a method is served from. Responses
/// are matched against it when correlating requests (step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Queue {
    ChainSync,
    StateQuery,
    TxMonitor,
    TxSubmission,
}

impl Queue {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Queue::ChainSync => "chain-sync",
            Queue::StateQuery => "state-query",
            Queue::TxMonitor => "tx-monitor",
            Queue::TxSubmission => "tx-submission",
        }
    }
}

macro_rules! methods {
    ($($variant:ident => $label:literal, $family:ident, $queue:ident;)*) => {
        /// An Ogmios JSON-RPC method, or one of the synthetic methods
        /// `unknown`, `invalid` and `binary`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Method {
            $($variant,)*
            /// A well-formed JSON-RPC object whose method is not listed.
            Unknown,
            /// Text that is not a JSON object with a string `method`.
            Invalid,
            /// A binary WebSocket frame.
            Binary,
        }

        impl Method {
            /// Every Ogmios method in the vocabulary, without the synthetic ones.
            #[allow(dead_code)]
            pub const LISTED: &'static [Method] = &[$(Method::$variant,)*];

            pub fn from_name(name: &str) -> Method {
                match name {
                    $($label => Method::$variant,)*
                    _ => Method::Unknown,
                }
            }

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Method::$variant => $label,)*
                    Method::Unknown => "unknown",
                    Method::Invalid => "invalid",
                    Method::Binary => "binary",
                }
            }

            pub fn family(self) -> Family {
                match self {
                    $(Method::$variant => Family::$family,)*
                    Method::Unknown | Method::Invalid | Method::Binary => Family::Other,
                }
            }

            #[allow(dead_code)]
            pub fn queue(self) -> Option<Queue> {
                match self {
                    $(Method::$variant => Some(Queue::$queue),)*
                    Method::Unknown | Method::Invalid | Method::Binary => None,
                }
            }
        }
    };
}

methods! {
    FindIntersection => "findIntersection", ChainSync, ChainSync;
    NextBlock => "nextBlock", ChainSync, ChainSync;

    AcquireLedgerState => "acquireLedgerState", LedgerState, StateQuery;
    ReleaseLedgerState => "releaseLedgerState", LedgerState, StateQuery;
    QueryConstitution => "queryLedgerState/constitution", LedgerState, StateQuery;
    QueryConstitutionalCommittee => "queryLedgerState/constitutionalCommittee", LedgerState, StateQuery;
    QueryDelegateRepresentatives => "queryLedgerState/delegateRepresentatives", LedgerState, StateQuery;
    QueryDump => "queryLedgerState/dump", LedgerState, StateQuery;
    QueryEpoch => "queryLedgerState/epoch", LedgerState, StateQuery;
    QueryEraStart => "queryLedgerState/eraStart", LedgerState, StateQuery;
    QueryEraSummaries => "queryLedgerState/eraSummaries", LedgerState, StateQuery;
    QueryGovernanceProposals => "queryLedgerState/governanceProposals", LedgerState, StateQuery;
    QueryLiveStakeDistribution => "queryLedgerState/liveStakeDistribution", LedgerState, StateQuery;
    QueryNonces => "queryLedgerState/nonces", LedgerState, StateQuery;
    QueryOperationalCertificates => "queryLedgerState/operationalCertificates", LedgerState, StateQuery;
    QueryProjectedRewards => "queryLedgerState/projectedRewards", LedgerState, StateQuery;
    QueryProposedProtocolParameters => "queryLedgerState/proposedProtocolParameters", LedgerState, StateQuery;
    QueryProtocolParameters => "queryLedgerState/protocolParameters", LedgerState, StateQuery;
    QueryRewardAccountSummaries => "queryLedgerState/rewardAccountSummaries", LedgerState, StateQuery;
    QueryRewardsProvenance => "queryLedgerState/rewardsProvenance", LedgerState, StateQuery;
    QueryStakePools => "queryLedgerState/stakePools", LedgerState, StateQuery;
    QueryStakePoolsPerformances => "queryLedgerState/stakePoolsPerformances", LedgerState, StateQuery;
    QueryLedgerTip => "queryLedgerState/tip", LedgerState, StateQuery;
    QueryTreasuryAndReserves => "queryLedgerState/treasuryAndReserves", LedgerState, StateQuery;
    QueryUtxo => "queryLedgerState/utxo", LedgerState, StateQuery;

    QueryBlockHeight => "queryNetwork/blockHeight", Network, StateQuery;
    QueryGenesisConfiguration => "queryNetwork/genesisConfiguration", Network, StateQuery;
    QueryStartTime => "queryNetwork/startTime", Network, StateQuery;
    QueryNetworkTip => "queryNetwork/tip", Network, StateQuery;

    AcquireMempool => "acquireMempool", Mempool, TxMonitor;
    NextTransaction => "nextTransaction", Mempool, TxMonitor;
    HasTransaction => "hasTransaction", Mempool, TxMonitor;
    SizeOfMempool => "sizeOfMempool", Mempool, TxMonitor;
    ReleaseMempool => "releaseMempool", Mempool, TxMonitor;

    SubmitTransaction => "submitTransaction", Submit, TxSubmission;

    EvaluateTransaction => "evaluateTransaction", Evaluate, TxSubmission;
}

/// Bucket of an element count: `0`, `1`, `2-10`, `11-100`, `101+`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CountBucket {
    Zero,
    One,
    UpTo10,
    UpTo100,
    Over100,
}

impl CountBucket {
    pub fn of(count: usize) -> Self {
        match count {
            0 => CountBucket::Zero,
            1 => CountBucket::One,
            2..=10 => CountBucket::UpTo10,
            11..=100 => CountBucket::UpTo100,
            _ => CountBucket::Over100,
        }
    }
}

/// Bucket of a message length in bytes: `<4k`, `4k-16k`, `16k-64k`, `64k+`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SizeBucket {
    Under4k,
    Under16k,
    Under64k,
    Over64k,
}

impl SizeBucket {
    pub fn of(bytes: usize) -> Self {
        match bytes {
            0..=4095 => SizeBucket::Under4k,
            4096..=16383 => SizeBucket::Under16k,
            16384..=65535 => SizeBucket::Under64k,
            _ => SizeBucket::Over64k,
        }
    }
}

/// What a request asks for, beyond its method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    None,
    Params,
    Oversize,
    Whole,
    Addresses(CountBucket),
    OutputReferences(CountBucket),
    Origin,
    Points(CountBucket),
    Point,
    Full,
    Ids,
    Bytes(SizeBucket),
    BytesUtxo(SizeBucket),
    Filtered,
    All,
    Keys(CountBucket),
}

impl Shape {
    pub fn as_str(self) -> &'static str {
        const ADDRESSES: [&str; 5] = [
            "addresses/0",
            "addresses/1",
            "addresses/2-10",
            "addresses/11-100",
            "addresses/101+",
        ];
        const OUTPUT_REFERENCES: [&str; 5] = [
            "outputReferences/0",
            "outputReferences/1",
            "outputReferences/2-10",
            "outputReferences/11-100",
            "outputReferences/101+",
        ];
        const POINTS: [&str; 5] = [
            "points/0",
            "points/1",
            "points/2-10",
            "points/11-100",
            "points/101+",
        ];
        const KEYS: [&str; 5] = ["keys/0", "keys/1", "keys/2-10", "keys/11-100", "keys/101+"];
        const BYTES: [&str; 4] = ["bytes/<4k", "bytes/4k-16k", "bytes/16k-64k", "bytes/64k+"];
        const BYTES_UTXO: [&str; 4] = [
            "bytes/<4k+utxo",
            "bytes/4k-16k+utxo",
            "bytes/16k-64k+utxo",
            // `bytes/{size}+utxo` with the size label `64k+`, literally.
            "bytes/64k++utxo",
        ];

        match self {
            Shape::None => "none",
            Shape::Params => "params",
            Shape::Oversize => "oversize",
            Shape::Whole => "whole",
            Shape::Addresses(count) => ADDRESSES[count as usize],
            Shape::OutputReferences(count) => OUTPUT_REFERENCES[count as usize],
            Shape::Origin => "origin",
            Shape::Points(count) => POINTS[count as usize],
            Shape::Point => "point",
            Shape::Full => "full",
            Shape::Ids => "ids",
            Shape::Bytes(size) => BYTES[size as usize],
            Shape::BytesUtxo(size) => BYTES_UTXO[size as usize],
            Shape::Filtered => "filtered",
            Shape::All => "all",
            Shape::Keys(count) => KEYS[count as usize],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_methods_round_trip_through_their_labels() {
        assert_eq!(Method::LISTED.len(), 36);
        for method in Method::LISTED {
            assert_eq!(Method::from_name(method.as_str()), *method);
            assert!(method.queue().is_some());
            assert_ne!(method.family(), Family::Other);
        }
    }

    #[test]
    fn synthetic_methods_are_other_without_a_queue() {
        for method in [Method::Unknown, Method::Invalid, Method::Binary] {
            assert_eq!(method.family(), Family::Other);
            assert_eq!(method.queue(), None);
        }
        // A synthetic label sent by a client is still just an unlisted method.
        assert_eq!(Method::from_name("unknown"), Method::Unknown);
        assert_eq!(Method::from_name("binary"), Method::Unknown);
        assert_eq!(Method::from_name("invalid"), Method::Unknown);
    }

    #[test]
    fn families_and_queues_follow_the_contract() {
        let cases = [
            (Method::NextBlock, "chain-sync", "chain-sync"),
            (Method::AcquireLedgerState, "ledger-state", "state-query"),
            (Method::QueryUtxo, "ledger-state", "state-query"),
            (Method::QueryNetworkTip, "network", "state-query"),
            (Method::NextTransaction, "mempool", "tx-monitor"),
            (Method::SubmitTransaction, "submit", "tx-submission"),
            (Method::EvaluateTransaction, "evaluate", "tx-submission"),
        ];
        for (method, family, queue) in cases {
            assert_eq!(method.family().as_str(), family);
            assert_eq!(method.queue().unwrap().as_str(), queue);
        }
    }

    #[test]
    fn count_buckets_at_their_edges() {
        let cases = [
            (0, "0"),
            (1, "1"),
            (2, "2-10"),
            (10, "2-10"),
            (11, "11-100"),
            (100, "11-100"),
            (101, "101+"),
        ];
        for (count, label) in cases {
            assert_eq!(
                Shape::Keys(CountBucket::of(count)).as_str(),
                format!("keys/{label}")
            );
        }
    }

    #[test]
    fn size_buckets_at_their_edges() {
        let cases = [
            (4095, "<4k"),
            (4096, "4k-16k"),
            (16383, "4k-16k"),
            (16384, "16k-64k"),
            (65535, "16k-64k"),
            (65536, "64k+"),
        ];
        for (bytes, label) in cases {
            assert_eq!(
                Shape::Bytes(SizeBucket::of(bytes)).as_str(),
                format!("bytes/{label}")
            );
        }
    }
}
