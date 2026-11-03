//! Matching instance responses to the client requests they answer, on one
//! WebSocket connection.
//!
//! Ogmios routes each request into one of four per-connection queues and
//! serves each queue in order, so responses interleave across queues but never
//! within one. Requests that fail to parse are answered at once, outside any
//! queue, and are never tracked here. The correlator mirrors the four queues:
//!
//! 1. A response whose id is an integer or a string takes the first pending
//!    entry with an equal id. Its own method's queue is searched first, so a
//!    client that reuses one id everywhere still matches within the queue that
//!    answered; the other queues follow, each from its front.
//! 2. Otherwise, a response that echoes a queued method pops that queue's
//!    front. An identified response that matched nothing does too, but only
//!    when the front carries no id of its own (the request side does not read
//!    the id of a message over 64 KiB).
//! 3. Anything else is unmatched.
//!
//! Each queue holds at most [`QUEUE_CAPACITY`] entries; the oldest is evicted
//! to make room.

use std::collections::VecDeque;
use std::time::Instant;

use super::classify::{Classification, RequestId};
use super::vocabulary::{Method, Queue, Shape};

pub const QUEUE_CAPACITY: usize = 256;

const QUEUES: [Queue; 4] = [
    Queue::ChainSync,
    Queue::StateQuery,
    Queue::TxMonitor,
    Queue::TxSubmission,
];

/// A request forwarded to the instance and not yet answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub id: RequestId,
    pub method: Method,
    pub shape: Shape,
    pub sent_at: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    /// `unknown`, `invalid` and `binary` messages have no queue.
    Ignored,
    Tracked {
        evicted: Option<Pending>,
    },
}

#[derive(Debug, Default)]
pub struct Correlator {
    queues: [VecDeque<Pending>; 4],
}

impl Correlator {
    /// Tracks a request about to be forwarded.
    pub fn push(&mut self, classification: &Classification, sent_at: Instant) -> Push {
        let Some(queue) = classification.method.queue() else {
            return Push::Ignored;
        };
        let queue = self.queue(queue);
        let evicted = if queue.len() >= QUEUE_CAPACITY {
            queue.pop_front()
        } else {
            None
        };
        queue.push_back(Pending {
            id: classification.id,
            method: classification.method,
            shape: classification.shape,
            sent_at,
        });
        Push::Tracked { evicted }
    }

    /// Takes the entry a response with `id` and the echoed `method` answers.
    pub fn take(&mut self, id: RequestId, method: Option<Method>) -> Option<Pending> {
        let own = method.and_then(Method::queue);
        let identified = matches!(id, RequestId::Int(_) | RequestId::Str(_));

        if identified {
            let order = own
                .into_iter()
                .chain(QUEUES.into_iter().filter(|queue| Some(*queue) != own));
            for queue in order {
                let queue = self.queue(queue);
                if let Some(at) = queue.iter().position(|pending| pending.id == id) {
                    return queue.remove(at);
                }
            }
        }

        let queue = self.queue(own?);
        let front_is_identified = queue
            .front()
            .is_some_and(|pending| matches!(pending.id, RequestId::Int(_) | RequestId::Str(_)));
        if identified && front_is_identified {
            return None;
        }
        queue.pop_front()
    }

