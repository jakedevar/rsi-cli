# AWS rsid host

This Terraform example provisions one Linux EC2 host for `rsid`, with local SQLite and worktrees on an encrypted gp3 data volume. Operator access uses AWS Systems Manager Session Manager. By default, the instance security group has no inbound rules; the host needs outbound HTTPS for SSM, Git, packages and provider APIs. An existing VPC/subnet and restricted SSH ingress can be selected for a work network. It is a single-host deployment, so an Availability Zone outage stops work until a replacement is restored. The module and example are under [`infra/aws/`](../infra/aws/).

No AWS resources were created when this configuration was prepared. Applying it creates billable resources in the AWS account selected by the operator. The example has no remote-state backend by default; protect the local Terraform state as account infrastructure data, or configure the commented backend using your organization's approved state bucket and locking setup.

## Prerequisites

- An AWS account and an operator IAM identity allowed to create VPC, EC2, EBS, IAM instance-profile, S3, CloudWatch and Budgets resources in the chosen Region. Check the account's EC2 quota and the selected instance type's availability before applying. Use an IAM deployment identity, not account-root credentials.
- Terraform matching the example's `required_version`, AWS CLI v2 and the Session Manager plugin on the operator's machine. Configure the intended AWS profile and Region. Check `aws sts get-caller-identity --profile <profile>` and the Region before spending; use your organization's AWS access process.
- A pinned, immutable Git commit containing the rsid source and a prebuilt bundle for the selected host architecture. The default boot path downloads the bundle over HTTPS, checks its SHA-256 digest and embedded commit/architecture, then runs `rsid` as the non-root `rsi` user. Review the commit, bundle and dependencies before applying. On-host builds are an explicit fallback for hosts with at least 4 vCPUs and 16 GiB memory.
- Secret values supplied by the operator during Terraform plan and apply through a password manager or secret CI job into the ephemeral `TF_VAR_operator_secrets` input. The module creates SSM `SecureString` parameters with Terraform write-only `value_wo`; secret values are omitted from state and plan. Never put them in `*.tfvars`, a CLI `-var` argument, saved plan, logs or this repository. Protect the apply process environment. The instance role reads only the selected parameter paths; rotate values by incrementing `secret_versions`.
- For a work VPC, an existing VPC/subnet with outbound routes and an approved VPN route to the host; check work network policy before enabling SSH. The optional new Client VPN endpoint needs ACM server and client CA certificate ARNs. Keep all private keys outside Terraform.
- A budget notification email. The module creates CloudWatch alarms for dashboard inspection without alarm actions; add an approved notification action if the team needs external alerts. Budget notifications are delayed spending signals, not a hard spending cap.

The host is intended to run a local TUI on the instance. It does not expose the daemon's Unix socket over the network. Choose an operator access path:

| Path | Network setting | Operator action |
| --- | --- | --- |
| SSM Session Manager | Default; no inbound rule | Start a session by instance ID. |
| Tailscale | Optional join; no inbound VPC rule needed | Put the auth key in SSM, enable join, and restrict tailnet ACLs. |
| Existing AWS Client VPN + SSH | Bring your VPC/subnet; explicit SSH CIDRs | Use the work VPN's client range and a reviewed SSH key/identity. Restrict the rule to that range; the module rejects `0.0.0.0/0` and `::/0`. |
| New AWS Client VPN + SSH | Optional endpoint, off by default | Supply ACM certificate ARNs and client CIDR; configure VPN authorization, client certificate distribution and SSH ingress from the dedicated VPN association security group. |

