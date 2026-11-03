use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use std::{net::SocketAddr, str::FromStr};

use hyper::server::conn::http1 as http1_server;
use hyper::{body::Incoming, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use prometheus::{
    histogram_opts, opts, Encoder, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Registry,
    TextEncoder,
};
use tokio::net::TcpListener;
use tracing::{error, info, instrument};

use crate::proxy::ProxyRequest;
use crate::rpc::{self, is_heavy, Classification, ErrorCode, Shape, Transport};
use crate::utils::{full, ProxyResponse};
use crate::State;

#[derive(Debug, Clone)]
pub struct Metrics {
    registry: Registry,
    pub ws_total_frame: IntCounterVec,
    pub ws_total_connection: IntGaugeVec,
    pub http_total_request: IntCounterVec,
    pub rpc_requests: IntCounterVec,
    pub rpc_consumer_requests: IntCounterVec,
    pub rpc_consumer_heavy_requests: IntCounterVec,
    /// Time from forwarding a request to its response: after the limiter, to
    /// the response frame (WebSocket) or the response headers (HTTP).
    ///
    /// `nextBlock` at the tip, `acquireMempool` and `nextTransaction` wait for
    /// the chain or the mempool by design, so their durations measure that
    /// wait, not the instance's latency. Keep them apart from query latency.
    pub rpc_duration: HistogramVec,
    pub rpc_response_bytes: HistogramVec,
    pub rpc_errors: IntCounterVec,
    pub rpc_in_flight: IntGaugeVec,
    pub rpc_abandoned: IntCounterVec,
    pub rpc_unmatched: IntCounterVec,
    pub ws_session_duration: HistogramVec,
    pub ws_session_requests: HistogramVec,
}

impl Metrics {
    pub fn try_new(registry: Registry) -> Result<Self, Box<dyn Error>> {
        let ws_total_frame = IntCounterVec::new(
            opts!("ogmios_proxy_ws_total_frame", "total of websocket frame",),
            &["namespace", "instance", "route", "consumer", "tier"],
        )
        .unwrap();

        let ws_total_connection = IntGaugeVec::new(
            opts!(
                "ogmios_proxy_total_connections",
                "total of websocket connection",
            ),
            &["namespace", "instance", "route", "consumer", "tier"],
        )
        .unwrap();

        let http_total_request = IntCounterVec::new(
            opts!("ogmios_proxy_http_total_request", "total of http request",),
            &[
                "namespace",
                "instance",
                "route",
                "status_code",
                "protocol",
                "consumer",
                "tier",
            ],
        )
        .unwrap();

        // Request classification. These avoid the label names Prometheus
        // target labels already use (namespace, instance, job, pod, container,
        // endpoint), and only the low-cardinality families carry `consumer`.
        let rpc_requests = IntCounterVec::new(
            opts!(
                "ogmios_proxy_rpc_requests_total",
                "client JSON-RPC messages by method and shape",
            ),
            &[
                "network",
                "version",
                "tier",
                "transport",
                "method",
                "shape",
                "heavy",
            ],
        )?;

        let rpc_consumer_requests = IntCounterVec::new(
            opts!(
                "ogmios_proxy_rpc_consumer_requests_total",
                "client JSON-RPC messages by consumer and method family",
            ),
            &["consumer", "network", "version", "family"],
        )?;

        let rpc_consumer_heavy_requests = IntCounterVec::new(
            opts!(
                "ogmios_proxy_rpc_consumer_heavy_requests_total",
                "client JSON-RPC messages of a heavy class by consumer",
            ),
            &["consumer", "network", "version", "method", "shape"],
        )?;

        // Response correlation. Only the session histograms carry `tier`, and
        // none carries `consumer`.
        let rpc_labels = ["network", "version", "transport", "method", "shape"];
        let rpc_duration = HistogramVec::new(
            histogram_opts!(
                "ogmios_proxy_rpc_duration_seconds",
                "time from forwarding a request to its response; nextBlock at the tip, acquireMempool and nextTransaction include waiting for the chain or the mempool",
                vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
                    300.0, 1800.0,
                ]
            ),
            &rpc_labels,
        )?;

        const KI: f64 = 1024.0;
        const MI: f64 = 1024.0 * KI;
        let rpc_response_bytes = HistogramVec::new(
            histogram_opts!(
                "ogmios_proxy_rpc_response_bytes",
                "size of the instance's response to a request",
                vec![
                    256.0,
                    KI,
                    4.0 * KI,
                    16.0 * KI,
                    64.0 * KI,
                    256.0 * KI,
                    MI,
                    4.0 * MI,
                    16.0 * MI,
                    64.0 * MI,
                ]
            ),
            &rpc_labels,
        )?;

        let rpc_errors = IntCounterVec::new(
            opts!(
                "ogmios_proxy_rpc_errors_total",
                "JSON-RPC error responses by method and code",
            ),
            &["network", "version", "transport", "method", "code"],
        )?;

        let rpc_in_flight = IntGaugeVec::new(
            opts!(
                "ogmios_proxy_rpc_in_flight",
                "requests forwarded to the instance and not yet answered",
            ),
            &["network", "version", "transport"],
        )?;

        let rpc_abandoned = IntCounterVec::new(
            opts!(
                "ogmios_proxy_rpc_abandoned_total",
                "requests whose client went away before the answer came",
            ),
            &rpc_labels,
        )?;

        let rpc_unmatched = IntCounterVec::new(
            opts!(
                "ogmios_proxy_rpc_unmatched_total",
                "responses matched to no request, and requests evicted unanswered",
            ),
            &["transport", "reason"],
        )?;

        let ws_session_duration = HistogramVec::new(
            histogram_opts!(
                "ogmios_proxy_ws_session_duration_seconds",
                "WebSocket session duration",
                vec![1.0, 10.0, 60.0, 300.0, 1800.0, 3600.0, 21600.0, 86400.0]
            ),
            &["network", "version", "tier"],
        )?;

        let ws_session_requests = HistogramVec::new(
            histogram_opts!(
                "ogmios_proxy_ws_session_requests",
                "client messages per WebSocket session",
                vec![1.0, 10.0, 100.0, 1000.0, 10000.0, 100000.0, 1000000.0]
            ),
            &["network", "version", "tier"],
        )?;

        registry.register(Box::new(ws_total_frame.clone()))?;
        registry.register(Box::new(ws_total_connection.clone()))?;
        registry.register(Box::new(http_total_request.clone()))?;
        registry.register(Box::new(rpc_requests.clone()))?;
        registry.register(Box::new(rpc_consumer_requests.clone()))?;
        registry.register(Box::new(rpc_consumer_heavy_requests.clone()))?;
        registry.register(Box::new(rpc_duration.clone()))?;
        registry.register(Box::new(rpc_response_bytes.clone()))?;
        registry.register(Box::new(rpc_errors.clone()))?;
        registry.register(Box::new(rpc_in_flight.clone()))?;
        registry.register(Box::new(rpc_abandoned.clone()))?;
        registry.register(Box::new(rpc_unmatched.clone()))?;
        registry.register(Box::new(ws_session_duration.clone()))?;
        registry.register(Box::new(ws_session_requests.clone()))?;

        Ok(Metrics {
            registry,
            ws_total_frame,
            ws_total_connection,
            http_total_request,
            rpc_requests,
            rpc_consumer_requests,
            rpc_consumer_heavy_requests,
            rpc_duration,
            rpc_response_bytes,
            rpc_errors,
            rpc_in_flight,
            rpc_abandoned,
            rpc_unmatched,
            ws_session_duration,
            ws_session_requests,
        })
    }

    pub fn metrics_collected(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.registry.gather()
    }

    pub fn count_ws_total_frame(&self, proxy_req: &ProxyRequest) {
        self.ws_total_frame
            .with_label_values(&[
                &proxy_req.namespace,
                &proxy_req.instance,
                &proxy_req.host,
                &proxy_req.consumer.to_string(),
                &proxy_req.consumer.tier,
            ])
            .inc()
    }

    pub fn inc_ws_total_connection(&self, proxy_req: &ProxyRequest) {
        self.ws_total_connection
            .with_label_values(&[
                &proxy_req.namespace,
                &proxy_req.instance,
                &proxy_req.host,
                &proxy_req.consumer.to_string(),
                &proxy_req.consumer.tier,
            ])
            .inc()
    }

    pub fn dec_ws_total_connection(&self, proxy_req: &ProxyRequest) {
        self.ws_total_connection
            .with_label_values(&[
                &proxy_req.namespace,
                &proxy_req.instance,
                &proxy_req.host,
                &proxy_req.consumer.to_string(),
                &proxy_req.consumer.tier,
            ])
            .dec()
    }

    pub fn count_http_total_request(&self, proxy_req: &ProxyRequest, status_code: StatusCode) {
        self.http_total_request
            .with_label_values(&[
                &proxy_req.namespace,
                &proxy_req.instance,
                &proxy_req.host,
                &status_code.as_u16().to_string(),
                &proxy_req.protocol.to_string(),
                &proxy_req.consumer.to_string(),
                &proxy_req.consumer.tier,
            ])
            .inc()
    }

    /// Counts one classified client message. `consumer` is the consumer's
    /// label value, resolved once per connection or request by the caller.
    pub fn count_rpc_request(
        &self,
        proxy_req: &ProxyRequest,
        consumer: &str,
        transport: Transport,
        classification: &Classification,
    ) {
        let network = proxy_req.consumer.network.as_str();
        let version = proxy_req.consumer.version.as_str();
        let method = classification.method.as_str();
        let shape = classification.shape.as_str();
        let heavy = is_heavy(classification);

        self.rpc_requests
            .with_label_values(&[
                network,
                version,
                &proxy_req.consumer.tier,
                transport.as_str(),
                method,
                shape,
                if heavy { "true" } else { "false" },
            ])
            .inc();

        self.rpc_consumer_requests
            .with_label_values(&[
                consumer,
                network,
                version,
                classification.method.family().as_str(),
            ])
            .inc();

        if heavy {
            self.rpc_consumer_heavy_requests
                .with_label_values(&[consumer, network, version, method, shape])
                .inc();
        }
    }

    pub fn observe_ws_session(&self, proxy_req: &ProxyRequest, duration: Duration, requests: u64) {
        let labels = [
            proxy_req.consumer.network.as_str(),
            proxy_req.consumer.version.as_str(),
            proxy_req.consumer.tier.as_str(),
        ];
        self.ws_session_duration
            .with_label_values(&labels)
            .observe(duration.as_secs_f64());
        self.ws_session_requests
            .with_label_values(&labels)
            .observe(requests as f64);
    }
}

