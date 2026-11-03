use futures_util::future::select;
use futures_util::SinkExt;
use futures_util::StreamExt;
use futures_util::TryStreamExt;
use http_body_util::{combinators::BoxBody, BodyExt};
use hyper::body::Incoming;
use hyper::client::conn::http1 as http1_client;
use hyper::header::{
    HeaderValue, CONNECTION, HOST, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE,
};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::error::Error;
use std::fmt::Display;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use std::{fs, io};
use tokio::net::{TcpListener, TcpStream};
use tokio::pin;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, WebSocketStream};
use tracing::{error, info};
use url::Url;

use crate::body::{buffer_body, BodyEnd, ObservedBody, ReplayBody};
use crate::config::Config;
use crate::limiter::limiter;
use crate::metrics::{RpcCall, RpcScope};
use crate::rpc::{self, Classification, Correlator, Push, Transport, OVERSIZE_BYTES};
use crate::utils::{full, get_header, ProxyResponse, DMTR_API_KEY};
use crate::{Consumer, State};

fn add_cors_headers<B>(response: &mut Response<B>, config: &Config) {
    let headers = response.headers_mut();
    headers.insert(
        "Access-Control-Allow-Origin",
        HeaderValue::from_str(&config.cors_allow_origin).unwrap(),
    );
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_str(&config.cors_allow_methods).unwrap(),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_str(&config.cors_allow_headers).unwrap(),
    );
    headers.insert(
        "Access-Control-Max-Age",
        HeaderValue::from_str(&config.cors_max_age).unwrap(),
    );
}

pub async fn start(state: Arc<State>) {
    let addr_result = SocketAddr::from_str(&state.config.proxy_addr);
    if let Err(err) = addr_result {
        error!(error = err.to_string(), "invalid proxy addr");
        std::process::exit(1);
    }
    let addr = addr_result.unwrap();

    let listener_result = TcpListener::bind(addr).await;
    if let Err(err) = listener_result {
        error!(error = err.to_string(), "fail to bind tcp server listener");
        std::process::exit(1);
    }
    let listener = listener_result.unwrap();

    let tls_acceptor_result = build_tls_acceptor(&state);
    if let Err(err) = tls_acceptor_result {
        error!(error = err.to_string(), "fail to load tls");
        std::process::exit(1);
    }
    let tls_acceptor = tls_acceptor_result.unwrap();

    info!(addr = state.config.proxy_addr, "proxy listening");

    loop {
        let state = state.clone();
        let accept_result = listener.accept().await;
        if let Err(err) = accept_result {
            error!(error = err.to_string(), "fail to accept client");
            continue;
        }
        let (stream, _) = accept_result.unwrap();

        let tls_acceptor = tls_acceptor.clone();

        tokio::spawn(async move {
            let tls_stream = match tls_acceptor.accept(stream).await {
                Ok(tls_stream) => tls_stream,
                Err(err) => {
                    error!(error = err.to_string(), "failed to perform tls handshake");
                    return;
                }
            };

            let io = TokioIo::new(tls_stream);

            let service = service_fn(move |req| handle(req, state.clone()));

            if let Err(err) = Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await
            {
                error!(error = err.to_string(), "failed proxy server connection");
            }
        });
    }
}

