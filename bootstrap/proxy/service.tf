resource "kubernetes_service_v1" "proxy_service_aws" {
  for_each = toset([for n in toset(["loadbalancer"]) : n if var.cloud_provider == "aws"])
  metadata {
    name      = local.name
    namespace = var.namespace
    annotations = merge({
      "service.beta.kubernetes.io/aws-load-balancer-nlb-target-type" : "instance"
      "service.beta.kubernetes.io/aws-load-balancer-scheme" : "internet-facing"
      "service.beta.kubernetes.io/aws-load-balancer-type" : "external"
      "service.beta.kubernetes.io/aws-load-balancer-healthcheck-protocol" : "HTTPS"
      "service.beta.kubernetes.io/aws-load-balancer-healthcheck-path" : "/healthz"
      "service.beta.kubernetes.io/aws-load-balancer-healthcheck-port" : var.healthcheck_port != null ? var.healthcheck_port : "traffic-port"
      },
      var.extra_annotations
    )
  }

  spec {
    load_balancer_class = "service.k8s.aws/nlb"
    selector            = local.proxy_labels

    port {
      name        = "proxy"
      port        = 443
      target_port = local.proxy_port
      protocol    = "TCP"
    }


    port {
      name        = "health"
      port        = 80
      target_port = local.proxy_port
      protocol    = "TCP"
    }

    type = "LoadBalancer"
  }
}

resource "kubernetes_service_v1" "proxy_service_gcp" {
  for_each = toset([for n in toset(["loadbalancer"]) : n if var.cloud_provider == "gcp"])
  metadata {
    name      = local.name
    namespace = var.namespace
    annotations = merge({
      "cloud.google.com/l4-rbs" : "enabled"
      },
      var.extra_annotations
    )
  }

  spec {
    external_traffic_policy = "Local"
    selector                = local.proxy_labels

    port {
      name        = "proxy"
      port        = 443
      target_port = local.proxy_port
      protocol    = "TCP"
    }

    port {
      name        = "health"
      port        = 80
      target_port = local.proxy_port
      protocol    = "TCP"
    }

    type = "LoadBalancer"
  }
}