    /// Removes every pending entry, at the end of the session.
    pub fn drain(&mut self) -> impl Iterator<Item = Pending> + '_ {
        self.queues.iter_mut().flat_map(|queue| queue.drain(..))
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    fn queue(&mut self, queue: Queue) -> &mut VecDeque<Pending> {
        let index = match queue {
            Queue::ChainSync => 0,
            Queue::StateQuery => 1,
            Queue::TxMonitor => 2,
            Queue::TxSubmission => 3,
        };
        &mut self.queues[index]
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::rpc::{classify, response};

    fn request(method: &str, id: Option<i64>) -> Classification {
        let message = match id {
            Some(id) => format!(r#"{{"jsonrpc":"2.0","method":"{method}","id":{id}}}"#),
            None => format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#),
        };
        classify(message.as_bytes())
    }

    fn reply(correlator: &mut Correlator, method: &str, id: Option<i64>) -> Option<Pending> {
        let id = id.map_or("null".to_string(), |id| id.to_string());
        let frame = format!(r#"{{"jsonrpc":"2.0","method":"{method}","result":{{}},"id":{id}}}"#);
        let response = response::scan(frame.as_bytes());
        correlator.take(response.id, response.method)
    }

    fn ids(pending: &[Option<Pending>]) -> Vec<RequestId> {
        pending.iter().map(|p| p.unwrap().id).collect()
    }

    #[test]
    fn order_within_a_queue() {
        let mut correlator = Correlator::default();
        let start = Instant::now();
        for (i, method) in [
            "acquireLedgerState",
            "queryLedgerState/tip",
            "queryNetwork/tip",
        ]
        .into_iter()
        .enumerate()
        {
            let sent_at = start + Duration::from_millis(i as u64);
            correlator.push(&request(method, None), sent_at);
        }

        let first = reply(&mut correlator, "acquireLedgerState", None).unwrap();
        assert_eq!(first.method, Method::AcquireLedgerState);
        assert_eq!(first.sent_at, start);
        let second = reply(&mut correlator, "queryLedgerState/tip", None).unwrap();
        assert_eq!(second.method, Method::QueryLedgerTip);
        let third = reply(&mut correlator, "queryNetwork/tip", None).unwrap();
        assert_eq!(third.method, Method::QueryNetworkTip);
        assert_eq!(correlator.len(), 0);
    }

    #[test]
    fn pipelined_next_blocks_interleaved_with_a_ledger_query() {
        for with_ids in [true, false] {
            let mut correlator = Correlator::default();
            let now = Instant::now();
            for i in 0..25 {
                correlator.push(&request("nextBlock", with_ids.then_some(i)), now);
            }
            correlator.push(
                &request("queryLedgerState/epoch", with_ids.then_some(1000)),
                now,
            );
            for i in 25..50 {
                correlator.push(&request("nextBlock", with_ids.then_some(i)), now);
            }

            let mut answered = Vec::new();
            for i in 0..10 {
                answered.push(reply(&mut correlator, "nextBlock", with_ids.then_some(i)));
            }
            let query = reply(
                &mut correlator,
                "queryLedgerState/epoch",
                with_ids.then_some(1000),
            );
            for i in 10..50 {
                answered.push(reply(&mut correlator, "nextBlock", with_ids.then_some(i)));
            }

            assert_eq!(query.unwrap().method, Method::QueryEpoch);
            assert!(answered
                .iter()
                .all(|p| p.unwrap().method == Method::NextBlock));
            if with_ids {
                assert_eq!(
                    ids(&answered),
                    (0..50).map(RequestId::Int).collect::<Vec<_>>()
                );
            }
            assert_eq!(correlator.len(), 0, "with ids: {with_ids}");
        }
    }

    #[test]
    fn duplicate_ids_match_in_order_and_within_the_answering_queue() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        correlator.push(&request("nextBlock", Some(1)), now);
        correlator.push(&request("queryNetwork/tip", Some(1)), now);
        correlator.push(&request("queryLedgerState/epoch", Some(1)), now);

        // The chain-sync request waits at the tip; the state queries answer.
        let tip = reply(&mut correlator, "queryNetwork/tip", Some(1)).unwrap();
        assert_eq!(tip.method, Method::QueryNetworkTip);
        let epoch = reply(&mut correlator, "queryLedgerState/epoch", Some(1)).unwrap();
        assert_eq!(epoch.method, Method::QueryEpoch);
        let block = reply(&mut correlator, "nextBlock", Some(1)).unwrap();
        assert_eq!(block.method, Method::NextBlock);
    }

    #[test]
    fn an_identified_response_without_a_method_searches_every_queue() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        correlator.push(&request("nextBlock", Some(1)), now);
        correlator.push(&request("submitTransaction", Some(2)), now);

        let submitted = correlator.take(RequestId::Int(2), None).unwrap();
        assert_eq!(submitted.method, Method::SubmitTransaction);
        assert_eq!(correlator.len(), 1);
    }

    #[test]
    fn absent_ids_fall_back_to_the_queue_front() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        correlator.push(&request("acquireMempool", None), now);
        correlator.push(&request("nextTransaction", None), now);

        let acquired = reply(&mut correlator, "acquireMempool", None).unwrap();
        assert_eq!(acquired.method, Method::AcquireMempool);
        assert_eq!(acquired.id, RequestId::Absent);
        // Same queue, so its front is the next answer whatever method echoes.
        let next = reply(&mut correlator, "hasTransaction", None).unwrap();
        assert_eq!(next.method, Method::NextTransaction);
    }