async fn handle(
    mut hyper_req: Request<Incoming>,
    state: Arc<State>,
) -> Result<ProxyResponse, hyper::Error> {
    match (hyper_req.method(), hyper_req.uri().path()) {
        (&Method::OPTIONS, _) => {
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .body(full(""))
                .unwrap();
            add_cors_headers(&mut response, &state.config);
            Ok(response)
        }
        (&Method::GET, "/healthz") => {
            let mut response = handle_healthz(&state).await?;
            add_cors_headers(&mut response, &state.config);
            Ok(response)
        }
        _ => {
            let proxy_req_result = ProxyRequest::new(&mut hyper_req, &state).await;
            if proxy_req_result.is_none() {
                let mut response = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(full("Unauthorized"))
                    .unwrap();
                add_cors_headers(&mut response, &state.config);
                return Ok(response);
            }

            let proxy_req = proxy_req_result.unwrap();
            let response_result = match proxy_req.protocol {
                Protocol::Http => handle_http(hyper_req, &proxy_req, &state).await,
                Protocol::Websocket => {
                    // Before handling the websocket connection, check if consumer has available
                    // connections.
                    let tiers = state.tiers.read().await.clone();
                    match tiers.get(&proxy_req.consumer.tier) {
                        Some(tier) => {
                            if proxy_req.consumer.active_connections >= tier.max_connections {
                                let mut response = Response::builder()
                                    .status(StatusCode::TOO_MANY_REQUESTS)
                                    .body(full("Connection limit exceeded"))
                                    .unwrap();
                                add_cors_headers(&mut response, &state.config);
                                Ok(response)
                            } else {
                                handle_websocket(hyper_req, &proxy_req, state.clone()).await
                            }
                        }
                        None => {
                            let mut response = Response::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(full(
                                    "Invalid tier value. Contact support team for more information.",
                                ))
                                .unwrap();
                            add_cors_headers(&mut response, &state.config);
                            Ok(response)
                        }
                    }
                }
            };

            match &response_result {
                Ok(response) => {
                    state
                        .metrics
                        .count_http_total_request(&proxy_req, response.status());
                }
                Err(err) => {
                    error!(error = err.to_string(), "Failed to handle request");
                    todo!("send error to prometheus");
                }
            };

            response_result
        }
    }
}

async fn handle_http(
    hyper_req: Request<Incoming>,
    proxy_req: &ProxyRequest,
    state: &State,
) -> Result<ProxyResponse, hyper::Error> {
    if hyper_req.method() == Method::POST
        && state
            .config
            .rpc_telemetry_networks
            .enabled_for(&proxy_req.consumer.network)
    {
        let mut response = forward_classified(
            hyper_req,
            |classification| {
                let consumer = proxy_req.consumer.to_string();
                state.metrics.count_rpc_request(
                    proxy_req,
                    &consumer,
                    Transport::Http,
                    classification,
                );
                *classification
            },
            |req, classification| {
                let scope = RpcScope::new(&state.metrics, proxy_req, Transport::Http);
                send_upstream(req, &proxy_req.instance, scope, classification)
            },
        )
        .await;
        add_cors_headers(&mut response, &state.config);
        return Ok(response);
    }

    let stream = TcpStream::connect(&proxy_req.instance).await.unwrap();
    let io: TokioIo<TcpStream> = TokioIo::new(stream);

    let (mut sender, conn) = http1_client::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await?;

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
    });

    let mut resp = sender.send_request(hyper_req).await?;
    add_cors_headers(&mut resp, &state.config);
    Ok(resp.map(|b| b.boxed()))
}

/// Buffers a POST body far enough to classify it, counts it, and forwards the
/// identical bytes with the headers as received. A client that aborts before
/// the body is classified gets a `400` and the upstream is never contacted.
///
/// Whatever `count` returns is handed to `upstream` with the request.
async fn forward_classified<B, T, F, Fut>(
    hyper_req: Request<B>,
    count: impl FnOnce(&Classification) -> T,
    upstream: F,
) -> ProxyResponse
where
    B: hyper::body::Body<Data = bytes::Bytes> + Unpin,
    B::Error: Display,
    F: FnOnce(Request<ReplayBody<B>>, T) -> Fut,
    Fut: Future<Output = ProxyResponse>,
{
    let (parts, body) = hyper_req.into_parts();
    let (prefix, body) = match buffer_body(body, OVERSIZE_BYTES).await {
        Ok(buffered) => buffered,
        Err(err) => {
            error!(error = err.to_string(), "client aborted the request body");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(full("Incomplete request body"))
                .unwrap();
        }
    };

    let counted = count(&rpc::classify(&prefix));

    upstream(Request::from_parts(parts, body), counted).await
}

