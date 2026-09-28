variable "prefix" { type = string }
variable "vpc_id" { type = string }
variable "target_subnet_id" { type = string }
variable "client_cidr" {
  type = string
  validation {
    condition     = can(cidrhost(var.client_cidr, 0)) && try(tonumber(split("/", var.client_cidr)[1]), 0) >= 12 && try(tonumber(split("/", var.client_cidr)[1]), 0) <= 22 && !strcontains(var.client_cidr, ":")
    error_message = "client_cidr must be an IPv4 /12 through /22 range."
  }
}
variable "authorized_cidr" {
  type = string
  validation {
    condition     = can(cidrhost(var.authorized_cidr, 0)) && try(tonumber(split("/", var.authorized_cidr)[1]), 0) > 0 && !strcontains(var.authorized_cidr, ":")
    error_message = "authorized_cidr must be an explicit non-world-open IPv4 CIDR."
  }
}
variable "server_certificate_arn" {
  type = string
  validation {
    condition     = can(regex("^arn:[^:]+:acm:[^:]+:[0-9]{12}:certificate/[0-9a-fA-F-]+$", var.server_certificate_arn))
    error_message = "server_certificate_arn must be an ACM certificate ARN."
  }
}
variable "client_ca_certificate_arn" {
  type = string
  validation {
    condition     = can(regex("^arn:[^:]+:acm:[^:]+:[0-9]{12}:certificate/[0-9a-fA-F-]+$", var.client_ca_certificate_arn))
    error_message = "client_ca_certificate_arn must be an ACM certificate ARN."
  }
}
variable "tags" { type = map(string) }
