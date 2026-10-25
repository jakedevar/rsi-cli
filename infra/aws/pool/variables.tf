variable "ssh_key_name" {
  type        = string
  description = "Existing EC2 key pair whose private key scripts/cloud-pool.sh uses (same pair as infra/aws/gate)."
}

variable "network_prefix" {
  type        = string
  default     = "rsid-rsid"
  description = "Name/Service tag prefix of the persistent rsid-host network (VPC, public subnet, SSH security group) the pool borrows, exactly as infra/aws/gate does."
}

variable "hosts" {
  type        = map(string)
  default     = {}
  description = "Pool hosts: slot name (pool-1 .. pool-6) to the RFC3339 UTC lease expiry written to that host's rsi:expires-at tag. scripts/cloud-pool.sh generates it from ~/.rsi/cloud/pool.json; an empty map is an empty pool."
  validation {
    condition     = length(var.hosts) <= 6
    error_message = "The pool holds at most 6 hosts."
  }
  validation {
    condition     = alltrue([for slot in keys(var.hosts) : can(regex("^pool-[1-6]$", slot))])
    error_message = "Host slots must be named pool-1 through pool-6."
  }
  validation {
    condition     = alltrue([for expiry in values(var.hosts) : can(regex("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$", expiry))])
    error_message = "Each lease expiry must be a UTC RFC3339 time such as 2026-10-21T10:00:00Z."
  }
}

variable "instance_type" {
  type        = string
  default     = "c7i.8xlarge"
  description = "The only type the rsi-cloud-terraform IAM policy allows as spot (see infra/aws/gate)."
}

variable "spot_max_price_usd" {
  type        = number
  default     = 0.75
  description = "Spot max price in USD per hour per host. AWS never bills an hour above it, which is what lets scripts/cloud-pool.sh reserve each lease's worst case (this + gp3 + IPv4, $0.7813/h) in the spend ledger before applying."
  validation {
    condition     = var.spot_max_price_usd > 0 && var.spot_max_price_usd <= 0.75
    error_message = "spot_max_price_usd must be greater than 0 and at most 0.75 USD per hour, the cap scripts/cloud-pool.sh reserves ($0.7813/h) its leases at."
  }
}

variable "root_volume_gib" {
  type        = number
  default     = 200
  description = "Encrypted gp3 root volume (at most 200 GiB, the size inside the reserved $0.7813/h); it holds the warm cargo target for the lease and is deleted with the instance."
  validation {
    condition     = var.root_volume_gib >= 60 && var.root_volume_gib <= 200
    error_message = "root_volume_gib must be between 60 and 200, the size scripts/cloud-pool.sh reserves its leases at."
  }
}

variable "lease_minutes" {
  type        = number
  default     = 720
  description = "Length of the first lease: the guest powers itself off after this long (shutdown terminates the instance and deletes its volume). scripts/cloud-pool.sh moves the guest timer when it renews a lease."
  validation {
    condition     = var.lease_minutes >= 30 && var.lease_minutes <= 720
    error_message = "lease_minutes must be between 30 and 720."
  }
}

variable "run_as" {
  type        = string
  default     = "rsi"
  description = "Unprivileged user that owns the toolchain and the warm cargo target."
  validation {
    condition     = can(regex("^[a-z_][a-z0-9_-]{0,31}$", var.run_as))
    error_message = "run_as must be a plain Linux user name."
  }
}

variable "target_dir" {
  type        = string
  default     = "/srv/rsi-pool/target"
  description = "Warm local cargo target directory on the root volume, shared by every build on the host for the length of its lease."
  validation {
    condition     = can(regex("^/[A-Za-z0-9_./-]+$", var.target_dir))
    error_message = "target_dir must be an absolute path of plain characters."
  }
}

variable "rust_toolchain" {
  type        = string
  default     = "1.94.1"
  description = "Match rust-toolchain.toml so the pool builds with the pinned compiler."
}

variable "nextest_version" {
  type        = string
  default     = "0.9.137"
  description = "cargo-nextest release installed from the upstream prebuilt archive."
  validation {
    condition     = can(regex("^[0-9]+\\.[0-9]+\\.[0-9]+$", var.nextest_version))
    error_message = "nextest_version must be an exact release such as 0.9.137."
  }
}

variable "instance_profile_name" {
  type        = string
  default     = ""
  description = "Existing instance profile to attach (the gate stack's rsi-gate-sccache). Empty attaches none: the trial runs without an sccache bucket (#3460 S4a is skipped), so the pool needs no IAM at all."
}
