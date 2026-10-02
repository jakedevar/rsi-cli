terraform {
  required_version = "= 1.13.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "= 6.14.0"
    }
  }
}

provider "aws" {
  region              = "us-west-1"
  profile             = "rsi-cloud-terraform"
  allowed_account_ids = ["000000000000"]
  default_tags {
    tags = { Owner = "rsi-cloud-tonight" }
  }
}
