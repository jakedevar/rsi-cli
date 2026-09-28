terraform {
  required_version = "= 1.13.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "= 6.14.0"
    }
  }

  # For team use, configure an encrypted remote backend before first apply.
  # backend "s3" {}
}

provider "aws" {
  region  = var.region
  profile = var.profile
}
