# Agent AWS profile (#3460)

These are the IAM policies that let an AI agent run the #3460 cloud work by itself, inside hard
limits. The work is the spot gate/QA host (S1), the bench docker host (S3), the warm pool (S4b)
and, optionally, Bedrock model experiments (S6). Agents never apply these policies. The operator
applies them with admin credentials (AGENTS.md: IAM changes get one reviewer pass before merge).

| File | Attach to | Purpose |
| --- | --- | --- |
| `rsi-cloud-agent.json` | user `rsi-cloud-terraform` (profile of the same name) | Day-to-day EC2 spot work in `us-west-1`. Replaces `rsi-cloud-terraform-scope`. |
| `rsi-cloud-bedrock.json` | same user, optional | Lists and invokes Bedrock models. Billed per token, and **not** metered by the daemon's `cloud_spend_*` caps. |
| `rsi-cloud-deny-launch.json` | target of an AWS Budget Action | Kill switch: denies new launches (including fleet and legacy spot requests) and Bedrock calls once a budget trips. |

The profile name stays `rsi-cloud-terraform`, because `infra/aws/gate/versions.tf` and
`scripts/cloud-gate.sh` hard-code it. The orphan sweeper keeps its own `rsi-cloud-watchdog`
profile, which these files do not change.

## What the agent policy allows

- **Read:** `ec2:Describe*` and CloudWatch metrics in `us-west-1`, the public Amazon Linux AMI
  parameter, Budgets, Cost Explorer, the Price List API, its own IAM permissions
  (`SimulatePrincipalPolicy` lets it check an action before trying it), and Access Analyzer
  policy linting.
- **Launch** (`RunInstances` only; no fleet, spot-fleet, spot-request or launch-template API is
  granted):
  - only **spot**, only `c7i.2xlarge`, `c7i.4xlarge` or `c7i.8xlarge`, only from Amazon-owned AMIs;
  - every volume encrypted and at most 300 GiB, so the request must carry an explicit encrypted
    root mapping (the Amazon Linux AMI's root snapshot is unencrypted and the account has no
    encryption by default);
  - only into subnet `subnet-0227c7c61c84a9dc1` (`rsid-rsid-public`) with security group
    `sg-0aedeb3f66034e92f` (`rsid-rsid`), both by exact ARN. The VPC's `default` group and any
    other subnet are refused;
  - only on a **new** network interface in that subnet that requests a public IPv4 address
    (`ec2:AssociatePublicIpAddress=true`). AWS assigns a public address only to a new primary
    interface, so a request that attaches an existing interface, or adds a second one, is refused.
    The gate sets `associate_public_ip_address = true`; the bench and pool tooling must too.
