module "rsid_host" {
  source          = "../modules/rsid-host"
  region          = "us-west-1"
  prefix          = "rsid"
  instance_type   = "c7i.16xlarge"
  architecture    = "x86_64"
  data_volume_gib = 200
  # Operator 2026-09-28: build volumes are deleted when done, never snapshotted.
  data_volume_final_snapshot = false
  ssh_key_name               = var.ssh_key_name
  ssh_ingress_cidrs          = [var.ssh_ingress_cidr]
  source_ref                 = var.source_ref
  install_mode               = "deferred"
  secret_names               = ["openrouter_api_key"]
  operator_secrets           = var.operator_secrets
  enable_tailscale           = false
  budget_usd                 = 200
  budget_email               = "operator@example.com"
  stop_when_idle_minutes     = 45
  shutdown_at_utc            = "2026-09-27T16:00:00Z"
  tags                       = { ManagedBy = "Terraform" }
}
