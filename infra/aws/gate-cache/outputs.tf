output "bucket" { value = aws_s3_bucket.cache.bucket }
output "region" { value = "us-west-1" }
output "key_prefix" { value = var.key_prefix }
output "instance_profile" { value = aws_iam_instance_profile.gate.name }
