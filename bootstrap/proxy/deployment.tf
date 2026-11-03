resource "kubernetes_deployment_v1" "ogmios_proxy" {
  wait_for_rollout = false
  depends_on       = [kubernetes_manifest.certificate_cluster_wildcard_tls]

  metadata {
    name      = local.name
    namespace = var.namespace
    labels    = local.proxy_labels
  }
  spec {
    replicas = var.replicas
    selector {
      match_labels = local.proxy_labels
    }
    template {
      metadata {
        name   = local.name
        labels = local.proxy_labels
      }
      spec {
        container {
          name              = "main"
          image             = "ghcr.io/demeter-run/ext-cardano-ogmios-proxy:${var.proxy_image_tag}"
          image_pull_policy = "IfNotPresent"

          resources {
            limits = {
              cpu    = var.resources.limits.cpu
              memory = var.resources.limits.memory
            }
            requests = {
              cpu    = var.resources.requests.cpu
              memory = var.resources.requests.memory
            }
          }

          port {
            name           = "metrics"
            container_port = local.prometheus_port
            protocol       = "TCP"
          }

          port {
            name           = "proxy"
            container_port = local.proxy_port
            protocol       = "TCP"
          }

          env {
            name  = "PROXY_NAMESPACE"
            value = var.namespace
          }

          env {
            name  = "PROXY_ADDR"
            value = local.proxy_addr
          }

          env {
            name  = "PROXY_TIERS_PATH"
            value = "/configs/tiers.toml"
          }

          env {
            name  = "PROMETHEUS_ADDR"
            value = local.prometheus_addr
          }

          env {
            name  = "OGMIOS_PORT"
            value = var.ogmios_port
          }

          env {
            name  = "OGMIOS_DNS"
            value = "ext-ogmios-m1.svc.cluster.local"
          }

          env {
            name  = "HEALTH_NETWORK"
            value = "cardano-mainnet"
          }

          env {
            name  = "SSL_CRT_PATH"
            value = "/certs/tls.crt"
          }

          env {
            name  = "SSL_KEY_PATH"
            value = "/certs/tls.key"
          }

          env {
            name  = "RPC_TELEMETRY_NETWORKS"
            value = join(",", var.rpc_telemetry_networks)
          }

          env {
            name  = "CORS_ALLOW_ORIGIN"
            value = "*"
          }

          env {
            name  = "CORS_ALLOW_METHODS"
            value = "GET, POST, PUT, DELETE, OPTIONS"
          }

          env {
            name  = "CORS_ALLOW_HEADERS"
            value = "Content-Type, Authorization, X-Requested-With, dmtr-api-key, traceparent"
          }

          env {
            name  = "CORS_MAX_AGE"
            value = "86400"
          }

          volume_mount {
            mount_path = "/certs"
            name       = "certs"
          }

          volume_mount {
            mount_path = "/configs"
            name       = "configs"
          }
        }

        volume {
          name = "certs"
          secret {
            secret_name = local.cert_secret_name
          }
        }

        volume {
          name = "configs"
          config_map {
            name = kubernetes_config_map.proxy.metadata.0.name
          }
        }

        dynamic "toleration" {
          for_each = var.tolerations
          content {
            effect   = toleration.value.effect
            key      = toleration.value.key
            operator = toleration.value.operator
            value    = toleration.value.value
          }
        }
      }
    }
  }
}
