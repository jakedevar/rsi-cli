# Persistent shared compile cache for the ephemeral remote gate host (Issue
# #1010). Operator rule 2026-09-28: no build volumes or snapshots survive a
# run, so the warm cache is a private S3 bucket that objects age out of after
# at most `expire_days` days. This is a separate root from infra/aws/gate so
# `terraform destroy` of the gate host never touches the bucket or the role:
#
#   terraform -chdir=infra/aws/gate-cache init
#   terraform -chdir=infra/aws/gate-cache apply -state="$HOME/.rsi/cloud/gate-cache.tfstate"
#
# scripts/cloud-gate.sh reads the outputs from that state file and passes them
# to the gate stack; without the state file a run is cold (no cache).

data "aws_caller_identity" "current" {}

locals {
  bucket_name = coalesce(var.bucket_name, "rsi-gate-sccache-${data.aws_caller_identity.current.account_id}-us-west-1")
  tags        = { Service = "rsi-gate-cache", Owner = "rsi-remote-gate" }
}

resource "aws_s3_bucket" "cache" {
  bucket        = local.bucket_name
  force_destroy = true # cache only: everything in it is reproducible from source
  tags          = local.tags
}

resource "aws_s3_bucket_public_access_block" "cache" {
  bucket                  = aws_s3_bucket.cache.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "cache" {
  bucket = aws_s3_bucket.cache.id
  rule { object_ownership = "BucketOwnerEnforced" }
}

resource "aws_s3_bucket_policy" "cache" {
  bucket = aws_s3_bucket.cache.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "DenyInsecureTransport"
      Effect    = "Deny"
      Principal = "*"
      Action    = "s3:*"
      Resource  = [aws_s3_bucket.cache.arn, "${aws_s3_bucket.cache.arn}/*"]
      Condition = { Bool = { "aws:SecureTransport" = "false" } }
    }]
  })
  depends_on = [aws_s3_bucket_public_access_block.cache]
}

resource "aws_s3_bucket_server_side_encryption_configuration" "cache" {
  bucket = aws_s3_bucket.cache.id
  rule {
    apply_server_side_encryption_by_default { sse_algorithm = "AES256" }
  }
}

# No versioning: an overwritten or expired cache object is simply gone.
resource "aws_s3_bucket_lifecycle_configuration" "cache" {
  bucket = aws_s3_bucket.cache.id
  rule {
    id     = "expire-cache"
    status = "Enabled"
    filter {}
    expiration { days = var.expire_days }
    abort_incomplete_multipart_upload { days_after_initiation = 1 }
  }
}

# The gate host's instance role: only the cache prefix in this one bucket. No
# SSM, no secrets, no other service. It lives here (not in the gate stack) so
# the per-run apply/destroy needs no IAM create/delete.
resource "aws_iam_role" "gate" {
  name = "rsi-gate-sccache"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
  tags = local.tags
}

resource "aws_iam_role_policy" "cache" {
  name = "sccache-bucket"
  role = aws_iam_role.gate.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject"], Resource = "${aws_s3_bucket.cache.arn}/${var.key_prefix}/*" },
      { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.cache.arn, Condition = { StringLike = { "s3:prefix" = ["${var.key_prefix}/*"] } } },
    ]
  })
}

resource "aws_iam_instance_profile" "gate" {
  name = "rsi-gate-sccache"
  role = aws_iam_role.gate.name
  tags = local.tags
}