/// The per-request instruments of one WebSocket connection or HTTP request,
/// with the labels they share resolved once.
#[derive(Clone)]
pub struct RpcScope {
    metrics: Metrics,
    network: String,
    version: String,
    transport: Transport,
    in_flight: IntGauge,
}

impl RpcScope {
    pub fn new(metrics: &Metrics, proxy_req: &ProxyRequest, transport: Transport) -> Self {
        let network = proxy_req.consumer.network.clone();
        let version = proxy_req.consumer.version.clone();
        let in_flight =
            metrics
                .rpc_in_flight
                .with_label_values(&[&network, &version, transport.as_str()]);
        Self {
            metrics: metrics.clone(),
            network,
            version,
            transport,
            in_flight,
        }
    }

    /// A request was forwarded to the instance.
    pub fn sent(&self) {
        self.in_flight.inc();
    }

    /// The instance answered a forwarded request.
    pub fn answered(
        &self,
        method: rpc::Method,
        shape: Shape,
        duration: Duration,
        bytes: usize,
        error: Option<ErrorCode>,
    ) {
        self.in_flight.dec();
        let labels = self.labels(method, shape);
        self.metrics
            .rpc_duration
            .with_label_values(&labels)
            .observe(duration.as_secs_f64());
        self.metrics
            .rpc_response_bytes
            .with_label_values(&labels)
            .observe(bytes as f64);
        if let Some(code) = error {
            self.error(method, code);
        }
    }

