module "rsid_host" {
  source                      = "../modules/rsid-host"
  region                      = var.region
  vpc_id                      = var.vpc_id
  subnet_id                   = var.subnet_id
  associate_public_ip_address = var.associate_public_ip_address
  ssh_ingress_cidrs           = var.ssh_ingress_cidrs
  ssh_key_name                = var.ssh_key_name
  prefix                      = var.prefix
  instance_type               = var.instance_type
  architecture                = var.architecture
  data_volume_gib             = var.data_volume_gib
  data_snapshot_id            = var.data_snapshot_id
  budget_usd                  = var.budget_usd
  budget_email                = var.budget_email
  source_repo_url             = var.source_repo_url
  source_ref                  = var.source_ref
  install_mode                = var.install_mode
  artifact_url                = var.artifact_url
  artifact_sha256             = var.artifact_sha256
  secret_names                = var.secret_names
  operator_secrets            = var.operator_secrets
  secret_versions             = var.secret_versions
  provider_cli_npm_packages   = var.provider_cli_npm_packages
  enable_tailscale            = var.enable_tailscale
  stop_when_idle_minutes      = var.stop_when_idle_minutes
  shutdown_at_utc             = var.shutdown_at_utc
  tags                        = { ManagedBy = "Terraform" }
}

module "client_vpn" {
  count                     = var.enable_client_vpn ? 1 : 0
  source                    = "../modules/client-vpn"
  prefix                    = var.prefix
  vpc_id                    = module.rsid_host.vpc_id
  target_subnet_id          = module.rsid_host.subnet_id
  client_cidr               = var.vpn_client_cidr
  authorized_cidr           = var.vpn_authorized_cidr
  server_certificate_arn    = var.vpn_server_certificate_arn
  client_ca_certificate_arn = var.vpn_client_ca_certificate_arn
  tags                      = { ManagedBy = "Terraform" }
}

# AWS Client VPN translates client source IPs. Its association SG, rather than
# the VPN client CIDR, is the source to allow on the host for SSH.
resource "aws_vpc_security_group_ingress_rule" "vpn_ssh" {
  count                        = var.enable_vpn_ssh ? 1 : 0
  security_group_id            = module.rsid_host.security_group_id
  referenced_security_group_id = var.enable_client_vpn ? module.client_vpn[0].security_group_id : var.existing_vpn_security_group_id
  ip_protocol                  = "tcp"
  from_port                    = 22
  to_port                      = 22
  description                  = "Explicitly enabled Client VPN SSH"
}
