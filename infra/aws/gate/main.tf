# Ephemeral remote shard-gate host (operator rule 2026-09-28: apply, run one
# remote gate, destroy; keep no build volumes or snapshots). Drive it with
# scripts/cloud-gate.sh, which always destroys on exit.
#
# The host borrows the persistent rsid-host network (VPC, public subnet and
# the security group whose only ingress is the operator's SSH /32), so it adds
# no network exposure. It has no secrets and no data volume: the root volume
# holds the build and is deleted on termination. When the persistent
# infra/aws/gate-cache stack exists it also gets that stack's instance profile,
# whose only permissions are read/write on the sccache prefix of one private
# S3 bucket (the warm compile cache, Issue #1010); without it the host has no
# instance profile and builds cold.

data "aws_vpc" "shared" {
  tags = { Name = "${var.network_prefix}-vpc" }
}

data "aws_subnet" "shared" {
  vpc_id = data.aws_vpc.shared.id
  tags   = { Name = "${var.network_prefix}-public" }
}

data "aws_security_group" "shared" {
  vpc_id = data.aws_vpc.shared.id
  tags   = { Service = var.network_prefix }
}

data "aws_ssm_parameter" "ami" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
}

resource "aws_instance" "gate" {
  ami                                  = data.aws_ssm_parameter.ami.value
  instance_type                        = var.instance_type
  subnet_id                            = data.aws_subnet.shared.id
  vpc_security_group_ids               = [data.aws_security_group.shared.id]
  associate_public_ip_address          = true
  key_name                             = var.ssh_key_name
  instance_initiated_shutdown_behavior = "terminate"
  iam_instance_profile                 = var.sccache_bucket == "" ? null : var.instance_profile_name
  user_data_replace_on_change          = true
  user_data = templatefile("${path.module}/bootstrap.sh.tftpl", {
    max_lifetime_minutes = var.max_lifetime_minutes
    run_as               = var.run_as
    rust_toolchain       = var.rust_toolchain
    nextest_version      = var.nextest_version
    sccache_version      = var.sccache_version
    sccache_sha256       = var.sccache_sha256
    sccache_bucket       = var.sccache_bucket
    sccache_prefix       = var.sccache_prefix
  })
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }
  root_block_device {
    encrypted             = true
    volume_type           = "gp3"
    volume_size           = var.root_volume_gib
    delete_on_termination = true
  }
  tags = { Name = "rsi-remote-gate", Service = "rsi-remote-gate" }
}