    /// The client went away before a forwarded request was answered.
    pub fn abandoned(&self, method: rpc::Method, shape: Shape) {
        self.in_flight.dec();
        self.metrics
            .rpc_abandoned
            .with_label_values(&self.labels(method, shape))
            .inc();
    }

    /// A forwarded request failed upstream and has no answer to observe.
    pub fn released(&self) {
        self.in_flight.dec();
    }

    /// A pending request was dropped to bound the correlator.
    pub fn evicted(&self) {
        self.in_flight.dec();
        self.unmatched("evicted");
    }

    /// A response answered no tracked request: most often the out-of-queue
    /// error to an `invalid` message, which carries no method, hence the label.
    pub fn unmatched_response(&self, method: Option<rpc::Method>, error: Option<ErrorCode>) {
        self.unmatched("no_request");
        if let Some(code) = error {
            self.error(method.unwrap_or(rpc::Method::Invalid), code);
        }
    }

    fn labels(&self, method: rpc::Method, shape: Shape) -> [&str; 5] {
        [
            &self.network,
            &self.version,
            self.transport.as_str(),
            method.as_str(),
            shape.as_str(),
        ]
    }

    fn error(&self, method: rpc::Method, code: ErrorCode) {
        self.metrics
            .rpc_errors
            .with_label_values(&[
                &self.network,
                &self.version,
                self.transport.as_str(),
                method.as_str(),
                &code.label(),
            ])
            .inc();
    }

