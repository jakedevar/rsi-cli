resource "aws_cloudwatch_log_group" "connections" {
  name              = "/${var.prefix}/client-vpn"
  retention_in_days = 30
  tags              = var.tags
}

resource "aws_cloudwatch_log_stream" "connections" {
  name           = "connections"
  log_group_name = aws_cloudwatch_log_group.connections.name
}

resource "aws_security_group" "vpn" {
  name_prefix = "${var.prefix}-client-vpn-"
  description = "Client VPN association traffic"
  vpc_id      = var.vpc_id
  tags        = var.tags
}

resource "aws_vpc_security_group_egress_rule" "ssh" {
  security_group_id = aws_security_group.vpn.id
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = var.authorized_cidr
}

resource "aws_ec2_client_vpn_endpoint" "main" {
  description            = "${var.prefix} rsid access"
  server_certificate_arn = var.server_certificate_arn
  client_cidr_block      = var.client_cidr
  vpc_id                 = var.vpc_id
  security_group_ids     = [aws_security_group.vpn.id]
  split_tunnel           = true
  transport_protocol     = "udp"
  vpn_port               = 443
  self_service_portal    = "disabled"

  authentication_options {
    type                       = "certificate-authentication"
    root_certificate_chain_arn = var.client_ca_certificate_arn
  }
  connection_log_options {
    enabled               = true
    cloudwatch_log_group  = aws_cloudwatch_log_group.connections.name
    cloudwatch_log_stream = aws_cloudwatch_log_stream.connections.name
  }
  tags = var.tags
}

resource "aws_ec2_client_vpn_network_association" "target" {
  client_vpn_endpoint_id = aws_ec2_client_vpn_endpoint.main.id
  subnet_id              = var.target_subnet_id
}

resource "aws_ec2_client_vpn_authorization_rule" "target" {
  client_vpn_endpoint_id = aws_ec2_client_vpn_endpoint.main.id
  target_network_cidr    = var.authorized_cidr
  authorize_all_groups   = true
  description            = "Mutual-TLS clients may reach the selected network"
  depends_on             = [aws_ec2_client_vpn_network_association.target]
}
