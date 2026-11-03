variable "namespace" {
  type = string
}

variable "ogmios_version" {
  type = string

  validation {
    condition     = contains(["5", "6", "7"], var.ogmios_version)
    error_message = "Invalid version. Allowed values are 5, 6 or 7."
  }
}

variable "ogmios_image" {
  type = string
}

variable "salt" {
  type = string
}

variable "network" {
  type = string
}

variable "node_private_dns" {
  description = "host:port of the node's n2c endpoint. Used by the socat balancer."
  type        = string
}

variable "node_balancer" {
  description = "Sidecar that serves /ipc/node.socket: \"socat\" forwards every session to node_private_dns; \"haproxy\" spreads sessions over the Ready members of the pool named by node_srv_record, by least connections."
  type        = string
  default     = "socat"
  nullable    = false

  validation {
    condition     = contains(["socat", "haproxy"], var.node_balancer)
    error_message = "Invalid node_balancer. Allowed values are socat or haproxy."
  }
}

variable "node_srv_record" {
  description = "DNS SRV name of a headless Service that publishes only the pool's Ready nodes, e.g. _n2c._tcp.node-mainnet-pool.ext-nodes-m1.svc.cluster.local. Required when node_balancer is haproxy."
  type        = string
  default     = null

  validation {
    condition     = var.node_srv_record == null || can(regex("^_[^.]+\\._tcp\\.[^.]+\\..+$", var.node_srv_record))
    error_message = "node_srv_record must be an SRV name of the form _<port>._tcp.<service>.<namespace>.svc.<domain>."
  }
}

variable "node_max_conn_per_server" {
  description = "With the haproxy balancer, the most sessions one node takes from this pod."
  type        = number
  default     = 200
  nullable    = false
}

variable "spread" {
  description = "Spread the pods of all instances of this network and version across hosts and zones."
  type        = bool
  default     = false
  nullable    = false
}

variable "node_affinity" {
  type = object({
    required_during_scheduling_ignored_during_execution = optional(
      object({
        node_selector_term = optional(
          list(object({
            match_expressions = optional(
              list(object({
                key      = string
                operator = string
                values   = list(string)
              })), []
            )
          })), []
        )
      }), {}
    )
    preferred_during_scheduling_ignored_during_execution = optional(
      list(object({
        weight = number
        preference = object({
          match_expressions = optional(
            list(object({
              key      = string
              operator = string
              values   = list(string)
            })), []
          )
          match_fields = optional(
            list(object({
              key      = string
              operator = string
              values   = list(string)
            })), []
          )
        })
      })), []
    )
  })
  default = {}
}

variable "replicas" {
  type    = number
  default = 1
}

variable "resources" {
  type = object({
    limits = object({
      cpu    = string
      memory = string
    })
    requests = object({
      cpu    = string
      memory = string
    })
  })
  default = {
    limits : {
      cpu : "2",
      memory : "1Gi"
    }
    requests : {
      cpu : "200m",
      memory : "500Mi"
    }
  }
}

variable "tolerations" {
  description = "List of tolerations for the instance"
  type = list(object({
    effect   = string
    key      = string
    operator = string
    value    = optional(string)
  }))
  default = [
    {
      effect   = "NoSchedule"
      key      = "demeter.run/compute-profile"
      operator = "Exists"
    },
    {
      effect   = "NoSchedule"
      key      = "demeter.run/compute-arch"
      operator = "Equal"
      value    = "x86"
    },
    {
      effect   = "NoSchedule"
      key      = "demeter.run/availability-sla"
      operator = "Equal"
      value    = "consistent"
    }
  ]
}

variable "image_pull_secret" {
  type    = string
  default = null
}
