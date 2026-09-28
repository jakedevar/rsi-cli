data "aws_availability_zones" "available" {
  count = var.vpc_id == null ? 1 : 0
  state = "available"
}

data "aws_subnet" "existing" {
  count = var.subnet_id == null ? 0 : 1
  id    = var.subnet_id
}

data "aws_ssm_parameter" "ami" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-${var.architecture}"
}

data "aws_ec2_instance_type" "build" {
  count         = var.install_mode == "build" ? 1 : 0
  instance_type = var.instance_type
}

locals {
  name                = "${var.prefix}-rsid"
  secret_base         = "/${var.prefix}/rsid"
  secret_arns         = [for name in var.secret_names : "arn:${data.aws_partition.current.partition}:ssm:${var.region}:${data.aws_caller_identity.current.account_id}:parameter${local.secret_base}/${name}"]
  common_tags         = merge(var.tags, { Service = local.name })
  effective_vpc_id    = var.vpc_id == null ? aws_vpc.main[0].id : var.vpc_id
  effective_subnet_id = var.subnet_id == null ? aws_subnet.public[0].id : var.subnet_id
  effective_az        = var.subnet_id == null ? aws_subnet.public[0].availability_zone : data.aws_subnet.existing[0].availability_zone
  public_ip           = var.associate_public_ip_address == null ? var.vpc_id == null : var.associate_public_ip_address
}

data "aws_caller_identity" "current" {}
data "aws_partition" "current" {}

data "aws_iam_policy" "ssm_core" {
  arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_vpc" "main" {
  count                = var.vpc_id == null ? 1 : 0
  cidr_block           = "10.61.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags                 = merge(local.common_tags, { Name = "${local.name}-vpc" })
}

resource "aws_subnet" "public" {
  count                   = var.vpc_id == null ? 1 : 0
  vpc_id                  = aws_vpc.main[0].id
  cidr_block              = "10.61.1.0/24"
  availability_zone       = data.aws_availability_zones.available[0].names[0]
  map_public_ip_on_launch = true
  tags                    = merge(local.common_tags, { Name = "${local.name}-public" })
}

resource "aws_internet_gateway" "main" {
  count  = var.vpc_id == null ? 1 : 0
  vpc_id = aws_vpc.main[0].id
  tags   = local.common_tags
}

resource "aws_route_table" "public" {
  count  = var.vpc_id == null ? 1 : 0
  vpc_id = aws_vpc.main[0].id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.main[0].id
  }
  tags = local.common_tags
}

resource "aws_route_table_association" "public" {
  count          = var.vpc_id == null ? 1 : 0
  subnet_id      = aws_subnet.public[0].id
  route_table_id = aws_route_table.public[0].id
}

resource "aws_security_group" "host" {
  name_prefix = "${local.name}-"
  description = "Outbound only; SSM Session Manager is operator access"
  vpc_id      = local.effective_vpc_id
  tags        = local.common_tags
}

resource "aws_vpc_security_group_ingress_rule" "ssh_ipv4" {
  for_each          = { for cidr in var.ssh_ingress_cidrs : cidr => cidr if !strcontains(cidr, ":") }
  security_group_id = aws_security_group.host.id
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = each.value
  description       = "Operator-approved SSH source"
}

resource "aws_vpc_security_group_ingress_rule" "ssh_ipv6" {
  for_each          = { for cidr in var.ssh_ingress_cidrs : cidr => cidr if strcontains(cidr, ":") }
  security_group_id = aws_security_group.host.id
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv6         = each.value
  description       = "Operator-approved SSH source"
}

resource "aws_vpc_security_group_egress_rule" "https" {
  security_group_id = aws_security_group.host.id
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_egress_rule" "git_ssh" {
  security_group_id = aws_security_group.host.id
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_egress_rule" "tailscale" {
  count             = var.enable_tailscale ? 1 : 0
  security_group_id = aws_security_group.host.id
  ip_protocol       = "udp"
  from_port         = 1
  to_port           = 65535
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_egress_rule" "dns_udp" {
  security_group_id = aws_security_group.host.id
  ip_protocol       = "udp"
  from_port         = 53
  to_port           = 53
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_egress_rule" "dns_tcp" {
  security_group_id = aws_security_group.host.id
  ip_protocol       = "tcp"
  from_port         = 53
  to_port           = 53
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_s3_bucket" "backups" {
  bucket_prefix = "${local.name}-backup-"
  force_destroy = true # Explicit terraform destroy removes backups; final EBS snapshot remains.
  tags          = local.common_tags
}

