// Recording rules over the proxies' request classification counters. They
// live in the root module because the proxy module is instantiated once per
// colour, which would record every series twice.
resource "kubernetes_manifest" "proxy_rpc_rules" {
  count      = length(var.proxy_rpc_telemetry_networks) > 0 ? 1 : 0
  depends_on = [kubernetes_namespace.namespace]

  manifest = {
    apiVersion = "monitoring.coreos.com/v1"
    kind       = "PrometheusRule"
    metadata = {
      labels = {
        "app.kubernetes.io/component" = "o11y"
        "app.kubernetes.io/part-of"   = "demeter"
      }
      name      = "ogmios-proxy-rpc"
      namespace = var.namespace
    }
    spec = {
      groups = [
        {
          name     = "ogmios-proxy-rpc"
          interval = "1m"
          rules = [
            {
              record = "ogmios_proxy:rpc_requests:rate5m"
              expr   = "sum by (network, version, tier, transport, method, shape, heavy) (rate(ogmios_proxy_rpc_requests_total[5m]))"
            },
            {
              record = "ogmios_proxy:rpc_consumer_requests:rate1h"
              expr   = "sum by (consumer, network, version, family) (rate(ogmios_proxy_rpc_consumer_requests_total[1h]))"
            },
            {
              record = "ogmios_proxy:rpc_consumer_heavy_requests:increase1h"
              expr   = "sum by (consumer, network, version, method, shape) (increase(ogmios_proxy_rpc_consumer_heavy_requests_total[1h]))"
            },
            {
              record = "ogmios_proxy:rpc_top_consumers:rate1h"
              expr   = "topk(25, sum by (consumer, network, version) (rate(ogmios_proxy_rpc_consumer_requests_total[1h])))"
            },
          ]
        }
      ]
    }
  }
}
