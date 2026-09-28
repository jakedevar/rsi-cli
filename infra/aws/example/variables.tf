variable "region" {
  type = string
}
variable "profile" {
  type    = string
  default = null
}
variable "vpc_id" {
  type    = string
  default = null
}
variable "subnet_id" {
  type    = string
  default = null
}
variable "associate_public_ip_address" {
  type    = bool
  default = null
}
variable "ssh_ingress_cidrs" {
  type    = set(string)
  default = []
}
variable "ssh_key_name" {
  type    = string
  default = null
}
variable "prefix" {
  type    = string
  default = "rsid"
}
variable "instance_type" {
  type    = string
  default = "m6i.large"
}
variable "architecture" {
  type    = string
  default = "x86_64"
}
variable "data_volume_gib" {
  type    = number
  default = 100
}
variable "data_snapshot_id" {
  type    = string
  default = null
}
variable "budget_usd" {
  type    = number
  default = 200
}
variable "budget_email" { type = string }
variable "source_repo_url" {
  type    = string
  default = null
}
variable "source_ref" { type = string }
variable "install_mode" {
  type    = string
  default = "artifact"
}
variable "artifact_url" {
  type    = string
  default = null
}
variable "artifact_sha256" {
  type    = string
  default = null
}
variable "secret_names" {
  type    = set(string)
  default = []
}
variable "operator_secrets" {
  type      = map(string)
  ephemeral = true
  sensitive = true
  default   = {}
}
variable "secret_versions" {
  type    = map(number)
  default = {}
}
variable "enable_tailscale" {
  type    = bool
  default = false
}
variable "stop_when_idle_minutes" {
  type    = number
  default = 0
}
variable "shutdown_at_utc" {
  type    = string
  default = ""
}
variable "enable_client_vpn" {
  type    = bool
  default = false
  validation {
    condition = !var.enable_client_vpn || (
      var.vpn_server_certificate_arn != null &&
      var.vpn_client_ca_certificate_arn != null &&
      var.vpn_client_cidr != null &&
      var.vpn_authorized_cidr != null
    )
    error_message = "A new Client VPN needs ACM server/client CA ARNs, client CIDR, and authorized CIDR."
  }
}
variable "vpn_server_certificate_arn" {
  type    = string
  default = null
}
variable "vpn_client_ca_certificate_arn" {
  type    = string
  default = null
}
variable "vpn_client_cidr" {
  type    = string
  default = null
}
variable "vpn_authorized_cidr" {
  type    = string
  default = null
}
variable "enable_vpn_ssh" {
  type    = bool
  default = false
  validation {
    condition = !var.enable_vpn_ssh || (
      var.ssh_key_name != null && (var.enable_client_vpn || var.existing_vpn_security_group_id != null)
    )
    error_message = "VPN SSH requires ssh_key_name and either a new Client VPN or an existing VPN security group ID."
  }
}
variable "existing_vpn_security_group_id" {
  type    = string
  default = null
}
