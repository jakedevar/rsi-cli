# Warm spot pool (#3460 S4b): up to 6 long-lived spot hosts that the daemon
# places worker scoped_test runs and full-library QA on (S4c/S4d). Drive it
# with scripts/cloud-pool.sh, which reserves each lease in the spend ledger
# before apply and destroys expired or idle hosts.
#
# It adds no IAM, security-group or network exposure: it borrows the same
# persistent rsid-host VPC, public subnet and SSH-only security group as
# infra/aws/gate, in us-west-1a, and attaches no instance profile unless one is
# named. Spot only (one-time request capped at var.spot_max_price_usd, so AWS
# never bills an hour above it), terminated on interruption and on guest
# shutdown. Every instance and its root volume carries Service=rsi-pool and an
# rsi:expires-at tag equal to the lease expiry, so the orphan sweep
# (rsi-cloud watchdog) terminates a host the hub forgot; the guest also
# powers itself off at the lease end.
#
# At most 6 hosts: var.hosts is validated to hold <= 6 slots, and the
# precondition below holds the subnet to us-west-1a. A map (not a bare count)
# lets one host be dropped without renumbering the others.

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

resource "aws_instance" "pool" {
  for_each = var.hosts

  ami                         = data.aws_ssm_parameter.ami.value
  instance_type               = var.instance_type
  subnet_id                   = data.aws_subnet.shared.id
  vpc_security_group_ids      = [data.aws_security_group.shared.id]
  associate_public_ip_address = true
  key_name                    = var.ssh_key_name
  # No instance_initiated_shutdown_behavior: AWS refuses to modify it on spot
  # instances (UnsupportedOperation, #3977) and a one-time spot instance
  # terminates on guest shutdown anyway.
  iam_instance_profile        = var.instance_profile_name == "" ? null : var.instance_profile_name
  user_data_replace_on_change = true
  instance_market_options {
    market_type = "spot"
    spot_options {
      max_price                      = tostring(var.spot_max_price_usd)
      instance_interruption_behavior = "terminate"
      spot_instance_type             = "one-time"
    }
  }
  # Nothing here may depend on the lease expiry: a renewal changes only the
  # tags below, never user_data (that would replace the running host).
  user_data = templatefile("${path.module}/bootstrap.sh.tftpl", {
    lease_minutes   = var.lease_minutes
    run_as          = var.run_as
    target_dir      = var.target_dir
    rust_toolchain  = var.rust_toolchain
    nextest_version = var.nextest_version
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
  tags        = { Name = "rsi-${each.key}", Service = "rsi-pool", "rsi:expires-at" = each.value }
  volume_tags = { Name = "rsi-${each.key}", Service = "rsi-pool", "rsi:expires-at" = each.value }

  lifecycle {
    precondition {
      condition     = data.aws_subnet.shared.availability_zone == "us-west-1a"
      error_message = "The pool runs in us-west-1a; the shared public subnet is in ${data.aws_subnet.shared.availability_zone}."
    }
    ignore_changes = [ami]
  }
}