/// Sends a request to the instance and answers any failure itself, so that
/// the classified path never surfaces an error to `handle`.
///
/// The request is timed from just before it is sent to the response headers,
/// and its response body is counted on the way to the client.
async fn send_upstream(
    req: Request<ReplayBody<Incoming>>,
    instance: &str,
    scope: RpcScope,
    classification: Classification,
) -> ProxyResponse {
    let failure = |status: StatusCode, err: &dyn Display| {
        error!(
            error = err.to_string(),
            "fail to forward request to instance"
        );
        Response::builder()
            .status(status)
            .body(full(status.canonical_reason().unwrap_or_default()))
            .unwrap()
    };

    let stream = match TcpStream::connect(instance).await {
        Ok(stream) => stream,
        Err(err) => return failure(StatusCode::BAD_GATEWAY, &err),
    };
    let io: TokioIo<TcpStream> = TokioIo::new(stream);

    let (mut sender, conn) = match http1_client::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await
    {
        Ok(handshake) => handshake,
        Err(err) => return failure(StatusCode::BAD_GATEWAY, &err),
    };

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
    });

    // Dropping the call unfinished counts it as abandoned, wherever the
    // client goes away.
    let call = RpcCall::start(scope, &classification);
    let sent_at = Instant::now();
    match sender.send_request(req).await {
        Ok(resp) => {
            let duration = sent_at.elapsed();
            resp.map(|body| {
                ObservedBody::new(body, move |end| match end {
                    BodyEnd::Complete { bytes, error } => call.answered(duration, bytes, error),
                    BodyEnd::Abandoned => drop(call),
                    BodyEnd::Failed => call.released(),
                })
                .boxed()
            })
        }
        // A user error here is the client's body failing mid-stream.
        Err(err) if err.is_user() => {
            drop(call);
            failure(StatusCode::BAD_REQUEST, &err)
        }
        Err(err) => {
            call.released();
            failure(StatusCode::BAD_GATEWAY, &err)
        }
    }
}

async fn handle_websocket(
    mut hyper_req: Request<Incoming>,
    proxy_req: &ProxyRequest,
    state: Arc<State>,
) -> Result<ProxyResponse, hyper::Error> {
    let headers = hyper_req.headers();
    let upgrade = HeaderValue::from_static("Upgrade");
    let websocket = HeaderValue::from_static("websocket");
    let key = headers.get(SEC_WEBSOCKET_KEY);
    let derived = key.map(|k| derive_accept_key(k.as_bytes()));
    let version = hyper_req.version();

    let proxy_req = proxy_req.clone();
    let config = state.config.clone();

    tokio::task::spawn(async move {
        match hyper::upgrade::on(&mut hyper_req).await {
            Ok(upgraded) => {
                let upgraded = TokioIo::new(upgraded);
                let client_stream =
                    WebSocketStream::from_raw_socket(upgraded, Role::Server, None).await;
                let (client_outgoing, mut client_incoming) = client_stream.split();

                let url =
                    Url::parse(&format!("ws://{}{}", proxy_req.instance, hyper_req.uri())).unwrap();
                let connection_result = connect_async(url).await;
                if let Err(err) = connection_result {
                    error!(error = err.to_string(), "fail to connect to the instance");
                    return;
                }
                let (instance_stream, _) = connection_result.unwrap();
                let (mut instance_outgoing, instance_incoming) = instance_stream.split();

                state.metrics.inc_ws_total_connection(&proxy_req);
                proxy_req.consumer.inc_connections(state.clone()).await;

                let active_connections = proxy_req
                    .consumer
                    .get_active_connections(state.clone())
                    .await;
                info!(
                    consumer = proxy_req.consumer.to_string(),
                    active_connections, "client connected"
                );

                // Decided once per connection; the disabled path is this one
                // branch per message.
                let telemetry = state
                    .config
                    .rpc_telemetry_networks
                    .enabled_for(&proxy_req.consumer.network)
                    .then(|| WsTelemetry::new(&state, &proxy_req));

                let client_in = async {
                    while let Some(result) = client_incoming.next().await {
                        match result {
                            Ok(data) => {
                                // Counted before the limiter so that stalled
                                // messages still show as demand.
                                let classification = telemetry.as_ref().and_then(|telemetry| {
                                    telemetry.received(&state, &proxy_req, &data)
                                });
                                if let Err(err) = limiter(state.clone(), &proxy_req.consumer).await
                                {
                                    error!(error = err.to_string(), "Failed to run limiter.");
                                    break;
                                };
                                // Tracked before the send, so that a fast
                                // response cannot overtake its own entry.
                                if let (Some(telemetry), Some(classification)) =
                                    (&telemetry, &classification)
                                {
                                    telemetry.forwarding(classification);
                                }
                                if let Err(err) = instance_outgoing.send(data).await {
                                    error!(
                                        error = err.to_string(),
                                        "fail to send data to instance"
                                    );
                                    break;
                                }
                            }
                            Err(err) => {
                                error!(error = err.to_string(), "stream client incoming");
                                break;
                            }
                        }
                    }
                };
                pin!(client_in);

                let instance_in = instance_incoming
                    .inspect_ok(|message| {
                        state.metrics.count_ws_total_frame(&proxy_req);
                        if let Some(telemetry) = &telemetry {
                            telemetry.answered(message);
                        }
                    })
                    .forward(client_outgoing);

                select(client_in, instance_in).await;

                if let Some(telemetry) = &telemetry {
                    telemetry.closed(&state, &proxy_req);
                }

                state.metrics.dec_ws_total_connection(&proxy_req);
                proxy_req.consumer.dec_connections(state.clone()).await;

                let active_connections = proxy_req
                    .consumer
                    .get_active_connections(state.clone())
                    .await;
                info!(
                    consumer = proxy_req.consumer.to_string(),
                    active_connections, "client disconnected"
                );
            }
            Err(err) => {
                error!(error = err.to_string(), "upgrade error");
            }
        }
    });

    let mut res = Response::new(BoxBody::default());
    *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    *res.version_mut() = version;
    res.headers_mut().append(CONNECTION, upgrade);
    res.headers_mut().append(UPGRADE, websocket);
    res.headers_mut()
        .append(SEC_WEBSOCKET_ACCEPT, derived.unwrap().parse().unwrap());
    add_cors_headers(&mut res, &config);

    Ok(res)
}

