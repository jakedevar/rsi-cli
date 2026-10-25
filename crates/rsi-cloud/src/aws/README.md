# Gate/pool AWS transport (#3952)

Before this change both cloud scripts export `AWS_PROFILE`, defaulting to
`rsi-cloud-terraform`, and pass `--region us-west-1` to AWS CLI v1. The existing
watchdog has its own explicit profile/region and separate CLI abstraction; its
wider API surface is outside this change.

`rsi-cloud aws` implements only the four operations/queries used by the gate and
pool. It preserves the instance id/state/state reason JSON projection, tabbed
leftover ids, decoded console `Output`, and head-bucket exit status. Describe
requests drain pagination before producing output. HTTP, credentials, XML,
pagination and read failures exit nonzero without stdout; the scripts overwrite
any failed command's captured output with `unknown` and fail cleanup. An empty
successful response alone means no leftovers.

Dependency decision: reuse the already locked `ring` 0.17 (HMAC/SHA-256),
workspace `reqwest` 0.12 with rustls plus its blocking feature, `quick-xml` 0.38.4
with serde, and workspace base64. No new crate package is needed. This avoids
the large EC2 SDK and a second signing/runtime dependency stack. The narrow
SigV4 construction supports only `/`, no query string, fixed AWS HTTPS hosts,
and EC2 URL-encoded POST bodies or S3 HEAD. Its crypto comes from ring; signing
is checked against four unmodified AWS-published vectors, including session
tokens and form payloads. No generic signer or arbitrary endpoint is exposed.

Credentials: absent an explicit `--profile`, environment access/secret/session
token wins; partial environment credentials fail. Then the selected profile
(`--profile`, `AWS_PROFILE`, `AWS_DEFAULT_PROFILE`, `default`) loads from
`AWS_SHARED_CREDENTIALS_FILE` or `~/.aws/credentials`, overriding static keys in
`AWS_CONFIG_FILE` or `~/.aws/config`. Region precedence is `--region`,
`AWS_REGION`, `AWS_DEFAULT_REGION`, profile config. HOME falls back to USERPROFILE
on Windows. Static and temporary credentials are supported. Role, SSO,
credential_process, container and metadata providers are unsupported and fail
explicitly; no subprocess can introduce Python. The deployed profile is assumed
to use shared static keys, as the existing Terraform lane does. No actual
credential file is inspected by development or tests.

Credentials have no Debug/Display implementation. Errors are fixed messages:
transport errors, AWS error bodies, headers and profile contents never enter
diagnostics. The HTTPS client disables redirects, uses platform-portable rustls,
and bounds connection/request time and response size. S3 buckets must be single
DNS labels (the existing cache naming); unsupported dotted buckets fail safely.

Offline fixtures: `tests/fixtures/aws/README.md` records source URLs and changes.
Tests do not contact AWS, run Terraform, or execute Python.
