//! Buffering an HTTP request body far enough to classify it, then replaying
//! it to the upstream byte for byte; and counting a response body on its way
//! to the client.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};
use hyper::HeaderMap;

use crate::rpc::{scan_head, ErrorCode};

/// Reads `body` until it ends or the buffered bytes exceed `cap`, and returns
/// the buffered prefix together with a body that replays the prefix followed
/// by whatever remains of `body`, unchanged.
///
/// The prefix holds every byte when the body ended within the cap, and more
/// than `cap` bytes otherwise; it can exceed `cap` by up to one frame. An
/// error means the client aborted before the body was complete.
pub async fn buffer_body<B>(mut body: B, cap: usize) -> Result<(Bytes, ReplayBody<B>), B::Error>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut buffer = BytesMut::new();
    let mut trailers = None;
    let mut ended = true;

    while let Some(frame) = body.frame().await {
        match frame?.into_data() {
            Ok(data) => buffer.extend_from_slice(&data),
            Err(frame) => {
                // Trailers are the last frame of a body.
                trailers = frame.into_trailers().ok();
                break;
            }
        }
        if buffer.len() > cap {
            ended = false;
            break;
        }
    }

    let prefix = buffer.freeze();
    let replay = ReplayBody {
        prefix: (!prefix.is_empty()).then(|| prefix.clone()),
        rest: (!ended).then_some(body),
        trailers,
    };
    Ok((prefix, replay))
}

/// A request body that yields a buffered prefix, then the rest of the original
/// body, then any trailers already read.
#[derive(Debug)]
pub struct ReplayBody<B> {
    prefix: Option<Bytes>,
    rest: Option<B>,
    trailers: Option<HeaderMap>,
}

impl<B> Body for ReplayBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        if let Some(prefix) = self.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        if let Some(rest) = self.rest.as_mut() {
            match Pin::new(rest).poll_frame(cx) {
                Poll::Ready(None) => self.rest = None,
                other => return other,
            }
        }
        Poll::Ready(
            self.trailers
                .take()
                .map(|trailers| Ok(Frame::trailers(trailers))),
        )
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_none()
            && self.trailers.is_none()
            && match &self.rest {
                Some(rest) => rest.is_end_stream(),
                None => true,
            }
    }

    fn size_hint(&self) -> SizeHint {
        let prefix = self.prefix.as_ref().map_or(0, |prefix| prefix.len() as u64);
        let rest = self
            .rest
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), |rest| rest.size_hint());

        let mut hint = SizeHint::new();
        hint.set_lower(rest.lower() + prefix);
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper + prefix);
        }
        hint
    }
}

/// How an observed response body ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEnd {
    /// Every byte reached the client. `error` is read from the head of the
    /// first data frame.
    Complete {
        bytes: usize,
        error: Option<ErrorCode>,
    },
    /// The body was dropped before its end: the client went away.
    Abandoned,
    /// The upstream failed mid-body.
    Failed,
}

type OnEnd = Box<dyn FnOnce(BodyEnd) + Send + Sync>;

/// A response body passed through unchanged, which reports how it ended
/// exactly once: at its end, on an error, or when dropped.
pub struct ObservedBody<B: Body> {
    inner: B,
    bytes: usize,
    error: Option<ErrorCode>,
    first_frame: bool,
    on_end: Option<OnEnd>,
}

impl<B: Body<Data = Bytes>> ObservedBody<B> {
    pub fn new(inner: B, on_end: impl FnOnce(BodyEnd) + Send + Sync + 'static) -> Self {
        Self {
            inner,
            bytes: 0,
            error: None,
            first_frame: true,
            on_end: Some(Box::new(on_end)),
        }
    }

    fn end(&mut self, end: BodyEnd) {
        if let Some(on_end) = self.on_end.take() {
            on_end(end);
        }
    }

    fn complete(&mut self) {
        let end = BodyEnd::Complete {
            bytes: self.bytes,
            error: self.error,
        };
        self.end(end);
    }
}

