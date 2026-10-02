variable "ssh_key_name" {
  type        = string
  description = "Existing EC2 key pair whose private key the lander uses as --remote-gate-identity."
}

variable "network_prefix" {
  type        = string
  default     = "rsid-rsid"
  description = "Name/Service tag prefix of the persistent rsid-host network (VPC, public subnet, SSH security group) this host borrows."
}

variable "instance_type" {
  type        = string
  default     = "c7i.8xlarge"
  description = "Smallest type the operator's rsi-cloud-terraform-scope IAM policy allows (dry-run probes 2026-09-28: c7i.4xlarge denied; c7i.8xlarge and c7i.16xlarge allowed). The lander runs up to RSI_REMOTE_GATE_PARALLEL (default 4) shards at once, each at --jobs 4..6."
}

variable "root_volume_gib" {
  type        = number
  default     = 200
  description = "Encrypted gp3 root volume; it holds both sides' target directories and is deleted with the instance."
  validation {
    condition     = var.root_volume_gib >= 60 && var.root_volume_gib <= 500
    error_message = "root_volume_gib must be between 60 and 500."
  }
}

variable "max_lifetime_minutes" {
  type        = number
  default     = 240
  description = "The guest powers itself off after this long; shutdown terminates the instance and deletes its volume even if terraform destroy never runs."
  validation {
    condition     = var.max_lifetime_minutes >= 30 && var.max_lifetime_minutes <= 720
    error_message = "max_lifetime_minutes must be between 30 and 720."
  }
}

variable "run_as" {
  type        = string
  default     = "rsi"
  description = "Unprivileged user that owns the toolchain; pass it to the lander as --remote-gate-run-as."
  validation {
    condition     = can(regex("^[a-z_][a-z0-9_-]{0,31}$", var.run_as))
    error_message = "run_as must be a plain Linux user name."
  }
}

variable "rust_toolchain" {
  type        = string
  default     = "1.94.1"
  description = "Match rust-toolchain.toml so remote base and candidate build with the pinned compiler."
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

variable "sccache_version" {
  type        = string
  default     = "0.10.0"
  description = "sccache release installed from the upstream prebuilt musl archive."
  validation {
    condition     = can(regex("^[0-9]+\\.[0-9]+\\.[0-9]+$", var.sccache_version))
    error_message = "sccache_version must be an exact release such as 0.10.0."
  }
}

variable "sccache_sha256" {
  type        = string
  default     = "1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b"
  description = "SHA-256 of sccache-v<version>-x86_64-unknown-linux-musl.tar.gz; bootstrap refuses a download that does not match."
  validation {
    condition     = can(regex("^[0-9a-f]{64}$", var.sccache_sha256))
    error_message = "sccache_sha256 must be 64 lowercase hex digits."
  }
}

variable "sccache_bucket" {
  type        = string
  default     = ""
  description = "S3 bucket of the shared compile cache (output of infra/aws/gate-cache). Empty runs cold: no sccache, no instance profile. scripts/cloud-gate.sh fills it from ~/.rsi/cloud/gate-cache.tfstate."
}

variable "sccache_prefix" {
  type        = string
  default     = "sccache"
  description = "Key prefix inside the cache bucket; must match the gate-cache stack's key_prefix."
}

variable "instance_profile_name" {
  type        = string
  default     = "rsi-gate-sccache"
  description = "Instance profile created by infra/aws/gate-cache; attached only when sccache_bucket is set."
}