/// Request telemetry for one WebSocket connection: what is in flight, and the
/// session's statistics.
struct WsTelemetry {
    consumer: String,
    scope: RpcScope,
    /// Never held across an `.await`.
    correlator: Mutex<Correlator>,
    requests: AtomicU64,
    connected_at: Instant,
}

impl WsTelemetry {
    fn new(state: &State, proxy_req: &ProxyRequest) -> Self {
        Self {
            consumer: proxy_req.consumer.to_string(),
            scope: RpcScope::new(&state.metrics, proxy_req, Transport::Websocket),
            correlator: Mutex::default(),
            requests: AtomicU64::new(0),
            connected_at: Instant::now(),
        }
    }

    fn correlator(&self) -> std::sync::MutexGuard<'_, Correlator> {
        self.correlator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Classifies and counts a client message; control frames are not
    /// requests.
    fn received(
        &self,
        state: &State,
        proxy_req: &ProxyRequest,
        message: &Message,
    ) -> Option<Classification> {
        let classification = rpc::classify_message(message)?;
        state.metrics.count_rpc_request(
            proxy_req,
            &self.consumer,
            Transport::Websocket,
            &classification,
        );
        self.requests.fetch_add(1, Ordering::Relaxed);
        Some(classification)
    }

    fn forwarding(&self, classification: &Classification) {
        let push = self.correlator().push(classification, Instant::now());
        if let Push::Tracked { evicted } = push {
            self.scope.sent();
            if evicted.is_some() {
                self.scope.evicted();
            }
        }
    }

    fn answered(&self, message: &Message) {
        // Ogmios answers in text frames only.
        let Message::Text(text) = message else {
            return;
        };
        let response = rpc::scan_response(text.as_bytes());
        let pending = self.correlator().take(response.id, response.method);
        match pending {
            Some(pending) => self.scope.answered(
                pending.method,
                pending.shape,
                pending.sent_at.elapsed(),
                text.len(),
                response.error,
            ),
            None => self
                .scope
                .unmatched_response(response.method, response.error),
        }
    }

    fn closed(&self, state: &State, proxy_req: &ProxyRequest) {
        let abandoned: Vec<_> = self.correlator().drain().collect();
        for pending in abandoned {
            self.scope.abandoned(pending.method, pending.shape);
        }
        state.metrics.observe_ws_session(
            proxy_req,
            self.connected_at.elapsed(),
            self.requests.load(Ordering::Relaxed),
        );
    }
}

async fn handle_healthz(state: &State) -> Result<ProxyResponse, hyper::Error> {
    if *state.upstream_health.read().await {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(full("OK"))
            .unwrap())
    } else {
        Ok(Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(full(""))
            .unwrap())
    }
}

