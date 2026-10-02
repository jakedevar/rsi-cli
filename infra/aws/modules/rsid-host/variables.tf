variable "region" { type = string }
variable "vpc_id" {
  type    = string
  default = null
  validation {
    condition     = (var.vpc_id == null) == (var.subnet_id == null)
    error_message = "vpc_id and subnet_id must both be set or both be null."
  }
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
  validation {
    condition = alltrue([
      for cidr in var.ssh_ingress_cidrs :
      can(cidrhost(cidr, 0)) && try(tonumber(split("/", cidr)[1]), 0) > 0
    ])
    error_message = "SSH sources must be valid CIDRs with a prefix greater than zero; world-open IPv4 and IPv6 ranges are refused."
  }
  validation {
    condition     = length(var.ssh_ingress_cidrs) == 0 || var.ssh_key_name != null
    error_message = "ssh_key_name is required when SSH CIDR ingress is enabled."
  }
}
variable "ssh_key_name" {
  type    = string
  default = null
  validation {
    condition     = var.ssh_key_name == null || trimspace(var.ssh_key_name) != ""
    error_message = "ssh_key_name must be a nonempty existing EC2 key pair name when set."
  }
}
variable "prefix" {
  type = string
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{2,23}$", var.prefix))
    error_message = "prefix must be 3-24 lowercase letters, digits, or hyphens, starting with a letter."
  }
}
variable "instance_type" { type = string }
variable "architecture" {
  type = string
  validation {
    condition     = contains(["x86_64", "arm64"], var.architecture)
    error_message = "architecture must be x86_64 or arm64."
  }
}
variable "data_volume_gib" {
  type = number
  validation {
    condition     = var.data_volume_gib >= 20 && var.data_volume_gib <= 16384
    error_message = "data_volume_gib must be between 20 and 16384."
  }
}
variable "data_snapshot_id" {
  type    = string
  default = null
}
variable "data_volume_final_snapshot" {
  type        = bool
  default     = true
  description = "Snapshot the data volume when it is destroyed. Keep true for a persistent host; set false where the volume only holds reproducible build state."
}
variable "budget_usd" {
  type = number
  validation {
    condition     = var.budget_usd > 0
    error_message = "budget_usd must be positive."
  }
}
variable "budget_email" { type = string }
variable "source_repo_url" {
  type    = string
  default = null
  validation {
    condition = (
      var.source_repo_url == null ||
      can(regex("^https://[^/@?#]+/[^@?#]+$", var.source_repo_url)) ||
      can(regex("^git@[^:/?#]+:[^@?#]+$", var.source_repo_url))
    )
    error_message = "Use an HTTPS URL without embedded credentials or a git@host:path SSH URL."
  }
}
variable "source_ref" {
  type = string
  validation {
    condition     = can(regex("^[0-9a-f]{40}$", var.source_ref))
    error_message = "source_ref must be a full lowercase Git commit SHA."
  }
}
variable "install_mode" {
  type    = string
  default = "artifact"
  validation {
    condition     = contains(["artifact", "build", "deferred"], var.install_mode)
    error_message = "install_mode must be artifact, build, or deferred."
  }
}
variable "shutdown_at_utc" {
  type    = string
  default = ""
  validation {
    condition     = var.shutdown_at_utc == "" || can(regex("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$", var.shutdown_at_utc))
    error_message = "shutdown_at_utc must be empty or an RFC3339 UTC timestamp without fractions."
  }
}
variable "artifact_url" {
  type    = string
  default = null
  validation {
    condition     = var.artifact_url == null ? true : can(regex("^https://[^/@?#]+/[^@?#]+$", var.artifact_url))
    error_message = "artifact_url must be a credential-free HTTPS URL without a query or fragment."
  }
}
variable "artifact_sha256" {
  type    = string
  default = null
  validation {
    condition     = var.artifact_sha256 == null ? true : can(regex("^[0-9a-f]{64}$", var.artifact_sha256))
    error_message = "artifact_sha256 must be a lowercase SHA-256 hex digest."
  }
}
variable "secret_names" {
  type    = set(string)
  default = []
  validation {
    condition = alltrue([
      for name in var.secret_names : contains(["anthropic_api_key", "codex_api_key", "openrouter_api_key", "github_deploy_key", "tailscale_auth_key"], name)
    ])
    error_message = "secret_names contains an unsupported name."
  }
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
  validation {
    condition     = alltrue([for version in values(var.secret_versions) : version >= 1 && floor(version) == version])
    error_message = "Secret versions must be positive integers."
  }
}
variable "provider_cli_npm_packages" {
  description = "Exact-version npm specs for provider CLIs installed system-wide at boot, e.g. @anthropic-ai/claude-code@2.1.283."
  type        = list(string)
  default     = []
  validation {
    condition = alltrue([
      for spec in var.provider_cli_npm_packages : can(regex("^(@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*@[0-9]+\\.[0-9]+\\.[0-9]+$", spec))
    ])
    error_message = "Each provider CLI must be an npm package pinned to an exact x.y.z version."
  }
}
variable "enable_tailscale" { type = bool }
variable "stop_when_idle_minutes" {
  type = number
  validation {
    condition     = var.stop_when_idle_minutes == 0 || var.stop_when_idle_minutes >= 30
    error_message = "Use 0 to disable or at least 30 minutes."
  }
}
variable "tags" { type = map(string) }
