output "endpoint_id" { value = aws_ec2_client_vpn_endpoint.main.id }
output "dns_name" { value = aws_ec2_client_vpn_endpoint.main.dns_name }
output "association_id" { value = aws_ec2_client_vpn_network_association.target.id }
output "security_group_id" { value = aws_security_group.vpn.id }
output "log_group_name" { value = aws_cloudwatch_log_group.connections.name }
