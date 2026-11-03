//! Request classification: maps each client JSON-RPC message to a bounded
//! method and shape vocabulary. The vocabulary is specified by the
//! `ogmios-request-classification` contract.
//!
//! Response scanning and correlation reuse that vocabulary to time each
//! request and size its answer.

mod classify;
mod correlate;
mod response;
mod vocabulary;

#[cfg(test)]
mod tests;

use tokio_tungstenite::tungstenite::Message;

#[allow(unused_imports)]
pub use classify::{classify, is_heavy, Classification, RequestId, OVERSIZE_BYTES};
#[allow(unused_imports)]
pub use correlate::{Correlator, Pending, Push, QUEUE_CAPACITY};
#[allow(unused_imports)]
pub use response::{scan as scan_response, scan_head, scan_id, ErrorCode, Head, Response};
#[allow(unused_imports)]
pub use vocabulary::{CountBucket, Family, Method, Queue, Shape, SizeBucket, Transport};

/// Classifies a client WebSocket message. Text frames are classified, binary
/// frames are `binary`, and control frames are not requests at all.
pub fn classify_message(message: &Message) -> Option<Classification> {
    match message {
        Message::Text(text) => Some(classify(text.as_bytes())),
        Message::Binary(bytes) => Some(Classification::synthetic(Method::Binary, bytes.len())),
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => None,
    }
}