resource "aws_s3_bucket_public_access_block" "backups" {
  bucket                  = aws_s3_bucket.backups.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_policy" "backups" {
  bucket = aws_s3_bucket.backups.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "DenyInsecureTransport"
      Effect    = "Deny"
      Principal = "*"
      Action    = "s3:*"
      Resource  = [aws_s3_bucket.backups.arn, "${aws_s3_bucket.backups.arn}/*"]
      Condition = { Bool = { "aws:SecureTransport" = "false" } }
    }]
  })
  depends_on = [aws_s3_bucket_public_access_block.backups]
}

resource "aws_s3_bucket_server_side_encryption_configuration" "backups" {
  bucket = aws_s3_bucket.backups.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_versioning" "backups" {
  bucket = aws_s3_bucket.backups.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_lifecycle_configuration" "backups" {
  bucket = aws_s3_bucket.backups.id
  rule {
    id     = "expire-old-versions"
    status = "Enabled"
    filter { prefix = "backups/" }
    noncurrent_version_expiration { noncurrent_days = 90 }
    abort_incomplete_multipart_upload { days_after_initiation = 7 }
  }
  depends_on = [aws_s3_bucket_versioning.backups]
}

resource "aws_cloudwatch_log_group" "rsid" {
  name              = "/${var.prefix}/rsid"
  retention_in_days = 30
  tags              = local.common_tags
}

resource "aws_iam_role" "host" {
  name_prefix = "${local.name}-"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
  tags = local.common_tags
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.host.name
  policy_arn = data.aws_iam_policy.ssm_core.arn
}

resource "aws_iam_role_policy" "runtime" {
  name = "runtime"
  role = aws_iam_role.host.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat(
      length(local.secret_arns) == 0 ? [] : [{ Effect = "Allow", Action = ["ssm:GetParameter", "ssm:GetParameters"], Resource = local.secret_arns }],
      [
        { Effect = "Allow", Action = ["s3:PutObject", "s3:GetObject"], Resource = "${aws_s3_bucket.backups.arn}/backups/*" },
        { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.backups.arn, Condition = { StringLike = { "s3:prefix" = ["backups/*"] } } },
        { Effect = "Allow", Action = ["logs:CreateLogStream", "logs:PutLogEvents", "logs:DescribeLogStreams"], Resource = "${aws_cloudwatch_log_group.rsid.arn}:*" },
        { Effect = "Allow", Action = ["cloudwatch:PutMetricData"], Resource = "*", Condition = { StringEquals = { "cloudwatch:namespace" = "${var.prefix}/rsid" } } }
      ]
    )
  })
}

resource "aws_iam_role_policy" "idle_stop" {
  count = var.stop_when_idle_minutes > 0 ? 1 : 0
  name  = "idle-stop"
  role  = aws_iam_role.host.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "ec2:StopInstances", Resource = aws_instance.host.arn }]
  })
}

resource "aws_iam_instance_profile" "host" {
  name_prefix = "${local.name}-"
  role        = aws_iam_role.host.name
  tags        = local.common_tags
}

resource "aws_ssm_parameter" "operator_secret" {
  for_each         = var.secret_names
  name             = "${local.secret_base}/${each.key}"
  type             = "SecureString"
  value_wo         = var.operator_secrets[each.key]
  value_wo_version = lookup(var.secret_versions, each.key, 1)
  tags             = local.common_tags
}

resource "aws_ebs_volume" "data" {
  availability_zone = local.effective_az
  size              = var.data_volume_gib
  snapshot_id       = var.data_snapshot_id
  type              = "gp3"
  encrypted         = true
  final_snapshot    = true
  tags              = merge(local.common_tags, { Name = "${local.name}-data" })
}

