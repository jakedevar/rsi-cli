variable "bucket_name" {
  type        = string
  default     = null
  description = "Override the cache bucket name; the default embeds the account id and region so it is globally unique."
}

variable "key_prefix" {
  type        = string
  default     = "sccache"
  description = "Object key prefix sccache uses; the instance role can read and write only below it."
  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9-]{0,30}$", var.key_prefix))
    error_message = "key_prefix must be a short lowercase path segment."
  }
}

variable "expire_days" {
  type        = number
  default     = 14
  description = "Objects expire this many days after they were written. Operator rule: keep nothing durable."
  validation {
    condition     = var.expire_days >= 1 && var.expire_days <= 14
    error_message = "expire_days must be between 1 and 14."
  }
}
