output "hosts" {
  description = "Per slot: instance id, public IP and the lease expiry tag. scripts/cloud-pool.sh reads this after each apply."
  value = {
    for slot, host in aws_instance.pool : slot => {
      instance_id = host.id
      public_ip   = host.public_ip
      expires_at  = var.hosts[slot]
    }
  }
}

output "instance_type" { value = var.instance_type }
output "run_as" { value = var.run_as }
output "target_dir" { value = var.target_dir }
