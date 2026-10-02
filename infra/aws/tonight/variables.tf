variable "ssh_key_name" {
  type = string
}
variable "ssh_ingress_cidr" {
  type = string
  validation {
    condition     = can(regex("^[0-9]{1,3}(\\.[0-9]{1,3}){3}/32$", var.ssh_ingress_cidr))
    error_message = "ssh_ingress_cidr must be one IPv4 /32."
  }
}
variable "source_ref" {
  type = string
  validation {
    condition     = can(regex("^[0-9a-f]{40}$", var.source_ref))
    error_message = "source_ref must be a full lowercase Git commit SHA."
  }
}
variable "operator_secrets" {
  type      = map(string)
  ephemeral = true
  sensitive = true
  default   = {}
}