    fn unmatched(&self, reason: &str) {
        self.metrics
            .rpc_unmatched
            .with_label_values(&[self.transport.as_str(), reason])
            .inc();
    }
}

/// One HTTP request in flight. Dropping it unfinished counts it as abandoned,
/// so a client that goes away at any point is counted, and in-flight falls.
pub struct RpcCall {
    scope: Option<RpcScope>,
    method: rpc::Method,
    shape: Shape,
}

impl RpcCall {
    pub fn start(scope: RpcScope, classification: &Classification) -> Self {
        scope.sent();
        Self {
            scope: Some(scope),
            method: classification.method,
            shape: classification.shape,
        }
    }

    pub fn answered(mut self, duration: Duration, bytes: usize, error: Option<ErrorCode>) {
        if let Some(scope) = self.scope.take() {
            scope.answered(self.method, self.shape, duration, bytes, error);
        }
    }

    pub fn released(mut self) {
        if let Some(scope) = self.scope.take() {
            scope.released();
        }
    }
}

impl Drop for RpcCall {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.take() {
            scope.abandoned(self.method, self.shape);
        }
    }
}

async fn api_get_metrics(state: &State) -> Result<ProxyResponse, hyper::Error> {
    let metrics = state.metrics.metrics_collected();

    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    encoder.encode(&metrics, &mut buffer).unwrap();

    let res = Response::builder().body(full(buffer)).unwrap();
    Ok(res)
}

async fn routes_match(
    req: Request<Incoming>,
    state: Arc<State>,
) -> Result<ProxyResponse, hyper::Error> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => api_get_metrics(&state).await,
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(full("Not Found"))
            .unwrap()),
    }
}