impl<B> Body for ObservedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if std::mem::take(&mut this.first_frame) {
                        this.error = scan_head(data).error;
                    }
                    this.bytes += data.len();
                }
                // hyper stops polling a body that says it has ended.
                if this.inner.is_end_stream() {
                    this.complete();
                }
            }
            Poll::Ready(Some(Err(_))) => this.end(BodyEnd::Failed),
            Poll::Ready(None) => this.complete(),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B: Body> Drop for ObservedBody<B> {
    fn drop(&mut self) {
        let Some(on_end) = self.on_end.take() else {
            return;
        };
        // An empty body may never be polled at all.
        on_end(if self.inner.is_end_stream() {
            BodyEnd::Complete {
                bytes: self.bytes,
                error: self.error,
            }
        } else {
            BodyEnd::Abandoned
        });
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use futures_util::stream;
    use http_body_util::StreamBody;

    use super::*;

    const CAP: usize = 64 * 1024;

    type Chunk = Result<Frame<Bytes>, &'static str>;

    fn stub(chunks: Vec<Chunk>) -> StreamBody<stream::Iter<std::vec::IntoIter<Chunk>>> {
        StreamBody::new(stream::iter(chunks))
    }

    /// Splits `data` into frames of `frame` bytes.
    fn frames(data: &[u8], frame: usize) -> Vec<Chunk> {
        data.chunks(frame)
            .map(|chunk| Ok(Frame::data(Bytes::copy_from_slice(chunk))))
            .collect()
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    async fn replayed<B>(body: B) -> Vec<u8>
    where
        B: Body<Data = Bytes>,
        B::Error: std::fmt::Debug,
    {
        body.collect().await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_buffered_whole_and_replayed() {
        for len in [0, 1, 4096, CAP - 1, CAP] {
            let data = payload(len);
            let (prefix, replay) = buffer_body(stub(frames(&data, 1000)), CAP).await.unwrap();
            assert_eq!(&prefix[..], &data[..], "len {len}");
            assert_eq!(replay.size_hint().exact(), Some(len as u64));
            assert_eq!(replayed(replay).await, data, "len {len}");
        }
    }

    #[tokio::test]
    async fn a_body_over_the_cap_replays_the_prefix_then_the_rest() {
        for (len, frame) in [(CAP + 1, 1000), (200 * 1024, 1000), (200 * 1024, 70_000)] {
            let data = payload(len);
            let (prefix, replay) = buffer_body(stub(frames(&data, frame)), CAP).await.unwrap();
            assert!(prefix.len() > CAP);
            assert!(prefix.len() <= CAP + frame);
            assert_eq!(&prefix[..], &data[..prefix.len()]);
            assert_eq!(replayed(replay).await, data, "len {len} frame {frame}");
        }
    }

    #[tokio::test]
    async fn trailers_are_replayed_after_the_data() {
        let mut trailers = HeaderMap::new();
        trailers.insert("x-check", "1".parse().unwrap());
        let chunks = vec![
            Ok(Frame::data(Bytes::from_static(b"{\"method\""))),
            Ok(Frame::data(Bytes::from_static(b":\"nextBlock\"}"))),
            Ok(Frame::trailers(trailers.clone())),
        ];

        let (prefix, replay) = buffer_body(stub(chunks), CAP).await.unwrap();
        assert_eq!(&prefix[..], b"{\"method\":\"nextBlock\"}");
        let collected = replay.collect().await.unwrap();
        assert_eq!(collected.trailers(), Some(&trailers));
        assert_eq!(&collected.to_bytes()[..], b"{\"method\":\"nextBlock\"}");
    }

    #[tokio::test]
    async fn a_client_abort_is_an_error() {
        let mut chunks = frames(&payload(10_000), 1000);
        chunks.push(Err("connection reset"));
        assert_eq!(
            buffer_body(stub(chunks), CAP).await.unwrap_err(),
            "connection reset"
        );
    }

    #[tokio::test]
    async fn an_abort_after_the_cap_reaches_the_replay() {
        let mut chunks = frames(&payload(CAP + 2000), 1000);
        chunks.push(Err("connection reset"));
        let (_, replay) = buffer_body(stub(chunks), CAP).await.unwrap();
        assert_eq!(replay.collect().await.unwrap_err(), "connection reset");
    }

    #[tokio::test]
    async fn an_empty_body_is_at_its_end() {
        let empty =
            http_body_util::Empty::<Bytes>::new().map_err(|never: Infallible| match never {});
        let (prefix, replay) = buffer_body(empty, CAP).await.unwrap();
        assert!(prefix.is_empty());
        assert!(replay.is_end_stream());
    }

    fn observed<B: Body<Data = Bytes>>(body: B) -> (ObservedBody<B>, Arc<Mutex<Vec<BodyEnd>>>) {
        let ends = Arc::new(Mutex::new(Vec::new()));
        let sink = ends.clone();
        let body = ObservedBody::new(body, move |end| sink.lock().unwrap().push(end));
        (body, ends)
    }

    #[tokio::test]
    async fn counted_bytes_equal_the_body_length() {
        for (len, frame) in [(0, 1), (1, 1), (10_000, 1000), (200 * 1024, 70_000)] {
            let data = payload(len);
            let (body, ends) = observed(stub(frames(&data, frame)));
            assert_eq!(replayed(body).await, data);
            assert_eq!(
                *ends.lock().unwrap(),
                [BodyEnd::Complete {
                    bytes: len,
                    error: None
                }],
                "len {len}"
            );
        }
    }

    #[tokio::test]
    async fn a_body_that_says_it_ended_completes_without_a_final_poll() {
        let full = http_body_util::Full::new(Bytes::from_static(b"{\"result\":1,\"id\":1}"))
            .map_err(|never: Infallible| match never {});
        let (mut body, ends) = observed(full);
        assert!(body.frame().await.is_some());
        assert_eq!(
            *ends.lock().unwrap(),
            [BodyEnd::Complete {
                bytes: br#"{"result":1,"id":1}"#.len(),
                error: None
            }]
        );
        drop(body);
        assert_eq!(ends.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_error_code_is_read_from_the_first_frame() {
        let chunks = vec![
            data(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"queryNetwork/tip\",\"error\":{\"code\":-32602,",
            ),
            data(b"\"message\":\"bad\"},\"id\":null}"),
        ];
        let (body, ends) = observed(stub(chunks));
        let bytes = replayed(body).await.len();
        assert_eq!(
            *ends.lock().unwrap(),
            [BodyEnd::Complete {
                bytes,
                error: Some(ErrorCode(Some(-32602)))
            }]
        );
    }

    #[tokio::test]
    async fn an_error_code_past_the_first_frame_is_not_read() {
        let chunks = vec![
            data(b"{\"jsonrpc\":\"2.0\","),
            data(b"\"error\":{\"code\":-32602}}"),
        ];
        let (body, ends) = observed(stub(chunks));
        replayed(body).await;
        assert!(matches!(
            ends.lock().unwrap()[..],
            [BodyEnd::Complete { error: None, .. }]
        ));
    }

    #[tokio::test]
    async fn a_drop_before_the_end_is_abandoned() {
        let (mut body, ends) = observed(stub(frames(&payload(10_000), 1000)));
        body.frame().await.unwrap().unwrap();
        assert!(ends.lock().unwrap().is_empty());
        drop(body);
        assert_eq!(*ends.lock().unwrap(), [BodyEnd::Abandoned]);
    }

    #[tokio::test]
    async fn an_upstream_error_mid_body_is_failed() {
        let mut chunks = frames(&payload(3000), 1000);
        chunks.push(Err("connection reset"));
        let (body, ends) = observed(stub(chunks));
        assert!(body.collect().await.is_err());
        assert_eq!(*ends.lock().unwrap(), [BodyEnd::Failed]);
    }

    fn data(bytes: &'static [u8]) -> Chunk {
        Ok(Frame::data(Bytes::from_static(bytes)))
    }
}