    #[test]
    fn an_oversize_request_matches_its_identified_response_by_queue() {
        let mut correlator = Correlator::default();
        let oversize = Classification {
            method: Method::EvaluateTransaction,
            shape: Shape::Oversize,
            id: RequestId::Other,
            bytes: 70_000,
        };
        correlator.push(&oversize, Instant::now());

        let evaluated = reply(&mut correlator, "evaluateTransaction", Some(5)).unwrap();
        assert_eq!(evaluated.shape, Shape::Oversize);
    }

    #[test]
    fn an_identified_response_does_not_pop_another_identified_entry() {
        let mut correlator = Correlator::default();
        correlator.push(&request("queryNetwork/tip", Some(1)), Instant::now());

        assert_eq!(reply(&mut correlator, "queryNetwork/tip", Some(2)), None);
        assert_eq!(correlator.len(), 1);
    }

    #[test]
    fn an_out_of_queue_error_is_unmatched_and_leaves_the_queues_intact() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        let invalid = classify(br#"{"jsonrpc":"2.0","method":"nextBlock","id":3"#);
        assert_eq!(invalid.method, Method::Invalid);
        assert_eq!(correlator.push(&invalid, now), Push::Ignored);
        correlator.push(&request("nextBlock", None), now);
        correlator.push(&request("queryNetwork/tip", Some(4)), now);

        let error = response::scan(
            br#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Invalid JSON"},"id":null}"#,
        );
        assert_eq!(correlator.take(error.id, error.method), None);
        let identified_error =
            response::scan(br#"{"jsonrpc":"2.0","error":{"code":-32600},"id":3}"#);
        assert_eq!(
            correlator.take(identified_error.id, identified_error.method),
            None
        );
        assert_eq!(correlator.len(), 2);
    }

    #[test]
    fn unknown_invalid_and_binary_messages_are_not_tracked() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        for method in [Method::Unknown, Method::Invalid, Method::Binary] {
            assert_eq!(
                correlator.push(&Classification::synthetic(method, 10), now),
                Push::Ignored
            );
        }
        assert_eq!(correlator.len(), 0);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        for i in 0..QUEUE_CAPACITY as i64 {
            assert_eq!(
                correlator.push(&request("nextBlock", Some(i)), now),
                Push::Tracked { evicted: None }
            );
        }
        // Other queues are bounded on their own.
        correlator.push(&request("queryNetwork/tip", Some(-1)), now);

        let Push::Tracked {
            evicted: Some(evicted),
        } = correlator.push(&request("nextBlock", Some(QUEUE_CAPACITY as i64)), now)
        else {
            panic!("the oldest entry is evicted");
        };
        assert_eq!(evicted.id, RequestId::Int(0));
        assert_eq!(correlator.len(), QUEUE_CAPACITY + 1);
        assert_eq!(reply(&mut correlator, "nextBlock", Some(0)), None);
        assert_eq!(
            reply(&mut correlator, "nextBlock", Some(1)).unwrap().id,
            RequestId::Int(1)
        );
    }

    #[test]
    fn abandonment_when_the_session_closes() {
        let mut correlator = Correlator::default();
        let now = Instant::now();
        correlator.push(&request("nextBlock", Some(1)), now);
        correlator.push(&request("acquireMempool", None), now);
        correlator.push(&request("submitTransaction", Some(2)), now);
        reply(&mut correlator, "submitTransaction", Some(2)).unwrap();

        let mut abandoned: Vec<Method> = correlator.drain().map(|p| p.method).collect();
        abandoned.sort_by_key(|method| method.as_str());
        assert_eq!(abandoned, [Method::AcquireMempool, Method::NextBlock]);
        assert_eq!(correlator.len(), 0);
    }
}
