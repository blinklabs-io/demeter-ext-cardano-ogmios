locals {
  name           = "ogmios-${var.network}-${var.ogmios_version}-${var.salt}"
  image          = var.ogmios_image
  container_port = 1337
  default_args = [
    "--node-socket", "/ipc/node.socket",
    "--node-config", "/config/config.json",
    "--host", "0.0.0.0"
  ]
  container_args = var.ogmios_version == "5" ? local.default_args : concat(local.default_args, ["--include-cbor"])
}

resource "kubernetes_deployment_v1" "ogmios" {
  wait_for_rollout = false

  metadata {
    name      = local.name
    namespace = var.namespace
    labels = {
      "role"                               = "instance"
      "demeter.run/kind"                   = "OgmiosInstance"
      "cardano.demeter.run/network"        = var.network
      "cardano.demeter.run/ogmios-version" = var.ogmios_version
    }
  }
  spec {
    replicas = var.replicas

    strategy {
      rolling_update {
        max_surge       = 2
        max_unavailable = 0
      }
    }

    selector {
      match_labels = {
        "role"                               = "instance"
        "demeter.run/instance"               = local.name
        "cardano.demeter.run/network"        = var.network
        "cardano.demeter.run/ogmios-version" = var.ogmios_version
      }
    }
    template {
      metadata {
        name = local.name
        labels = {
          "role"                               = "instance"
          "demeter.run/instance"               = local.name
          "cardano.demeter.run/network"        = var.network
          "cardano.demeter.run/ogmios-version" = var.ogmios_version
        }
      }
      spec {
        restart_policy = "Always"

        dynamic "image_pull_secrets" {
          for_each = var.image_pull_secret != null ? [var.image_pull_secret] : []
          content {
            name = image_pull_secrets.value
          }
        }

        security_context {
          fs_group = 1000
        }

        dynamic "affinity" {
          for_each = (
            var.node_affinity != null &&
            (
              try(length(var.node_affinity.required_during_scheduling_ignored_during_execution.node_selector_term), 0) > 0 ||
              try(length(var.node_affinity.preferred_during_scheduling_ignored_during_execution), 0) > 0
            )
          ) ? [var.node_affinity] : []
          content {
            node_affinity {
              dynamic "required_during_scheduling_ignored_during_execution" {
                for_each = (
                  var.node_affinity.required_during_scheduling_ignored_during_execution != null &&
                  length(var.node_affinity.required_during_scheduling_ignored_during_execution.node_selector_term) > 0
                ) ? [var.node_affinity.required_during_scheduling_ignored_during_execution] : []
                content {
                  dynamic "node_selector_term" {
                    for_each = required_during_scheduling_ignored_during_execution.value.node_selector_term
                    content {
                      dynamic "match_expressions" {
                        for_each = length(node_selector_term.value.match_expressions) > 0 ? node_selector_term.value.match_expressions : []
                        content {
                          key      = match_expressions.value.key
                          operator = match_expressions.value.operator
                          values   = match_expressions.value.values
                        }
                      }
                    }
                  }
                }
              }
              dynamic "preferred_during_scheduling_ignored_during_execution" {
                for_each = (
                  var.node_affinity.preferred_during_scheduling_ignored_during_execution != null &&
                  length(var.node_affinity.preferred_during_scheduling_ignored_during_execution) > 0
                ) ? var.node_affinity.preferred_during_scheduling_ignored_during_execution : []
                content {
                  weight = preferred_during_scheduling_ignored_during_execution.value.weight

                  dynamic "preference" {
                    for_each = (
                      length(preferred_during_scheduling_ignored_during_execution.value.preference.match_expressions) > 0 ||
                      length(preferred_during_scheduling_ignored_during_execution.value.preference.match_fields) > 0
                    ) ? [preferred_during_scheduling_ignored_during_execution.value.preference] : []
                    content {
                      dynamic "match_expressions" {
                        for_each = length(preference.value.match_expressions) > 0 ? preference.value.match_expressions : []
                        content {
                          key      = match_expressions.value.key
                          operator = match_expressions.value.operator
                          values   = match_expressions.value.values
                        }
                      }
                      dynamic "match_fields" {
                        for_each = length(preference.value.match_fields) > 0 ? preference.value.match_fields : []
                        content {
                          key      = match_fields.value.key
                          operator = match_fields.value.operator
                          values   = match_fields.value.values
                        }
                      }
                    }
                  }
                }
              }
            }
          }
        }

        container {
          name              = "main"
          image             = local.image
          image_pull_policy = "IfNotPresent"
          args              = local.container_args

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
            container_port = local.container_port
            name           = "api"
            protocol       = "TCP"
          }

          volume_mount {
            name       = "ipc"
            mount_path = "/ipc"
          }

          volume_mount {
            name       = "node-config"
            mount_path = "/config"
          }

          dynamic "volume_mount" {
            for_each = var.network == "vector-testnet" ? toset([1]) : toset([])

            content {
              name       = "genesis"
              mount_path = "/genesis/${var.network}"
            }

          }

          liveness_probe {
            http_get {
              path   = "/health"
              port   = "api"
              scheme = "HTTP"
            }
            initial_delay_seconds = 60
            period_seconds        = 30
            timeout_seconds       = 5
            success_threshold     = 1
            failure_threshold     = 2
          }
        }

        container {
          name  = "socat"
          image = "alpine/socat"
          args = [
            "UNIX-LISTEN:/ipc/node.socket,reuseaddr,fork,unlink-early",
            "TCP-CONNECT:${var.node_private_dns}"
          ]

          security_context {
            run_as_user  = 1000
            run_as_group = 1000
          }

          volume_mount {
            name       = "ipc"
            mount_path = "/ipc"
          }
        }

        volume {
          name = "ipc"
          empty_dir {}
        }

        volume {
          name = "node-config"

          config_map {
            name = "configs-${var.network}"
          }
        }

        dynamic "volume" {
          for_each = var.network == "vector-testnet" ? toset([1]) : toset([])

          content {
            name = "genesis"

            config_map {
              name = "genesis-${var.network}"
            }
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