- **Launch tags:** the instance must carry `Service` with exactly one of the approved ephemeral
  workloads (`rsi-remote-gate` for the gate, `rsi-bench` for S3, `rsi-pool` for S4b) and an
  `rsi:expires-at` tag. Tag keys are matched case-exactly, and only `Name`, `Owner`, `Service`
  and `rsi:expires-at` may be set (`Owner` comes from the gate's provider `default_tags`).
  Adding a workload or a tag key is a policy change that needs review.
- **Manage:** terminate, stop, reboot and read the console of instances whose `Service` is one of
  those three values. The persistent `rsid-rsid` host and every other project in the account are
  out of reach.
- **Spot service-linked role:** it may create `AWSServiceRoleForEC2Spot`, and nothing else in IAM.
  The 2026-10-22T03:16Z Stage-1 proof run failed for lack of it
  (`AuthFailure.ServiceLinkedRoleCreationNotPermitted`).
- **Warm cache (S4):** `iam:PassRole` of `rsi-gate-sccache` to EC2 only, plus list of the cache
  bucket and read and delete on its `sccache/` prefix.

## What it explicitly denies

Each of these is an explicit Deny, so no other grant on the user can override it.

- **On-demand launches.** Six on-demand c7i.8xlarge cost $256 per 24 h, which is over the cap.
- **Launches without an approved `Service` or without `rsi:expires-at`.** This keeps every agent
  instance inside the orphan sweep: `rsi-cloud watchdog` selects `tag:Service` by exact key
  and exempts `*-rsid`, and none of the three approved values ends in `-rsid`.
- **Tag keys outside `Name`, `Owner`, `Service`, `rsi:expires-at`** (compared case-sensitively,
  so `service` or `RSI:EXPIRES-AT` is refused), on every `CreateTags` the user makes.
- **Any tag change after launch.** `CreateTags` outside a launch and every `DeleteTags` are
  denied, so an instance cannot be retagged into the sweeper's exemption, out of its selection or
  to a later expiry. A longer-lived host is relaunched, not retagged; S4b renewal must do that
  (or come back for a reviewed policy change).
- **Security group, VPC, subnet, route, network-interface and IAM mutation.** No new ingress,
  interfaces, users, keys, roles or policy versions. An AZ move to `us-west-1c`, or recreating
  the `rsid-rsid` network, is an operator change that also updates the subnet and group ARNs here.
- **c7i.16xlarge and bigger** are simply not in the allow list, so a spot hour cannot exceed the
  $0.7813/h reserve rate that `rsi-cloud spend` assumes.

IAM cannot cap the spot max price or the instance lifetime. Those bounds stay in the gate tooling
(`spot_max_price_usd`, the guest power-off), the reservation ledger and the orphan sweep. Budgets
and the kill switch below are the backstop.

## Operator steps (admin credentials)

The new policy only restricts anything once it is the user's **only** grant: IAM adds up every
Allow the user has, and a conditional Allow that does not match never cancels another Allow. So
the old grants come off before the checks run, and the checks then test the effective set.

```bash
ACCT=625527221604
U=rsi-cloud-terraform
cd infra/aws/iam

# 0. Inventory every grant the user has now. Keep this output: it is the rollback list.
aws iam list-attached-user-policies --user-name $U
aws iam list-user-policies          --user-name $U   # inline; save each with get-user-policy
aws iam list-groups-for-user        --user-name $U   # for each group: list-attached-group-policies, list-group-policies
aws iam get-user --user-name $U --query User.PermissionsBoundary
for p in $(aws iam list-user-policies --user-name $U --query PolicyNames --output text); do
  aws iam get-user-policy --user-name $U --policy-name "$p" > "rollback-inline-$p.json"
done

# 1. Spot service-linked role (one time). create-service-linked-role fails if it already exists,
#    so check first.
aws iam get-role --role-name AWSServiceRoleForEC2Spot --query Role.Arn --output text 2>/dev/null \
  || aws iam create-service-linked-role --aws-service-name spot.amazonaws.com

# 2. Create and attach the agent policy.
aws iam create-policy --policy-name rsi-cloud-agent --policy-document file://rsi-cloud-agent.json
aws iam attach-user-policy --user-name $U --policy-arn arn:aws:iam::$ACCT:policy/rsi-cloud-agent

# 3. Remove every other grant found in step 0, so rsi-cloud-agent is the effective set.
#    Detach (do not delete yet) the old managed policy; delete inline policies only after saving
#    them; take the user out of any group that grants EC2, S3 or IAM.
aws iam detach-user-policy --user-name $U --policy-arn arn:aws:iam::$ACCT:policy/rsi-cloud-terraform-scope
# aws iam delete-user-policy --user-name $U --policy-name <each inline policy from step 0>
# aws iam remove-user-from-group --user-name $U --group-name <each granting group from step 0>
aws iam list-attached-user-policies --user-name $U   # expect rsi-cloud-agent only
aws iam list-user-policies          --user-name $U   # expect none
aws iam list-groups-for-user        --user-name $U   # expect none that grant anything

# 4. Optional: Bedrock experiments (attach after the checks below pass).
aws iam create-policy --policy-name rsi-cloud-bedrock --policy-document file://rsi-cloud-bedrock.json
aws iam attach-user-policy --user-name $U --policy-arn arn:aws:iam::$ACCT:policy/rsi-cloud-bedrock

# 5. Kill-switch policy for a Budget Action (attach only through the action).
aws iam create-policy --policy-name rsi-cloud-deny-launch --policy-document file://rsi-cloud-deny-launch.json
```

**Rollback** (if a check below fails, or the Stage-1 proof run breaks): re-attach the old policy,
restore any saved inline policy, and detach the new one. Agent cloud work stops either way until a
fixed policy is reviewed.

```bash
aws iam attach-user-policy --user-name $U --policy-arn arn:aws:iam::$ACCT:policy/rsi-cloud-terraform-scope
# aws iam put-user-policy --user-name $U --policy-name <name> \
#   --policy-document "$(jq -c .PolicyDocument rollback-inline-<name>.json)"
aws iam detach-user-policy --user-name $U --policy-arn arn:aws:iam::$ACCT:policy/rsi-cloud-agent
```

## Verify the effective set (agent profile, no admin)

Run this in bash **after step 3**, once IAM changes have propagated (about a minute). Every launch
is a `--dry-run`, and each check compares the AWS error code: both `DryRunOperation` (would have
succeeded) and `UnauthorizedOperation` exit nonzero, so the exit status proves nothing. Every
negative differs from the positive control in exactly one respect.

```bash
P="--profile rsi-cloud-terraform --region us-west-1"
AMI=$(aws ssm get-parameter $P --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 --query Parameter.Value --output text)
ROOT=$(aws ec2 describe-images $P --image-ids "$AMI" --query 'Images[0].RootDeviceName' --output text)
SUBNET=subnet-0227c7c61c84a9dc1; SG=sg-0aedeb3f66034e92f; DEFAULT_SG=sg-0a6a39d2391480577
SPOT="--instance-market-options MarketType=spot"
disk() { echo "DeviceName=$ROOT,Ebs={Encrypted=$1,VolumeSize=$2,VolumeType=gp3,DeleteOnTermination=true}"; }
tags() { echo "ResourceType=instance,Tags=[{Key=$1,Value=$2},{Key=$3,Value=2099-01-01T00:00:00Z}]"; }
OK_DISK=$(disk true 200); OK_TAGS=$(tags Service rsi-remote-gate rsi:expires-at)
check() {  # check <expected code> <label> <run-instances args...>
  want=$1 label=$2; shift 2
  got=$(aws ec2 run-instances --dry-run $P --image-id "$AMI" --instance-type "${TYPE:-c7i.8xlarge}" "$@" 2>&1 \
        | grep -o 'DryRunOperation\|UnauthorizedOperation' | head -n1)
  if [ "$got" = "$want" ]; then echo "PASS $label ($got)"; else echo "FAIL $label: want $want, got ${got:-another error}"; fi
}
NET="--subnet-id $SUBNET --security-group-ids $SG --associate-public-ip-address"
check DryRunOperation       positive          $NET $SPOT --block-device-mappings "$OK_DISK" --tag-specifications "$OK_TAGS"
check UnauthorizedOperation on-demand         $NET       --block-device-mappings "$OK_DISK" --tag-specifications "$OK_TAGS"
check UnauthorizedOperation untagged          $NET $SPOT --block-device-mappings "$OK_DISK"
check UnauthorizedOperation unencrypted-root  $NET $SPOT --block-device-mappings "$(disk false 200)" --tag-specifications "$OK_TAGS"
check UnauthorizedOperation root-301GiB       $NET $SPOT --block-device-mappings "$(disk true 301)" --tag-specifications "$OK_TAGS"
TYPE=c7i.16xlarge check UnauthorizedOperation c7i.16xlarge $NET $SPOT --block-device-mappings "$OK_DISK" --tag-specifications "$OK_TAGS"
check UnauthorizedOperation default-sg        --subnet-id $SUBNET --security-group-ids $DEFAULT_SG --associate-public-ip-address $SPOT --block-device-mappings "$OK_DISK" --tag-specifications "$OK_TAGS"
check UnauthorizedOperation no-public-ip      --subnet-id $SUBNET --security-group-ids $SG --no-associate-public-ip-address $SPOT --block-device-mappings "$OK_DISK" --tag-specifications "$OK_TAGS"
check UnauthorizedOperation lowercase-keys    $NET $SPOT --block-device-mappings "$OK_DISK" --tag-specifications "$(tags service rsi-remote-gate RSI:EXPIRES-AT)"
check UnauthorizedOperation exempt-service    $NET $SPOT --block-device-mappings "$OK_DISK" --tag-specifications "$(tags Service rsi-rsid rsi:expires-at)"
```

Then check the actions a dry run cannot reach, with the simulator on the user's own effective policies, and the persistent host and another project's
bucket directly:

```bash
aws iam simulate-principal-policy $P --policy-source-arn arn:aws:iam::625527221604:user/rsi-cloud-terraform \
  --action-names ec2:CreateTags ec2:DeleteTags ec2:CreateFleet ec2:RequestSpotInstances iam:CreateRole iam:CreateAccessKey \
  --query 'EvaluationResults[].[EvalActionName,EvalDecision]' --output text
# expect explicitDeny for ec2:CreateTags, ec2:DeleteTags, iam:CreateRole, iam:CreateAccessKey;
# implicitDeny for ec2:CreateFleet and ec2:RequestSpotInstances.
RSID=$(aws ec2 describe-instances $P --filters Name=tag:Service,Values=rsid-rsid --query 'Reservations[].Instances[].InstanceId' --output text)
[ -n "$RSID" ] && aws ec2 stop-instances --dry-run $P --instance-ids $RSID      # expect UnauthorizedOperation
[ -n "$RSID" ] && aws ec2 terminate-instances --dry-run $P --instance-ids $RSID # expect UnauthorizedOperation
aws s3api list-objects-v2 $P --bucket <a bucket of another project> --max-items 1 # expect AccessDenied
```

When every line passes, delete the old `rsi-cloud-terraform-scope` policy after the Stage-1 proof
run succeeds: `scripts/cloud-sweep.sh cloud "$(git rev-parse origin/rolling)"`. Until then it stays
detached, as the rollback.

## Budgets (operator)

- **Stage 1:** raise `rsi-cloud-us-west-1-daily` from $15 to $25 so a normal day does not alert.
- **Stage 2, at $200 per 24 h:** set the daily budget to $200 and the monthly budget to at least
  $1400. Add a Budget Action at 100% of the daily budget that applies `rsi-cloud-deny-launch` to
  `rsi-cloud-terraform`.
- **With Bedrock:** add a separate Bedrock-service budget, because the daemon caps never see that
  spend.
- The kill switch stops new launches and Bedrock calls; it does not stop or terminate instances
  already running. Their lifetime limit and the orphan sweep end them.
- Cost Explorer API calls cost $0.01 each, so agents should prefer `budgets describe-budgets` for
  routine checks.
