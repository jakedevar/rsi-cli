output "instance_id" { value = aws_instance.host.id }
output "instance_arn" { value = aws_instance.host.arn }
output "vpc_id" { value = local.effective_vpc_id }
output "subnet_id" { value = local.effective_subnet_id }
output "security_group_id" { value = aws_security_group.host.id }
output "public_ip_enabled" { value = local.public_ip }
output "public_ip" { value = aws_instance.host.public_ip }
output "data_volume_id" { value = aws_ebs_volume.data.id }
output "backup_bucket" { value = aws_s3_bucket.backups.bucket }
output "ssm_start_session" { value = "aws ssm start-session --region ${var.region} --target ${aws_instance.host.id}" }
output "secret_parameter_paths" { value = { for name, parameter in aws_ssm_parameter.operator_secret : name => parameter.name } }
