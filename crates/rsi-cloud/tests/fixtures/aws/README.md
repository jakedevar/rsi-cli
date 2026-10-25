Offline AWS fixtures for #3952. No live account data or credentials.

The four vector directories are unmodified public test inputs and signatures
from AWS's Apache-2.0 SDK for Rust signing suite:
https://github.com/awslabs/aws-sdk-rust/tree/main/sdk/aws-sigv4/aws-signing-test-suite/v4
Each directory preserves context.json, request.txt and header-signature.txt.
All keys/tokens in those contexts are AWS's published test values.

The XML files record published AWS API example responses (2016-11-15 API):
- describe.xml: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeInstances.html
  Example 2; instance state and reason changed to a spot termination for the
  script's spot-reason projection; the public example IP is replaced with
  documentation address 203.0.113.10, all other example fields retained.
- console.xml: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_GetConsoleOutput.html
  Example 1, unchanged, including the line-wrapped base64 console output.
- terminate.xml: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_TerminateInstances.html
  Example 1, unchanged.

Tests derive empty/malformed/paginated responses from these recorded fixtures.
No fixture is obtained at test time; no AWS or Terraform process runs.
