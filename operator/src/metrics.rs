use std::sync::Arc;

use chrono::Utc;
use kube::{Resource, ResourceExt};
use prometheus::{opts, IntCounterVec, Registry};
use regex::Regex;
use serde::{Deserialize, Deserializer};
use tracing::{error, info, instrument};

use crate::{get_config, Error, OgmiosPort, State};

#[derive(Clone)]
pub struct Metrics {
    pub usage: IntCounterVec,
    pub reconcile_failures: IntCounterVec,
    pub metrics_failures: IntCounterVec,
}

impl Default for Metrics {
    fn default() -> Self {
        let usage = IntCounterVec::new(
            opts!("usage", "Feature usage",),
            &["feature", "project", "resource_name", "tier"],
        )
        .unwrap();

        let reconcile_failures = IntCounterVec::new(
            opts!(
                "ogmios_operator_reconciliation_errors_total",
                "reconciliation errors",
            ),
            &["instance", "error"],
        )
        .unwrap();

        let metrics_failures = IntCounterVec::new(
            opts!(
                "ogmios_metrics_controller_errors_total",
                "errors to calculation metrics",
            ),
            &["error"],
        )
        .unwrap();

        Metrics {
            reconcile_failures,
            usage,
            metrics_failures,
        }
    }
}

impl Metrics {
    pub fn register(self, registry: &Registry) -> Result<Self, prometheus::Error> {
        registry.register(Box::new(self.metrics_failures.clone()))?;
        registry.register(Box::new(self.reconcile_failures.clone()))?;
        registry.register(Box::new(self.usage.clone()))?;

        Ok(self)
    }

    pub fn reconcile_failure(&self, crd: &OgmiosPort, e: &Error) {
        self.reconcile_failures
            .with_label_values(&[crd.name_any().as_ref(), e.metric_label().as_ref()])
            .inc()
    }

    pub fn metrics_failure(&self, e: &Error) {
        self.metrics_failures
            .with_label_values(&[e.metric_label().as_ref()])
            .inc()
    }

    pub fn count_usage(&self, project: &str, resource_name: &str, tier: &str, value: f64) {
        let feature = &OgmiosPort::kind(&());
        let value: u64 = value.ceil() as u64;

        self.usage
            .with_label_values(&[feature, project, resource_name, tier])
            .inc_by(value);
    }
}

/// Builds the PromQL that meters an Ogmios port over `interval` seconds ending
/// at `timestamp` (Unix seconds).
///
/// The billed quantity is a *message the proxy served*, so both counters are
/// counter-shaped `increase(...)` — never an average of the
/// `ogmios_proxy_total_connections` gauge, which is a capacity signal and a
/// tier constraint, not a billed quantity.
///
/// Two details are load bearing:
///
/// - `protocol="http"` excludes the WebSocket upgrade. The proxy counts
///   `http_total_request` once for *every* proxied request, including the
///   upgrade (which answers `101`), so without the filter each WebSocket
///   connection would bill one phantom message on top of its frames.
/// - `or` unions the two counters *inside* the `sum`. A `+` between aggregated
///   vectors drops any series missing on either side, which would bill a
///   WebSocket-only consumer — the common case — zero.
fn build_usage_query(interval: i64, timestamp: i64) -> String {
    format!(
        "sum by (consumer, route, tier) (\
           increase(ogmios_proxy_ws_total_frame[{interval}s] @ {timestamp}) \
           or \
           increase(ogmios_proxy_http_total_request{{protocol=\"http\", status_code!~\"401|429|503\"}}[{interval}s] @ {timestamp})\
         )"
    )
}

#[instrument("metrics collector run", skip_all)]
pub async fn run_metrics_collector(state: Arc<State>) {
    tokio::spawn(async move {
        info!("collecting metrics running");

        let config = get_config();
        let client = reqwest::Client::builder().build().unwrap();
        let project_regex = Regex::new(r"prj-(.+)\.(.+)$").unwrap();
        let network_regex = Regex::new(r"([\w-]+)-.+").unwrap();
        let mut last_execution = Utc::now();

        loop {
            tokio::time::sleep(config.metrics_delay).await;

            let end = Utc::now();
            let interval = (end - last_execution).num_seconds();

            last_execution = end;

            let query = build_usage_query(interval, end.timestamp_millis() / 1000);

            let response = match client
                .get(format!("{}/query?query={query}", config.prometheus_url))
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    error!(error = err.to_string(), "error to make prometheus request");
                    state
                        .metrics
                        .metrics_failure(&Error::HttpError(err.to_string()));
                    continue;
                }
            };

            let status = response.status();
            if status.is_client_error() || status.is_server_error() {
                error!(status = status.to_string(), "request status code fail");
                state.metrics.metrics_failure(&Error::HttpError(format!(
                    "Prometheus request error. Status: {} Query: {}",
                    status, query
                )));
                continue;
            }

            let response = response.json::<PrometheusResponse>().await.unwrap();
            for result in response.data.result {
                if result.value == 0.0
                    || result.metric.consumer.is_none()
                    || result.metric.route.is_none()
                {
                    continue;
                }

                let consumer = result.metric.consumer.unwrap();
                let project_captures = project_regex.captures(&consumer);
                if project_captures.is_none() {
                    continue;
                }
                let project_captures = project_captures.unwrap();
                let project = project_captures.get(1).unwrap().as_str();
                let resource_name = project_captures.get(2).unwrap().as_str();

                let route = result.metric.route.unwrap();
                let network_captures = network_regex.captures(&route);
                if network_captures.is_none() {
                    continue;
                }

                if let Some(tier) = result.metric.tier {
                    state
                        .metrics
                        .count_usage(project, resource_name, &tier, result.value);
                }
            }
        }
    });
}

#[derive(Debug, Deserialize)]
struct PrometheusDataResultMetric {
    consumer: Option<String>,
    route: Option<String>,
    tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrometheusDataResult {
    metric: PrometheusDataResultMetric,
    #[serde(deserialize_with = "deserialize_value")]
    value: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrometheusData {
    result: Vec<PrometheusDataResult>,
}

#[derive(Debug, Deserialize)]
struct PrometheusResponse {
    data: PrometheusData,
}

fn deserialize_value<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Vec<serde_json::Value> = Deserialize::deserialize(deserializer)?;
    Ok(value.into_iter().as_slice()[1]
        .as_str()
        .unwrap()
        .parse::<f64>()
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::build_usage_query;

    #[test]
    fn usage_query_is_the_counter_union() {
        assert_eq!(
            build_usage_query(60, 1721000000),
            "sum by (consumer, route, tier) (increase(ogmios_proxy_ws_total_frame[60s] @ 1721000000) or increase(ogmios_proxy_http_total_request{protocol=\"http\", status_code!~\"401|429|503\"}[60s] @ 1721000000))"
        );
    }

    /// A `+` between the two aggregated vectors would drop any series missing
    /// on either side, billing a WebSocket-only consumer zero. The union must
    /// be `or`, and it must sit inside the `sum`.
    #[test]
    fn usage_query_unions_with_or_never_plus() {
        let query = build_usage_query(300, 1721000000);

        assert!(query.contains(" or "));
        assert!(!query.contains(" + "));
        assert!(query.starts_with("sum by (consumer, route, tier) ("));
    }

    /// The gauge is a capacity signal and the tier's max-connections
    /// constraint — never a billed quantity.
    #[test]
    fn usage_query_never_meters_the_connections_gauge() {
        let query = build_usage_query(300, 1721000000);

        assert!(!query.contains("ogmios_proxy_total_connections"));
        assert!(!query.contains("avg_over_time"));
    }
}
