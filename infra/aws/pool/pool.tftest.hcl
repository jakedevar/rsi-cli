mock_provider "aws" {
  mock_data "aws_vpc" {
    defaults = { id = "vpc-0123456789abcdef0" }
  }
  mock_data "aws_subnet" {
    defaults = { id = "subnet-0123456789abcdef0", availability_zone = "us-west-1a" }
  }
  mock_data "aws_security_group" {
    defaults = { id = "sg-0123456789abcdef0" }
  }
  mock_data "aws_ssm_parameter" {
    defaults = { value = "ami-0123456789abcdef0" }
  }
}

variables {
  ssh_key_name = "test-key"
}

run "six_hosts_are_allowed" {
  command = plan
  variables {
    hosts = {
      "pool-1" = "2030-01-01T12:00:00Z"
      "pool-2" = "2030-01-01T12:00:00Z"
      "pool-3" = "2030-01-01T12:00:00Z"
      "pool-4" = "2030-01-01T12:00:00Z"
      "pool-5" = "2030-01-01T12:00:00Z"
      "pool-6" = "2030-01-01T12:00:00Z"
    }
  }
  assert {
    condition     = length(aws_instance.pool) == 6
    error_message = "six hosts must plan six instances"
  }
  assert {
    condition     = alltrue([for host in aws_instance.pool : host.tags["rsi:expires-at"] == "2030-01-01T12:00:00Z" && host.volume_tags["rsi:expires-at"] == "2030-01-01T12:00:00Z"])
    error_message = "every instance and volume carries the lease expiry"
  }
  assert {
    condition     = alltrue([for host in aws_instance.pool : host.instance_market_options[0].market_type == "spot" && host.instance_market_options[0].spot_options[0].spot_instance_type == "one-time" && host.instance_market_options[0].spot_options[0].instance_interruption_behavior == "terminate"])
    error_message = "one-time spot, terminated on interruption and on guest shutdown (#3977: AWS refuses instance_initiated_shutdown_behavior on spot)"
  }
}

run "seven_hosts_are_refused" {
  command = plan
  variables {
    hosts = {
      "pool-1" = "2030-01-01T12:00:00Z"
      "pool-2" = "2030-01-01T12:00:00Z"
      "pool-3" = "2030-01-01T12:00:00Z"
      "pool-4" = "2030-01-01T12:00:00Z"
      "pool-5" = "2030-01-01T12:00:00Z"
      "pool-6" = "2030-01-01T12:00:00Z"
      "pool-7" = "2030-01-01T12:00:00Z"
    }
  }
  expect_failures = [var.hosts]
}

run "another_zone_is_refused" {
  command = plan
  variables {
    hosts = { "pool-1" = "2030-01-01T12:00:00Z" }
  }
  override_data {
    target = data.aws_subnet.shared
    values = { id = "subnet-0123456789abcdef0", availability_zone = "us-west-1b" }
  }
  expect_failures = [aws_instance.pool["pool-1"]]
}

run "spot_price_above_the_reserved_cap_is_refused" {
  command = plan
  variables {
    hosts              = { "pool-1" = "2030-01-01T12:00:00Z" }
    spot_max_price_usd = 0.80
  }
  expect_failures = [var.spot_max_price_usd]
}

run "root_volume_above_the_reserved_size_is_refused" {
  command = plan
  variables {
    hosts           = { "pool-1" = "2030-01-01T12:00:00Z" }
    root_volume_gib = 300
  }
  expect_failures = [var.root_volume_gib]
}
