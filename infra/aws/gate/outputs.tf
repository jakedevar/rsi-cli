output "instance_id" { value = aws_instance.gate.id }
output "public_ip" { value = aws_instance.gate.public_ip }
output "instance_type" { value = aws_instance.gate.instance_type }
output "run_as" { value = var.run_as }