#[instrument("metrics server", skip_all)]
pub async fn start(state: Arc<State>) {
    let addr_result = SocketAddr::from_str(&state.config.prometheus_addr);
    if let Err(err) = addr_result {
        error!(error = err.to_string(), "invalid prometheus addr");
        std::process::exit(1);
    }
    let addr = addr_result.unwrap();

    let listener_result = TcpListener::bind(addr).await;
    if let Err(err) = listener_result {
        error!(
            error = err.to_string(),
            "fail to bind tcp prometheus server listener"
        );
        std::process::exit(1);
    }
    let listener = listener_result.unwrap();

    info!(addr = state.config.prometheus_addr, "metrics listening");

    loop {
        let state = state.clone();

        let accept_result = listener.accept().await;
        if let Err(err) = accept_result {
            error!(error = err.to_string(), "accept client prometheus server");
            continue;
        }
        let (stream, _) = accept_result.unwrap();

        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            let service = service_fn(move |req| routes_match(req, state.clone()));

            if let Err(err) = http1_server::Builder::new()
                .serve_connection(io, service)
                .await
            {
                error!(error = err.to_string(), "failed metrics server connection");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Protocol;
    use crate::rpc::classify;
    use crate::Consumer;

    fn exposition(metrics: &Metrics) -> String {
        let mut buffer = vec![];
        TextEncoder::new()
            .encode(&metrics.metrics_collected(), &mut buffer)
            .unwrap();
        String::from_utf8(buffer).unwrap()
    }

    fn proxy_request() -> ProxyRequest {
        ProxyRequest {
            namespace: "ftr-ogmios-v1".into(),
            host: "key.cardano-preprod-v6.ogmios-m1.demeter.run".into(),
            instance: "ogmios-cardano-preprod-6".into(),
            consumer: Consumer {
                namespace: "prj-a".into(),
                port_name: "port-b".into(),
                tier: "1".into(),
                network: "cardano-preprod".into(),
                version: "6".into(),
                ..Default::default()
            },
            protocol: Protocol::Websocket,
        }
    }

    #[test]
    fn rpc_families_have_no_series_until_a_message_is_counted() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        assert!(!exposition(&metrics).contains("ogmios_proxy_rpc"));
    }

    #[test]
    fn a_heavy_message_is_counted_in_all_three_families() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        let utxo = classify(br#"{"method":"queryLedgerState/utxo","params":{"addresses":["a"]}}"#);
        metrics.count_rpc_request(
            &proxy_request(),
            "prj-a.port-b",
            Transport::Websocket,
            &utxo,
        );

        let text = exposition(&metrics);
        assert!(text.contains(r#"ogmios_proxy_rpc_requests_total{heavy="true",method="queryLedgerState/utxo",network="cardano-preprod",shape="addresses/1",tier="1",transport="websocket",version="6"} 1"#), "{text}");
        assert!(text.contains(r#"ogmios_proxy_rpc_consumer_requests_total{consumer="prj-a.port-b",family="ledger-state",network="cardano-preprod",version="6"} 1"#), "{text}");
        assert!(text.contains(r#"ogmios_proxy_rpc_consumer_heavy_requests_total{consumer="prj-a.port-b",method="queryLedgerState/utxo",network="cardano-preprod",shape="addresses/1",version="6"} 1"#), "{text}");
    }

    #[test]
    fn a_light_message_has_no_heavy_series() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        let tip = classify(br#"{"method":"queryNetwork/tip"}"#);
        metrics.count_rpc_request(&proxy_request(), "prj-a.port-b", Transport::Http, &tip);

        let text = exposition(&metrics);
        assert!(
            text.contains(r#"heavy="false",method="queryNetwork/tip""#),
            "{text}"
        );
        assert!(text.contains(r#"transport="http""#), "{text}");
        assert!(
            !text.contains("ogmios_proxy_rpc_consumer_heavy_requests_total{"),
            "{text}"
        );
    }

    fn in_flight(metrics: &Metrics, transport: Transport) -> i64 {
        metrics
            .rpc_in_flight
            .with_label_values(&["cardano-preprod", "6", transport.as_str()])
            .get()
    }

    #[test]
    fn response_families_have_no_series_until_observed() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        let text = exposition(&metrics);
        assert!(!text.contains("ogmios_proxy_ws_session"), "{text}");
    }

    #[test]
    fn an_answered_request_is_timed_sized_and_its_error_counted() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        let scope = RpcScope::new(&metrics, &proxy_request(), Transport::Websocket);
        scope.sent();
        assert_eq!(in_flight(&metrics, Transport::Websocket), 1);
        scope.answered(
            rpc::Method::SubmitTransaction,
            Shape::Bytes(rpc::SizeBucket::Under4k),
            Duration::from_millis(30),
            700,
            Some(ErrorCode(Some(3005))),
        );
        assert_eq!(in_flight(&metrics, Transport::Websocket), 0);

        let text = exposition(&metrics);
        assert!(text.contains(r#"ogmios_proxy_rpc_duration_seconds_bucket{method="submitTransaction",network="cardano-preprod",shape="bytes/<4k",transport="websocket",version="6",le="0.05"} 1"#), "{text}");
        assert!(text.contains(r#"ogmios_proxy_rpc_response_bytes_bucket{method="submitTransaction",network="cardano-preprod",shape="bytes/<4k",transport="websocket",version="6",le="1024"} 1"#), "{text}");
        assert!(text.contains(r#"ogmios_proxy_rpc_errors_total{code="3005",method="submitTransaction",network="cardano-preprod",transport="websocket",version="6"} 1"#), "{text}");
        assert!(!text.contains("consumer=\"prj-a.port-b\""), "{text}");
        assert!(!text.contains("tier="), "{text}");
    }

    #[test]
    fn abandoned_evicted_and_unmatched() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        let scope = RpcScope::new(&metrics, &proxy_request(), Transport::Websocket);
        scope.sent();
        scope.sent();
        scope.abandoned(rpc::Method::NextBlock, Shape::None);
        scope.evicted();
        scope.unmatched_response(None, Some(ErrorCode(Some(-32700))));
        scope.unmatched_response(Some(rpc::Method::NextBlock), None);
        assert_eq!(in_flight(&metrics, Transport::Websocket), 0);

        let text = exposition(&metrics);
        assert!(text.contains(r#"ogmios_proxy_rpc_abandoned_total{method="nextBlock",network="cardano-preprod",shape="none",transport="websocket",version="6"} 1"#), "{text}");
        assert!(
            text.contains(
                r#"ogmios_proxy_rpc_unmatched_total{reason="evicted",transport="websocket"} 1"#
            ),
            "{text}"
        );
        assert!(
            text.contains(
                r#"ogmios_proxy_rpc_unmatched_total{reason="no_request",transport="websocket"} 2"#
            ),
            "{text}"
        );
        assert!(text.contains(r#"ogmios_proxy_rpc_errors_total{code="-32700",method="invalid",network="cardano-preprod",transport="websocket",version="6"} 1"#), "{text}");
    }

    #[test]
    fn an_http_call_dropped_unfinished_is_abandoned() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        let tip = classify(br#"{"method":"queryNetwork/tip"}"#);
        let scope = || RpcScope::new(&metrics, &proxy_request(), Transport::Http);

        let call = RpcCall::start(scope(), &tip);
        assert_eq!(in_flight(&metrics, Transport::Http), 1);
        drop(call);
        RpcCall::start(scope(), &tip).released();
        RpcCall::start(scope(), &tip).answered(Duration::from_millis(2), 100, None);
        assert_eq!(in_flight(&metrics, Transport::Http), 0);

        let text = exposition(&metrics);
        assert!(text.contains(r#"ogmios_proxy_rpc_abandoned_total{method="queryNetwork/tip",network="cardano-preprod",shape="none",transport="http",version="6"} 1"#), "{text}");
        assert!(text.contains(r#"ogmios_proxy_rpc_duration_seconds_count{method="queryNetwork/tip",network="cardano-preprod",shape="none",transport="http",version="6"} 1"#), "{text}");
    }

    #[test]
    fn session_histograms_carry_tier_and_no_consumer() {
        let metrics = Metrics::try_new(Registry::default()).unwrap();
        metrics.observe_ws_session(&proxy_request(), Duration::from_secs(90), 12);

        let text = exposition(&metrics);
        assert!(text.contains(r#"ogmios_proxy_ws_session_duration_seconds_bucket{network="cardano-preprod",tier="1",version="6",le="300"} 1"#), "{text}");
        assert!(text.contains(r#"ogmios_proxy_ws_session_requests_bucket{network="cardano-preprod",tier="1",version="6",le="100"} 1"#), "{text}");
        assert!(!text.contains("consumer="), "{text}");
    }
}