#[derive(Debug, Clone)]
pub enum Protocol {
    Http,
    Websocket,
}
impl Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Http => write!(f, "http"),
            Protocol::Websocket => write!(f, "websocket"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProxyRequest {
    pub namespace: String,
    pub host: String,
    pub instance: String,
    pub consumer: Consumer,
    pub protocol: Protocol,
}
impl ProxyRequest {
    pub async fn new(hyper_req: &mut Request<Incoming>, state: &State) -> Option<Self> {
        let namespace = state.config.proxy_namespace.clone();

        let protocol = get_header(hyper_req, UPGRADE.as_str())
            .map(|h| {
                if h.eq_ignore_ascii_case("websocket") {
                    return Protocol::Websocket;
                }

                Protocol::Http
            })
            .unwrap_or(Protocol::Http);

        let host = get_header(hyper_req, HOST.as_str())?;
        let captures = state.host_regex.captures(&host)?;

        let token = get_header(hyper_req, DMTR_API_KEY)
            .or_else(|| captures.get(1).map(|v| v.as_str().to_string()))
            .unwrap_or_default();

        let consumer = state.get_consumer(&token).await?;
        let instance = state.config.instance(&consumer.network, &consumer.version);

        Some(Self {
            namespace,
            instance,
            consumer,
            protocol,
            host,
        })
    }
}

fn build_tls_acceptor(state: &State) -> Result<TlsAcceptor, Box<dyn Error>> {
    let certs = load_certs(&state.config.ssl_crt_path)?;

    let key = load_private_key(&state.config.ssl_key_path)?;

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();

    let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
    Ok(tls_acceptor)
}

fn load_certs(path: &PathBuf) -> io::Result<Vec<CertificateDer<'static>>> {
    let cert_file = fs::File::open(path)?;
    let mut reader = io::BufReader::new(cert_file);
    rustls_pemfile::certs(&mut reader).collect()
}

fn load_private_key(path: &PathBuf) -> io::Result<PrivateKeyDer<'static>> {
    let key_file = fs::File::open(path)?;
    let mut reader = io::BufReader::new(key_file);
    rustls_pemfile::private_key(&mut reader).map(|key| key.unwrap())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use bytes::Bytes;
    use futures_util::stream;
    use http_body_util::StreamBody;
    use hyper::body::Frame;

    use super::*;

    type Chunk = Result<Frame<Bytes>, &'static str>;

    fn post(chunks: Vec<Chunk>) -> Request<StreamBody<stream::Iter<std::vec::IntoIter<Chunk>>>> {
        Request::post("/")
            .header("content-type", "application/json")
            .body(StreamBody::new(stream::iter(chunks)))
            .unwrap()
    }

    fn data(bytes: &'static [u8]) -> Chunk {
        Ok(Frame::data(Bytes::from_static(bytes)))
    }

    #[tokio::test]
    async fn a_client_abort_is_answered_without_contacting_the_upstream() {
        let counted = Cell::new(false);
        let contacted = Cell::new(false);

        let response = forward_classified(
            post(vec![data(b"{\"method\":"), Err("connection reset")]),
            |_| counted.set(true),
            |_, _| {
                contacted.set(true);
                async { Response::new(full("")) }
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!counted.get());
        assert!(!contacted.get());
    }

    #[tokio::test]
    async fn a_classified_post_is_forwarded_unchanged() {
        let mut counted = None;

        let response = forward_classified(
            post(vec![
                data(b"{\"jsonrpc\":\"2.0\",\"method\":"),
                data(b"\"queryNetwork/tip\"}"),
            ]),
            |classification| counted = Some(*classification),
            |req, _| async move {
                assert_eq!(req.headers()["content-type"], "application/json");
                let body = req.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(
                    &body[..],
                    b"{\"jsonrpc\":\"2.0\",\"method\":\"queryNetwork/tip\"}"
                );
                Response::new(full("ok"))
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let counted = counted.unwrap();
        assert_eq!(counted.method, rpc::Method::QueryNetworkTip);
        assert_eq!(
            counted.bytes,
            r#"{"jsonrpc":"2.0","method":"queryNetwork/tip"}"#.len()
        );
    }
}
