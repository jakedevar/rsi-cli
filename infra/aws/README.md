# Single-host rsid on AWS

`example/` is an account-agnostic root using the reusable `modules/rsid-host/` module. By default it creates one Amazon Linux 2023 host in its own VPC/public subnet with outbound internet access, no VPC ingress rules, SSM Session Manager access, a separately attached encrypted gp3 data volume, and an encrypted versioned S3 backup bucket. It can instead use an existing VPC/subnet, and `modules/client-vpn/` is an optional endpoint. The host installs SHA-256 checked binaries built from a **full Git commit SHA** and runs `rsid` as the unprivileged `rsi` user. The TUI is installed on the host for an SSM terminal. Nothing in this directory provisions resources until an operator runs `terraform apply`.

## Local validation

Install Terraform 1.13.5 in user space from [HashiCorp releases](https://releases.hashicorp.com/terraform/1.13.5/), download its `SHA256SUMS`, verify the ZIP with `sha256sum -c`, and unpack `terraform` into `~/.local/bin`. The AWS provider is fixed to `6.14.0` and the example and both modules have lock files. No AWS authentication is needed for these commands:

```sh
terraform fmt -check -recursive infra/aws
(cd infra/aws/modules/rsid-host && terraform init -backend=false && terraform validate)
(cd infra/aws/modules/client-vpn && terraform init -backend=false && terraform validate)
(cd infra/aws/example && terraform init -backend=false && terraform validate)
```

## Operator deployment

Use a deployment role/profile with privileges to create VPC, EC2, EBS, S3, IAM, SSM parameters, CloudWatch and Budgets resources. The instance role is separate and restricted to SSM managed-instance access, the exact configured secret paths, its backup prefix, one log group, its metrics namespace, and optional self-stop. Supply the required nonsecret `budget_email` and `source_ref` (40-character lowercase Git SHA). Default `install_mode=artifact` also needs a credential-free HTTPS `artifact_url` and its lowercase `artifact_sha256` for the selected architecture. See [the build and publication procedure](../../docs/cloud-aws.md#prebuilt-binaries) before apply. For example, set `instance_type=m7g.large` together with `architecture=arm64`, or keep `m6i.large` with `x86_64`. Check that the chosen type is available in the target region and that all provider CLIs needed by your workload support that architecture. `install_mode=build` is an explicit fallback that requires `source_repo_url`, at least 4 vCPUs and 16 GiB memory.

The operator selects secret names from `anthropic_api_key` (exported as `ANTHROPIC_API_KEY` for Claude Code), `codex_api_key`, `openrouter_api_key`, `github_deploy_key`, and `tailscale_auth_key` in `secret_names`. For both a saved-plan run and its apply, inject a matching `TF_VAR_operator_secrets` JSON map from a password manager or CI secret store. Terraform marks that input `ephemeral` and writes each value through `aws_ssm_parameter.value_wo`; the value is omitted from plan and state. Never put it in a `.tfvars` file, shell history, CLI `-var`, saved plan or repository. The SSM parameter paths are `/<prefix>/rsid/<name>`. To rotate a Terraform-managed value, supply the new ephemeral value and increment its `secret_versions` entry. If `enable_tailscale=true`, include `tailscale_auth_key`. A private SSH Git URL needs `github_deploy_key`; a public HTTPS URL does not. The host fetches secrets at boot or service restart; the deploy key is held in `/run/rsi/ssh` (tmpfs), while API credentials are inherited by the daemon process. Use an instance profile and encrypted remote Terraform state if deploying with a team. The commented S3 backend in `example/versions.tf` is a starting point; create its bucket and locking setup separately.

After reviewing a fresh account price and the exact plan, the operator may run:

```sh
cd infra/aws/example
terraform init
terraform plan -out=reviewed.tfplan
terraform apply reviewed.tfplan
terraform output ssm_start_session
```

The `plan` and `apply` commands above are **operator steps**, not part of local validation. Terraform may read account data (availability zones, the public Amazon Linux AMI parameter, identity) during plan. No key pair or inbound SSH is created by default. From an SSM shell, run `sudo -iu rsi rsi` for the TUI. `rsi` and `rsid` stay local to the EC2 host; the Unix RPC socket is never exposed on a public listener. The default instance public IPv4 supports outbound SSM, packages, Git, provider APIs and optional Tailscale. Systemd waits for `/srv/rsi`; Nitro EBS is identified by volume serial and mounted by filesystem UUID, so an existing data volume survives host replacement. User data and log outputs contain only secret **paths**, never secret values. Set `provider_cli_npm_packages` to exact-version npm specs (the example defaults to Claude Code and Codex) and the host installs Node.js and those CLIs into `/usr/local` at first boot; the service sets `DISABLE_AUTOUPDATER=1` and keeps them read-only, so change a version by editing the list. Without `anthropic_api_key`, sign Claude Code in once as the `rsi` user from an SSM shell (`sudo -iu rsi claude`); its credentials stay on the data volume.

## Network and access choices

| Access path | Inputs | Inbound rule |
| --- | --- | --- |
| SSM Session Manager (default) | Instance role and outbound HTTPS | None |
| Tailscale (optional satellite peer) | `enable_tailscale=true` and SSM auth-key parameter | None in the VPC security group |
| SSH from named CIDRs | Existing `ssh_key_name` and `ssh_ingress_cidrs` | TCP/22 only from each listed CIDR |
| Existing work Client VPN + SSH | Work VPC/subnet, `ssh_key_name`, `enable_vpn_ssh=true`, `existing_vpn_security_group_id` | TCP/22 from that VPN association security group |
| New Client VPN + SSH | `enable_client_vpn=true`, ACM certificate ARNs, client and authorization CIDRs, `ssh_key_name`, `enable_vpn_ssh=true` | TCP/22 from the new VPN association security group |

Set both `vpc_id` and `subnet_id` to use a work VPC. The module creates no VPC, subnet, internet gateway or route table in this mode and checks that the subnet belongs to the VPC. `associate_public_ip_address` defaults to **false** for an existing subnet and **true** for the module-owned public subnet; set it explicitly if the work routing requires a different choice. A private subnet needs existing NAT or suitable VPC endpoints for SSM and logs, plus outbound access for Git, packages, Rustup and provider APIs. This module does not change work route tables or create those endpoints. The selected subnet's availability zone determines the EBS volume's zone.

`ssh_ingress_cidrs` is empty by default and rejects IPv4 and IPv6 `/0` ranges. Setting any CIDR requires the name of an **existing** EC2 SSH key pair; Terraform never handles the private key. A CIDR rule is useful for direct corporate/VPN routes that preserve client addresses. [AWS Client VPN translates client source IPs](https://docs.aws.amazon.com/vpn/latest/clientvpn-admin/how-it-works.html), so its client CIDR is usually *not* the address seen by the host. For Client VPN, explicitly enable `enable_vpn_ssh` and name the existing VPN association security group, or enable the new endpoint and its security group rule. Keep the source security group in the host's VPC. This grants SSH to users allowed by that VPN's authentication and authorization policy; review the work VPN policy before enabling it.

Prefer the work's existing VPN. The new `modules/client-vpn/` option is **off by default** and requires operator-created ACM server certificate and client CA certificate ARNs, an explicit non-overlapping IPv4 client range, and an authorized target CIDR. It creates a mutual-TLS, split-tunnel endpoint, associates the selected subnet, authorizes all holders of trusted client certificates for the named target CIDR, and retains connection logs for 30 days. Certificate private keys and client profiles never enter Terraform. The client CIDR cannot overlap the target VPC or manually added routes; AWS will reject overlapping inputs. Enabling the new endpoint alone leaves host SSH closed until `enable_vpn_ssh` is also set.

`stop_when_idle_minutes=0` disables self-stop. A value of at least 30 enables a five-minute timer that requests an EC2 stop after the configured idle period. It resets idle time while an SSM shell, SSH login, `rsi` TUI, provider CLI, active rsid session, or enabled daemon wake exists, regardless of CPU use. Creating `/srv/rsi/.keep-running` also holds the host on until the operator removes it. The timer fails closed if it cannot read the SQLite database or scheduled jobs. Stopping removes compute cost but gp3, snapshots, S3, and some other charges remain. The operator restarts the instance via EC2/SSM; this option does not schedule an automatic restart.

## Backups, replacement and teardown

Nightly at 03:00 UTC, `rsi-backup` skips backup if rsid has active sessions. Otherwise it stops rsid, archives the data volume excluding build artifacts and prior archives, uploads the archive to the `backups/` S3 prefix, then restarts rsid. This includes SQLite files, repositories, worktrees and uncommitted files in `/srv/rsi`. CloudWatch alarms cover EC2 status, service failure, disk use and backup age; rsid file logs have 30-day retention. Alarms are visible in CloudWatch; configure alarm actions if external notification is required. The budget sends actual 80% and forecast 100% email notices. **The budget is account-wide**, so use a separate account or interpret it alongside other work spend.

For a replacement drill, first quiesce sessions and take a successful backup. Stop rsid, create an EBS snapshot, and note the snapshot ID. In a fresh workspace/state, set `data_snapshot_id` to that ID, ensure `data_volume_gib` is at least the snapshot size, use the same `source_ref` and fresh operator secrets, then apply in the snapshot's region. The new volume is created in the selected subnet's availability zone. Connect with the SSM output, check `systemctl status rsid`, inspect `/srv/rsi/.rsi/rsi.db`, and run a provider launch plus SSM reconnect test. For an S3 restore, download a selected archive to a clean encrypted volume, extract it at `/srv/rsi` while rsid is stopped, fix ownership to `rsi:rsi`, then start rsid. Rehearse this before relying on backups. Restoring active provider sessions may require operator reconciliation because external provider effects are not in the archive.

`terraform destroy` creates a **final encrypted EBS snapshot** before deleting the data volume unless `data_volume_final_snapshot = false` (the `tonight` build root sets it: its volume holds only reproducible build state). It also deletes all versions in the managed backup bucket (`force_destroy=true`), so export any archive to independent storage first. Locate the final EBS snapshot by the volume's tags in the AWS console; it is not in Terraform outputs after state deletion. A pre-destroy manual snapshot is safer for an important host. If destroy fails, keep state and resolve the failing resource, then retry; do not delete the state file. Snapshot and S3 charges persist until separately cleaned up.

## Cost envelope

The table uses the [2026-09-24 plan's](../../thoughts/shared/plans/2026-09-24-issue-729-aws-rsid-deployment.md) read-only `us-west-1` EC2/gp3 rates, 730 running hours, 130 GiB gp3 (100 data + 30 root), one [$0.005/hour public IPv4](https://aws.amazon.com/vpc/pricing/), and an illustrative $10–$45/month for backups/logs. Rates and account discounts can change; confirm them with [AWS On-Demand pricing](https://aws.amazon.com/ec2/pricing/on-demand/) before spending.

| Example size | Architecture | Compute / month | Approximate total / month |
| --- | --- | ---: | ---: |
| `m6i.large` | x86_64 | $82 | $108–$143 |
| `m6i.xlarge` | x86_64 | $164 | $190–$225 |
| `m7g.large` | arm64 | quote live | live rate × 730 + about $26–$61 |

These exclude provider/inference fees, taxes, data transfer, extra snapshots and SSM advanced features. The budget alerts rather than enforcing a spend cap. Initial bootstrap can take a while because it compiles Rust. Git and Rustup downloads, the CloudWatch agent package, and optional Tailscale installer are network dependencies; pin and mirror them if repeatable offline bootstrap is required.

Creating a new Client VPN has a separate recurring cost even when the EC2 host stops. [AWS's Client VPN pricing example](https://aws.amazon.com/vpn/pricing/) uses about **$0.10/hour for an endpoint with one subnet association** and **$0.05/hour per connected client** in US East (Ohio): about $73/month plus $36.50/month for one client connected all 730 hours. Public IPv4, CloudWatch logs and data transfer may add charges. Quote the target region and account before enabling it; reusing an existing work VPN avoids creating this endpoint.

## Ephemeral remote gate host

`infra/aws/gate` is a separate root for one `rsi-rolling-land` remote shard gate. Operator rule (2026-09-28): apply, run one gate, destroy; keep no build volumes or snapshots. It creates a single instance, default `c7i.8xlarge` (the smallest type the `rsi-cloud-terraform-scope` IAM policy allows), in the persistent `tonight` VPC, public subnet and SSH security group. Those are found by tag, so it adds no network exposure. It has no secrets and no data volume (an instance profile is attached only when the warm-cache stack below exists). Its encrypted root volume holds the build and is deleted on termination. The guest powers off after `max_lifetime_minutes` (default 240), and the instance's shutdown behavior is `terminate`, so compute and storage end even if nobody runs `destroy`.

Run it only through the wrapper, which destroys on every exit (success, failure, signal) and then checks AWS for leftover `Service=rsi-remote-gate` instances:

```bash
RSI_LANDER_GUARD_TIMEOUT_SECS=3600 scripts/cloud-gate.sh -- --repo "$PWD" --remote origin --accepted BASE:SOURCE
```

An isolated retry of one candidate failure has its own bound, `RSI_LANDER_RETRY_TIMEOUT_SECS` (default 600, clamped to 60 s..=the guard it isolates), so a hung retry refuses the gate instead of holding it for the shard timeout (#988). A base result is only reused when the candidate sees the same gate environment: the scratch filesystem class and the sorted spec env are part of the base-cache key.

The lander runs up to `RSI_REMOTE_GATE_PARALLEL` (default 4, max 8) remote shards at once on the host, base and candidate alike; each keeps its fingerprinted `--jobs`. Remote shard scripts must support `--keep-rsid-artifacts` (rolling since 2026-09-26). The wrapper takes no desktop lander slot: `~/.rsi/cloud/gate.lock` is the remote-gate pool (one host per state), and each local Cargo or shard-runner command the lander still runs takes a desktop build slot through `RSI_LANDER_LOCAL_SLOT_WRAPPER` (#969). Under `systemd-run --user`, pass `-p TimeoutStopSec=600` so a stopped unit's destroy trap can finish before SIGKILL; the guest lifetime limit remains the backstop.

`~/.rsi/cloud/gate.tfvars` names the EC2 key pair (`ssh_key_name`); `RSI_GATE_IDENTITY` is its private key. The wrapper pins the host's ED25519 key only after it matches the fingerprint cloud-init printed to the instance console. It appends start and stop lines with the estimated compute cost to `~/.rsi/cloud/spend.md`. State lives in `~/.rsi/cloud/gate.tfstate`, and one gate host runs at a time (`gate.lock`).

### Warm compile cache and spend guard (Issue #1010)

Heavy runs are routine only if they do not start cold, and the operator ruled that no build volume or snapshot is kept. The warm cache is therefore a private S3 bucket, `rsi-gate-sccache-<account>-us-west-1`, created by the separate root `infra/aws/gate-cache` so `terraform destroy` of the gate host never touches it. Objects expire 14 days after they are written (`expire_days`, at most 14), the bucket is private, encrypted and TLS-only, and it has no versioning. The same stack owns the instance role and profile `rsi-gate-sccache`, whose only permissions are `s3:GetObject`/`s3:PutObject` on the `sccache/` prefix and `s3:ListBucket` limited to that prefix.

One-time apply with a profile that may create S3 buckets and IAM roles. The day-to-day `rsi-cloud-terraform` user cannot, so an admin (or a policy addition granting `s3:CreateBucket`, `s3:PutBucket*`, `s3:PutLifecycleConfiguration`, `s3:PutEncryptionConfiguration`, `s3:DeleteBucket` on `arn:aws:s3:::rsi-gate-sccache-*` and `iam:CreateRole`, `iam:PutRolePolicy`, `iam:CreateInstanceProfile`, `iam:AddRoleToInstanceProfile`, `iam:GetRole`, `iam:GetRolePolicy`, `iam:GetInstanceProfile`, `iam:ListRolePolicies`, `iam:ListInstanceProfilesForRole`, `iam:TagRole`, `iam:TagInstanceProfile`, `iam:Delete*`/`Remove*` counterparts on `role/rsi-gate-sccache` and `instance-profile/rsi-gate-sccache`, plus `iam:PassRole` on that role with `iam:PassedToService = ec2.amazonaws.com`) must run it:

```bash
terraform -chdir=infra/aws/gate-cache init
AWS_PROFILE=<admin profile> terraform -chdir=infra/aws/gate-cache apply -state="$HOME/.rsi/cloud/gate-cache.tfstate"
```

`scripts/cloud-gate.sh` reads that state file. Without it the host builds cold and says so; with it the host bootstrap installs a pinned, checksum-verified `sccache`, writes its S3 config and a `build.rustc-wrapper`/`incremental = false` cargo config for the `rsi` user (so the lander's remote shards and the sweep both use it), and starts a long-lived server whose counters cover the run. After the run the wrapper prints `sccache_hits=… sccache_misses=… sccache_hit_rate=…%`, saves the numbers to `~/.rsi/cloud/logs/sccache-<instance>.json` and appends them to the run's `~/.rsi/cloud/spend.md` stop line.

Every remote run first runs `scripts/cloud-spend.py check`: it refuses to start (exit 4) when the ledger's cumulative "est compute" total is at or above the stop line read from the ledger header (`Operator grant: $100 ...`, `Stop and report by $90 cumulative`). A new grant starts a new ledger. AWS Budgets (`rsi-cloud-us-west-1-daily` $15, `-monthly` $100) remain the external backstop.

One-command full-suite check of the current rolling tip: `scripts/cloud-sweep.sh cloud "$(git rev-parse origin/rolling)"`. It reuses `cloud-gate.sh --run`, so it has the same apply, spend guard, ledger lines and EXIT-trap destroy (400 GiB root, 420-minute guest limit), and leaves `QA.md`, `report.json` and `sccache.json` in `~/.rsi/cloud/results/<sha>/`.