resource "aws_instance" "host" {
  ami                                  = data.aws_ssm_parameter.ami.value
  instance_type                        = var.instance_type
  subnet_id                            = local.effective_subnet_id
  vpc_security_group_ids               = [aws_security_group.host.id]
  associate_public_ip_address          = local.public_ip
  key_name                             = var.ssh_key_name
  iam_instance_profile                 = aws_iam_instance_profile.host.name
  instance_initiated_shutdown_behavior = "stop"
  user_data_replace_on_change          = true
  user_data = templatefile("${path.module}/bootstrap.sh.tftpl", {
    volume_id              = aws_ebs_volume.data.id
    region                 = var.region
    prefix                 = var.prefix
    bucket                 = aws_s3_bucket.backups.bucket
    source_repo_url        = var.source_repo_url == null ? "" : var.source_repo_url
    source_ref             = var.source_ref
    install_mode           = var.install_mode
    architecture           = var.architecture
    artifact_url           = var.artifact_url == null ? "" : var.artifact_url
    artifact_sha256        = var.artifact_sha256 == null ? "" : var.artifact_sha256
    secret_names           = sort(tolist(var.secret_names))
    enable_tailscale       = var.enable_tailscale
    stop_when_idle_minutes = var.stop_when_idle_minutes
    shutdown_at_utc        = var.shutdown_at_utc
    log_group              = aws_cloudwatch_log_group.rsid.name
  })
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }
  root_block_device {
    encrypted   = true
    volume_type = "gp3"
    volume_size = 30
  }
  lifecycle {
    precondition {
      condition     = var.enable_tailscale == false || contains(var.secret_names, "tailscale_auth_key")
      error_message = "enable_tailscale requires tailscale_auth_key in secret_names."
    }
    precondition {
      condition     = var.vpc_id == null ? true : data.aws_subnet.existing[0].vpc_id == var.vpc_id
      error_message = "The selected subnet must belong to vpc_id."
    }
    precondition {
      condition     = var.install_mode != "artifact" || (var.artifact_url != null && var.artifact_sha256 != null)
      error_message = "Artifact installation requires artifact_url and artifact_sha256 for the selected architecture."
    }
    precondition {
      condition     = var.install_mode != "build" || var.source_repo_url != null
      error_message = "On-host build requires source_repo_url."
    }
    precondition {
      condition = var.install_mode == "build" ? (
        data.aws_ec2_instance_type.build[0].default_vcpus >= 4 &&
        data.aws_ec2_instance_type.build[0].memory_size >= 16384
      ) : true
      error_message = "On-host build requires an instance with at least 4 vCPUs and 16 GiB memory."
    }
  }
  tags = merge(local.common_tags, { Name = local.name })
  depends_on = [
    aws_iam_role_policy_attachment.ssm,
    aws_iam_role_policy.runtime,
    aws_ssm_parameter.operator_secret,
    aws_s3_bucket_public_access_block.backups,
    aws_s3_bucket_policy.backups,
    aws_s3_bucket_server_side_encryption_configuration.backups,
    aws_s3_bucket_versioning.backups,
  ]
}

resource "aws_volume_attachment" "data" {
  device_name = "/dev/sdf"
  volume_id   = aws_ebs_volume.data.id
  instance_id = aws_instance.host.id
}

resource "aws_cloudwatch_metric_alarm" "status" {
  alarm_name          = "${local.name}-status"
  namespace           = "AWS/EC2"
  metric_name         = "StatusCheckFailed"
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 2
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { InstanceId = aws_instance.host.id }
  tags                = local.common_tags
}

resource "aws_cloudwatch_metric_alarm" "disk" {
  alarm_name          = "${local.name}-disk"
  namespace           = "${var.prefix}/rsid"
  metric_name         = "DiskUsedPercent"
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 2
  threshold           = 85
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { InstanceId = aws_instance.host.id }
  treat_missing_data  = "breaching"
  tags                = local.common_tags
}

resource "aws_cloudwatch_metric_alarm" "service" {
  alarm_name          = "${local.name}-service"
  namespace           = "${var.prefix}/rsid"
  metric_name         = "ServiceFailed"
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 2
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { InstanceId = aws_instance.host.id }
  treat_missing_data  = "breaching"
  tags                = local.common_tags
}

resource "aws_cloudwatch_metric_alarm" "backup_age" {
  alarm_name          = "${local.name}-backup-age"
  namespace           = "${var.prefix}/rsid"
  metric_name         = "BackupAgeHours"
  statistic           = "Maximum"
  period              = 3600
  evaluation_periods  = 2
  threshold           = 30
  comparison_operator = "GreaterThanThreshold"
  dimensions          = { InstanceId = aws_instance.host.id }
  treat_missing_data  = "breaching"
  tags                = local.common_tags
}

resource "aws_budgets_budget" "monthly" {
  name         = "${local.name}-monthly"
  budget_type  = "COST"
  limit_amount = tostring(var.budget_usd)
  limit_unit   = "USD"
  time_unit    = "MONTHLY"
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.budget_email]
  }
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.budget_email]
  }
  tags = local.common_tags
}
