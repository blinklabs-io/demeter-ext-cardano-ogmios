locals {
  network_version_combinations = [
    for combo in setproduct(var.networks, var.versions) : {
      network = combo[0]
      version = combo[1]
    }
  ]
}

resource "kubernetes_namespace" "namespace" {
  metadata {
    name = var.namespace
  }
}

module "ogmios_v1_feature" {
  depends_on         = [kubernetes_namespace.namespace]
  source             = "./feature"
  namespace          = var.namespace
  operator_image_tag = var.operator_image_tag
  metrics_delay      = var.metrics_delay
  extension_name     = var.extension_name
  api_key_salt       = var.api_key_salt
  dns_zone           = var.dns_zone
  resources          = var.operator_resources
  tolerations        = var.operator_tolerations
}

module "ogmios_v1_proxies_blue" {
  depends_on = [kubernetes_namespace.namespace]
  source     = "./proxy"

  cloud_provider    = var.cloud_provider
  cluster_issuer    = var.cluster_issuer
  dns_zone          = var.dns_zone
  dns_names         = distinct(flatten(values(var.dns_names)))
  environment       = var.proxy_blue_environment
  extension_name    = var.extension_name
  extra_annotations = length(var.proxy_blue_extra_annotations) > 0 ? var.proxy_blue_extra_annotations : (length(var.proxy_blue_extra_annotations_by_network) > 0 ? merge(values(var.proxy_blue_extra_annotations_by_network)...) : {})
  name              = "proxy-${var.proxy_blue_environment}"
  networks          = var.networks
  namespace         = var.namespace
  proxy_image_tag   = var.proxy_blue_image_tag
  replicas          = var.proxy_blue_replicas
  resources         = var.proxy_resources
  tolerations       = var.proxy_tolerations
  versions          = var.versions

  rpc_telemetry_networks = var.proxy_rpc_telemetry_networks
}

module "ogmios_v1_proxies_green" {
  depends_on = [kubernetes_namespace.namespace]
  source     = "./proxy"

  cloud_provider    = var.cloud_provider
  cluster_issuer    = var.cluster_issuer
  dns_zone          = var.dns_zone
  dns_names         = distinct(flatten(values(var.dns_names)))
  environment       = var.proxy_green_environment
  extension_name    = var.extension_name
  extra_annotations = length(var.proxy_green_extra_annotations) > 0 ? var.proxy_green_extra_annotations : (length(var.proxy_green_extra_annotations_by_network) > 0 ? merge(values(var.proxy_green_extra_annotations_by_network)...) : {})
  name              = "proxy-${var.proxy_green_environment}"
  networks          = var.networks
  namespace         = var.namespace
  proxy_image_tag   = var.proxy_green_image_tag
  replicas          = var.proxy_green_replicas
  resources         = var.proxy_resources
  tolerations       = var.proxy_tolerations
  versions          = var.versions

  rpc_telemetry_networks = var.proxy_rpc_telemetry_networks
}

// mainnet

module "ogmios_configs" {
  depends_on = [kubernetes_namespace.namespace]
  for_each   = { for network in var.networks : "${network}" => network }

  source    = "./configs"
  namespace = var.namespace
  network   = each.value
}

module "ogmios_instances" {
  depends_on = [kubernetes_namespace.namespace, module.ogmios_configs]
  for_each   = var.instances
  source     = "./instance"

  namespace         = var.namespace
  salt              = each.value.salt
  network           = each.value.network
  ogmios_image      = each.value.ogmios_image
  node_private_dns  = each.value.node_private_dns
  ogmios_version    = each.value.ogmios_version
  replicas          = each.value.replicas
  image_pull_secret = each.value.image_pull_secret

  node_balancer            = each.value.node_balancer
  node_srv_record          = each.value.node_srv_record
  node_max_conn_per_server = each.value.node_max_conn_per_server
  spread                   = each.value.spread

  tolerations = coalesce(each.value.tolerations, [
    {
      effect   = "NoSchedule"
      key      = "demeter.run/compute-profile"
      operator = "Exists"
    },
    {
      effect   = "NoSchedule"
      key      = "demeter.run/compute-arch"
      operator = "Equal"
      value    = "arm64"
    },
    {
      effect   = "NoSchedule"
      key      = "demeter.run/availability-sla"
      operator = "Equal"
      value    = "consistent"
    }
  ])
  node_affinity = coalesce(each.value.node_affinity, {
    required_during_scheduling_ignored_during_execution  = {}
    preferred_during_scheduling_ignored_during_execution = []
  })
}

module "ogmios_services" {
  depends_on = [kubernetes_namespace.namespace]
  for_each   = { for i, nv in local.network_version_combinations : "${nv.network}-${nv.version}" => nv }
  source     = "./service"

  namespace      = var.namespace
  ogmios_version = each.value.version
  network        = each.value.network
}

module "ogmios_monitoring" {
  source = "./monitoring"

  for_each = var.o11y_datasource_uid != null ? toset(["enabled"]) : toset([])

  o11y_datasource_uid = var.o11y_datasource_uid
}