Prefer an existing work VPN over a new endpoint. A new endpoint creates recurring charges and may need additional organizational controls. AWS Client VPN translates client source IPs to the endpoint address, so an SSH rule using only the client CIDR may fail for a new endpoint; use the dedicated VPN association security group as source. For an existing work VPN, have the network team confirm the effective source range or security group before enabling a listed CIDR. See [AWS Client VPN network behavior](https://docs.aws.amazon.com/vpn/latest/clientvpn-admin/what-is-best-practices.html). SSM and Tailscale can remain usable alongside VPN access. For satellite use (#852), the host can establish a peer link through any approved path, but the rsid RPC socket must remain local until an authenticated remote protocol is explicitly deployed.

## Cost estimates

Prices below are **estimates as of 2026-09-27** for Linux on-demand use in `us-west-1`, 730 hours/month. They are planning figures, not a quote for another Region or account. The savings-plan column models a **30% lower compute rate** for one year of full utilization; it is illustrative because actual Savings Plans terms, payment options, coverage and commitments vary. A one-year commitment keeps billing through an idle or destroyed temporary deployment, so on-demand is the sensible initial choice.

| Instance | Architecture | vCPU / GiB | On-demand compute / month | Illustrative savings-plan compute / month |
| --- | --- | ---: | ---: | ---: |
| `m6g.large` | Graviton arm64 | 2 / 8 | $65.41 | ~$45.79 |
| `m6i.large` | x86_64 | 2 / 8 | $81.76 | ~$57.23 |
| `m6g.xlarge` | Graviton arm64 | 4 / 16 | $130.82 | ~$91.57 |
| `m6i.xlarge` | x86_64 | 4 / 16 | $163.52 | ~$114.46 |

The compute rates are from the [m6g.large](https://aws-pricing.com/m6g.large.html), [m6i.large](https://aws-pricing.com/m6i.large.html), [m6g.xlarge](https://aws-pricing.com/m6g.xlarge.html) and [m6i.xlarge](https://aws-pricing.com/m6i.xlarge.html) regional tables. AWS describes the [Savings Plans commitment model](https://aws.amazon.com/savingsplans/compute-pricing/); use the AWS Pricing Calculator or account-specific quote before purchasing a plan.

Add the root and data gp3 volumes. At an illustrative **$0.08/GB-month**, the example default of 30 GiB root plus 100 GiB data costs about **$10.40/month** before snapshots and extra provisioned performance. Increasing the data volume to 300 GiB adds about $16/month. A public IPv4 address adds **$0.005/hour**, about **$3.65/month** at 730 hours. With those assumptions, the four on-demand hosts above total roughly **$79, $96, $145 and $178/month**, respectively, before S3 backups, EBS snapshots, CloudWatch, Budgets, data transfer, taxes and any secret/KMS charges. EBS and retained snapshots continue billing while the instance is stopped. Rates vary by Region and volume allocation; see [EBS pricing](https://aws.amazon.com/ebs/pricing/) and [public IPv4 pricing](https://aws.amazon.com/vpc/pricing/).

An optional new AWS Client VPN endpoint has separate charges: the [AWS pricing example](https://aws.amazon.com/vpn/pricing/) uses **$0.10 per endpoint-association hour** and **$0.05 per connected client-hour** in US East (Ohio). At 730 hours that illustrates about **$73/month** for one endpoint association plus **$36.50/month** for one continuously connected client, before public IPv4, logs and data transfer. These are example rates, not a `us-west-1` quote; check the target Region's pricing before enabling it. An existing work VPN avoids a new endpoint's charges in this stack.

Stopping an instance stops most EC2 compute charges but keeps EBS charges. A stopped instance normally receives a new public IPv4 address on restart; connect by instance ID through SSM instead of relying on the address. See the [EC2 lifecycle documentation](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-instance-lifecycle.html).

## Local validation

From each of `infra/aws/modules/rsid-host` and `infra/aws/example`, run `terraform init -backend=false` followed by `terraform validate`. Validate `infra/aws/modules/client-vpn` too when that optional module is present. From `infra/aws`, run `terraform fmt -check -recursive`. These commands validate local syntax and provider schemas without creating resources. No live-account plan or apply was part of preparing this example.

## Prebuilt binaries

The default `install_mode="artifact"` avoids a first-boot Rust build on the example's 2 vCPU / 8 GiB host. In a clean checkout at the desired full `source_ref`, run the local build script on a Linux build machine with Rust and `cross` installed. Native builds use Cargo; the other architecture uses `cross`. From a distribution newer than the host (for example Arch), set `RSI_ARTIFACT_BUILDER=zigbuild`: with `zig` and `cargo-zigbuild` installed, it builds both architectures against glibc `RSI_ARTIFACT_GLIBC` (default `2.34`, Amazon Linux 2023's), so the binaries load on the host without a container. If cross compilation is unavailable, run the script with the optional third argument `x86_64` on an x86_64 machine and `arm64` on an arm64 machine, using the same source commit. The script builds the four host binaries with `--locked --release`, packages each architecture with its source commit and architecture metadata, and writes a SHA-256 file beside each archive:

```sh
scripts/build-aws-artifacts.sh <40-character-source-ref> /path/outside/checkout/artifacts
```

Publish the two `rsi-<ref>-x86_64.tar.gz` and `rsi-<ref>-arm64.tar.gz` archives to an operator-controlled HTTPS location, such as public release assets or a read-only artifact endpoint. Keep the `.sha256` files with the release. For the chosen host architecture, set `artifact_url` to its archive's credential-free HTTPS URL, set `artifact_sha256` to the 64-character digest, and set `source_ref` to the same commit. The module rejects a missing URL or digest at plan time. It verifies the downloaded bytes before extraction and refuses a bundle whose embedded commit or architecture differs. URLs containing credentials or expiring query tokens are rejected because user data is retained in Terraform state. This path needs no additional instance IAM permission. Keep the artifact available for replacement hosts.

Set `install_mode="build"` only as a fallback. That path requires `source_repo_url`, clones it at `source_ref`, installs Rust if needed and builds on the instance. Terraform checks the selected EC2 instance type at plan time and refuses fewer than 4 vCPUs or 16 GiB memory; the default `m6i.large` is therefore valid only for the artifact path. Budget for a longer boot and temporary build storage when using the fallback.

## Deployment and access

Review the example variables, exact Git ref, matching artifact URL and digest, secret paths, Region, instance family/architecture, budget, retention settings, optional existing VPC/subnet, SSH CIDRs, VPN settings and AWS identity first. From `infra/aws/example`, run:

```sh
terraform init
terraform apply
```

Supply nonsecret variables through your organization-approved CLI or local untracked variable file. Supply selected `secret_names` and inject matching `TF_VAR_operator_secrets` through a secure process for both plan and apply when using a saved plan. Terraform creates their SSM `SecureString` parameters through write-only values; never print or persist the apply environment. Do not commit local state, plans or variable files. Record the instance ID, data volume ID, backup bucket, selected Availability Zone and Git ref from nonsecret outputs. Wait for cloud-init and the `rsid` systemd service, then open an SSM shell with the AWS console or:

```sh
aws ssm start-session --target <instance-id> --profile <profile> --region <region>
```

Inside the host, inspect `systemctl status rsid`, `journalctl -u rsid` and the mounted data directory. Run the TUI as the `rsi` user within this SSM session, following the service paths from the module. Test an agent session, reconnect, reboot, and confirm state persists before relying on the host. If Tailscale is enabled, verify its tailnet identity and access policy separately.

## Backups and replacement-host restore drill

The data volume holds the SQLite databases, repository state and worktrees. A usable backup includes uncommitted work and provider state needed to resume, not only `rsi.db`. Make a quiesced EBS snapshot: stop new agents, stop `rsid`, flush and unmount the data filesystem (or stop the instance), then snapshot the data volume. AWS warns that snapshots taken while writes continue may lack filesystem consistency. Record the snapshot ID, source volume ID, Git ref and time. Keep a versioned encrypted S3 copy of file-level backups if the configured backup workflow is enabled; test the actual restore contents. See [EBS snapshot guidance](https://docs.aws.amazon.com/ebs/latest/userguide/ebs-creating-snapshot.html).

For the drill, launch a **replacement host in the same Availability Zone** with the same architecture, pinned Git ref and compatible secret references. Create an encrypted gp3 volume from the selected snapshot in that AZ, attach it as the data volume, and verify its filesystem UUID and mount. EBS volumes attach only within their AZ; a snapshot can be used to create a volume in a chosen AZ. Keep the old `rsid` stopped so only one daemon owns the restored state. After mounting, run database integrity checks and inspect repository/worktree state before starting `rsid`; then check `systemctl status rsid`, reconnect with SSM, and run a small agent session. Document observed restore time and snapshot recovery point. AWS's [volume restore procedure](https://docs.aws.amazon.com/ebs/latest/userguide/ebs-restoring-volume.html) covers replacement mechanics. Do not attach one writable SQLite volume to two active daemon hosts.

## Stop and teardown

Use the module's stop-when-idle option for temporary use only after verifying its idle signal matches your workload. Active sessions, interactive shells, provider processes and enabled daemon wakes keep the host on; `/srv/rsi/.keep-running` is a manual hold. You can also stop the EC2 instance from the AWS console and restart it later. Stopped volumes and retained backups still cost money.

Before teardown, stop `rsid`, take a quiesced snapshot, verify the snapshot completed and record its ID outside Terraform state. Then, from `infra/aws/example`, run `terraform destroy` with the same profile, Region, backend and nonsecret variables used for apply. The data-volume resource is configured to take a final snapshot on deletion; retain and verify that snapshot before deleting any backups. The managed S3 backup bucket has `force_destroy=true`: `terraform destroy` deletes its objects and all versions. Export any needed archives to separate storage first. The final EBS snapshot is retained outside Terraform state. Confirm the post-destroy inventory: EC2 instance, public IPv4, VPC, unattached volumes, snapshots, S3 objects, IAM roles, alarms and budget. Delete retained snapshots and backups only after a successful restore drill and an explicit data-retention decision. No destroy was run during preparation.
