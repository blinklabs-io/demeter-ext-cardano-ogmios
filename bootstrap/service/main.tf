variable "namespace" {
  description = "the namespace where the resources will be created"
}

variable "network" {
  description = "cardano node network"
}

variable "ogmios_version" {
  type = string

  validation {
    condition     = contains(["5", "6", "7"], var.ogmios_version)
    error_message = "Invalid version. Allowed values are 5, 6 or 7."
  }
}
