#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# provision.sh — the EC2 host for slot 11 (pmj) on its own engine
# (docs/risk-policy.md "PMJ — slot 11", ### J16: slot 11 arms only ALONE in
# its engine — the standalone, R-PM-23 / R-PMJ-S77). Run ON THE MAC by the
# operator, with his own AWS profile. A session never runs it.
#
#   deploy/ec2-pmj/provision.sh --az eu-west-1b --key-name my-key [--my-ip A.B.C.D]
#   deploy/ec2-pmj/provision.sh --az eu-west-1b --create-key multivenue-pmj   # ssh-keygen + import
#   deploy/ec2-pmj/provision.sh ... --dry-run      # prints every call; needs no credentials
#
# What it makes (all tagged Project=multivenue Lane=pmj Name=<name>):
#   - one c7i.2xlarge in eu-west-1 (the AZ you pass), Ubuntu 24.04 LTS amd64,
#     the AMI resolved from Canonical's SSM public parameter;
#   - a security group admitting TCP 22 from your IP /32 ONLY (egress open);
#   - an Elastic IP, associated;
#   - IMDSv2 required (http-tokens required, hop limit 1);
#   - an encrypted gp3 root of 100 GiB; termination protection ON;
#   - cloud-init user-data: UTC, chrony on the AWS Time Sync endpoint
#     (169.254.169.123 and fd00:ec2::123), and the kernel arguments that
#     isolate the engine's physical core (isolcpus / nohz_full / rcu_nocbs /
#     irqaffinity), then ONE reboot. The core map is computed on the box
#     from sysfs (README "Core map").
#
# Idempotent by tag lookup: re-running finds the instance, the group and the
# address by their tags and only fixes what differs (the SSH rule follows a
# new --my-ip; termination protection and IMDSv2 are re-asserted). It never
# terminates, stops or replaces anything, and it writes no secret but the SSH
# key you ask for with --create-key: generated on the Mac by ssh-keygen with a
# passphrase (~/.ssh/<name>, 0600), only its public half imported to AWS. No
# instance profile: the host holds no AWS credential at all. No snapshot, AMI
# or backup policy may ever cover the root volume (it holds the wallet
# keypair): README "No snapshots".
#
# A region or a VM is never a way around a venue's geoblock: the region is
# chosen for latency only; eligibility under Jupiter's and Polymarket's terms
# is the operator's ruling (R-PM-11). Jupiter refuses US and KR addresses.
set -u -o pipefail

REGION="eu-west-1"
INSTANCE_TYPE="c7i.2xlarge"
ROOT_GIB=100
AMI_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
NAME="multivenue-pmj"

AZ=""
KEY_NAME=""
CREATE_KEY=""
MY_IP=""
PROFILE="${AWS_PROFILE:-}"
SUBNET_ID=""
DRY=0

usage() {
  sed -n '6,13p' "$0" | sed 's/^# \{0,1\}//'
  cat >&2 <<'EOF'
options:
  --az eu-west-1a|eu-west-1b|eu-west-1c   (required)
  --key-name NAME        an EC2 key pair that already exists in eu-west-1
  --create-key NAME      ssh-keygen ~/.ssh/NAME (ed25519, a passphrase
                         required) and import its public key
  --my-ip A.B.C.D        the only address SSH admits (default: detected from
                         https://checkip.amazonaws.com)
  --profile NAME         the AWS CLI profile (default: $AWS_PROFILE)
  --subnet-id SUBNET     a subnet in --az (default: the default VPC's default
                         subnet for that AZ)
  --name NAME            the Name tag (default multivenue-pmj)
  --dry-run              print every AWS call and the user-data; call nothing
EOF
}

say() { echo "provision: $*" >&2; }
die() { echo "provision: $*" >&2; exit 1; }
need() { [ $# -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --az) need "$@"; AZ="$2"; shift 2 ;;
    --key-name) need "$@"; KEY_NAME="$2"; shift 2 ;;
    --create-key) need "$@"; CREATE_KEY="$2"; shift 2 ;;
    --my-ip) need "$@"; MY_IP="$2"; shift 2 ;;
    --profile) need "$@"; PROFILE="$2"; shift 2 ;;
    --subnet-id) need "$@"; SUBNET_ID="$2"; shift 2 ;;
    --name) need "$@"; NAME="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1 (--help)" ;;
  esac
done

case "$AZ" in
  eu-west-1a | eu-west-1b | eu-west-1c) ;;
  *) die "--az must be eu-west-1a, eu-west-1b or eu-west-1c (got '$AZ'); az-rtt-probe.sh measures them" ;;
esac
if [ -n "$KEY_NAME" ] && [ -n "$CREATE_KEY" ]; then
  die "--key-name and --create-key are exclusive"
fi
if [ -z "$KEY_NAME$CREATE_KEY" ]; then
  die "one of --key-name NAME or --create-key NAME is required"
fi
for k in "$KEY_NAME" "$CREATE_KEY" "$NAME"; do
  [ -z "$k" ] || [[ "$k" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || die "'$k' is not [A-Za-z0-9._-]{1,64}"
done
if [ -n "$SUBNET_ID" ] && ! [[ "$SUBNET_ID" =~ ^subnet-[0-9a-f]{8,17}$ ]]; then
  die "--subnet-id '$SUBNET_ID' is not a subnet id"
fi

is_ipv4() {
  [[ "$1" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] || return 1
  local o
  for o in "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[4]}"; do
    [ "$o" -le 255 ] || return 1
  done
}

if [ -z "$MY_IP" ]; then
  if [ "$DRY" = 1 ]; then
    MY_IP="<your-ip>"
  else
    MY_IP="$(curl -fsS --max-time 10 https://checkip.amazonaws.com | tr -d '[:space:]')" ||
      die "could not detect your IP — pass --my-ip"
  fi
fi
if [ "$MY_IP" != "<your-ip>" ]; then
  is_ipv4 "$MY_IP" || die "--my-ip '$MY_IP' is not an IPv4 address"
fi
MY_CIDR="$MY_IP/32"

AWS_BASE=(aws --region "$REGION" --output text)
if [ -n "$PROFILE" ]; then
  AWS_BASE+=(--profile "$PROFILE")
fi

# A dry-run line: the exact call, shell-quoted, on stderr.
show() {
  printf '[dry-run]' >&2
  printf ' %q' "$@" >&2
  printf '\n' >&2
}

# A mutating call: printed in a dry run, run otherwise.
act() {
  if [ "$DRY" = 1 ]; then
    show "${AWS_BASE[@]}" "$@"
    return 0
  fi
  "${AWS_BASE[@]}" "$@"
}

# A read whose answer the script needs: $1 is what a dry run answers.
ask() {
  local placeholder="$1"
  shift
  if [ "$DRY" = 1 ]; then
    show "${AWS_BASE[@]}" "$@"
    printf '%s\n' "$placeholder"
    return 0
  fi
  "${AWS_BASE[@]}" "$@"
}

# AWS's text output says None for a null.
nonnull() { [ -n "$1" ] && [ "$1" != "None" ]; }

TAGS="{Key=Project,Value=multivenue},{Key=Lane,Value=pmj},{Key=Name,Value=$NAME}"
TAG_FILTERS=("Name=tag:Project,Values=multivenue" "Name=tag:Lane,Values=pmj" "Name=tag:Name,Values=$NAME")

if [ "$DRY" = 0 ]; then
  command -v aws >/dev/null 2>&1 || die "the AWS CLI v2 is not on PATH"
  aws --version 2>&1 | grep -q '^aws-cli/2\.' || die "the AWS CLI must be v2 ($(aws --version 2>&1))"
  ACCOUNT="$("${AWS_BASE[@]}" sts get-caller-identity --query Account)" ||
    die "no working credentials for profile '${PROFILE:-default}'"
  say "account $ACCOUNT, region $REGION, profile ${PROFILE:-default}"
else
  say "DRY RUN — nothing below is called; <...> marks what a real run reads from AWS"
fi

# --- the AMI (Canonical's SSM public parameter) ---------------------------
AMI="$(ask '<ami-from-ssm>' ssm get-parameter --name "$AMI_PARAM" --query Parameter.Value)" ||
  die "could not resolve $AMI_PARAM"
nonnull "$AMI" || die "SSM answered no AMI for $AMI_PARAM"
ROOT_DEV="$(ask '/dev/sda1' ec2 describe-images --image-ids "$AMI" --query 'Images[0].RootDeviceName')" ||
  die "could not read $AMI"
nonnull "$ROOT_DEV" || die "$AMI has no root device name"
say "AMI $AMI (Ubuntu 24.04 LTS amd64, root $ROOT_DEV)"

# --- the subnet and its VPC ------------------------------------------------
if [ -n "$SUBNET_ID" ]; then
  read -r SUBNET_AZ VPC_ID < <(ask "$AZ <vpc-of-subnet>" ec2 describe-subnets --subnet-ids "$SUBNET_ID" \
    --query 'Subnets[0].[AvailabilityZone,VpcId]') || die "cannot read $SUBNET_ID"
  [ "$SUBNET_AZ" = "$AZ" ] || die "$SUBNET_ID is in $SUBNET_AZ, not $AZ"
else
  VPC_ID="$(ask '<default-vpc>' ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId')" ||
    die "cannot list VPCs"
  nonnull "$VPC_ID" || die "no default VPC in $REGION — pass --subnet-id"
  SUBNET_ID="$(ask "<default-subnet-$AZ>" ec2 describe-subnets --filters "Name=vpc-id,Values=$VPC_ID" \
    "Name=availability-zone,Values=$AZ" Name=default-for-az,Values=true --query 'Subnets[0].SubnetId')" ||
    die "cannot list subnets"
  nonnull "$SUBNET_ID" || die "no default subnet in $AZ — pass --subnet-id"
fi
say "subnet $SUBNET_ID in $AZ (VPC $VPC_ID)"

# --- the key pair ----------------------------------------------------------
# --create-key: generated ON THE MAC by ssh-keygen (ed25519, a passphrase is
# REQUIRED — an empty one refuses), only the PUBLIC key imported to AWS
# (import-key-pair). The private key never leaves the Mac. It is worth as much
# as slot 11's wallet: ssh + the ubuntu user's sudo reads the keypair file
# (README "The SSH key"). --key-name: an existing pair; its private key is
# yours to name in the stanza.
IDENT=""
if [ -n "$KEY_NAME" ]; then
  have="$(ask "$KEY_NAME" ec2 describe-key-pairs --key-names "$KEY_NAME" --query 'KeyPairs[0].KeyName')" ||
    die "key pair '$KEY_NAME' does not exist in $REGION (or --create-key it)"
  [ "$have" = "$KEY_NAME" ] || die "key pair '$KEY_NAME' does not exist in $REGION"
  IDENT="<the private key of EC2 key pair $KEY_NAME>"
else
  KEY_NAME="$CREATE_KEY"
  KEY_PATH="$HOME/.ssh/$KEY_NAME"
  # shellcheck disable=SC2088 # printed into the ssh-config stanza; ssh expands it
  IDENT="~/.ssh/$KEY_NAME"
  exists="$(ask 'None' ec2 describe-key-pairs --filters "Name=key-name,Values=$KEY_NAME" --query 'KeyPairs[0].KeyName')" ||
    die "cannot list key pairs"
  if nonnull "$exists"; then
    if [ ! -f "$KEY_PATH" ] || [ ! -f "$KEY_PATH.pub" ]; then
      die "key pair '$KEY_NAME' exists in AWS but $KEY_PATH(.pub) does not — use --key-name with the key you hold, or pick another name"
    fi
    say "key pair $KEY_NAME exists and $KEY_PATH is present — reusing it"
  else
    if [ ! -e "$KEY_PATH" ]; then
      if [ "$DRY" = 1 ]; then
        show ssh-keygen -t ed25519 -a 100 -f "$KEY_PATH" -C "multivenue-pmj $KEY_NAME"
        say "(dry run) ssh-keygen asks for a passphrase; an empty one refuses"
      else
        if ! { mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh"; }; then die "cannot prepare ~/.ssh"; fi
        say "ssh-keygen: choose a passphrase (REQUIRED; keep it in the Keychain, never in a file)"
        ssh-keygen -t ed25519 -a 100 -f "$KEY_PATH" -C "multivenue-pmj $KEY_NAME" || die "ssh-keygen failed"
      fi
    fi
    if [ "$DRY" = 0 ]; then
      [ -f "$KEY_PATH.pub" ] || die "$KEY_PATH.pub is missing"
      if ssh-keygen -y -P '' -f "$KEY_PATH" >/dev/null 2>&1; then
        die "$KEY_PATH has an EMPTY passphrase — set one (ssh-keygen -p -f $KEY_PATH), then re-run"
      fi
      chmod 600 "$KEY_PATH" || die "cannot chmod $KEY_PATH"
    fi
    act ec2 import-key-pair --key-name "$KEY_NAME" --public-key-material "fileb://$KEY_PATH.pub" \
      --tag-specifications "ResourceType=key-pair,Tags=[$TAGS]" --query KeyName >/dev/null ||
      die "import-key-pair failed"
    say "key pair $KEY_NAME imported (public key only; the private key stays in $KEY_PATH, passphrase-protected)"
  fi
fi

# --- the security group: TCP 22 from MY_CIDR only --------------------------
SG_NAME="$NAME-ssh"
SG_ID="$(ask 'None' ec2 describe-security-groups --filters "Name=vpc-id,Values=$VPC_ID" \
  "Name=group-name,Values=$SG_NAME" --query 'SecurityGroups[0].GroupId')" || die "cannot list security groups"
if ! nonnull "$SG_ID"; then
  if [ "$DRY" = 1 ]; then
    act ec2 create-security-group --group-name "$SG_NAME" --vpc-id "$VPC_ID" \
      --description "multivenue pmj: SSH from the operator only" \
      --tag-specifications "ResourceType=security-group,Tags=[$TAGS]" --query GroupId
    SG_ID="<new-sg>"
  else
    SG_ID="$("${AWS_BASE[@]}" ec2 create-security-group --group-name "$SG_NAME" --vpc-id "$VPC_ID" \
      --description "multivenue pmj: SSH from the operator only" \
      --tag-specifications "ResourceType=security-group,Tags=[$TAGS]" --query GroupId)" ||
      die "create-security-group failed"
  fi
  say "security group $SG_ID created ($SG_NAME; egress: AWS's default allow-all)"
fi
# Ingress, rule by rule (describe-security-group-rules: one source per rule):
# EVERY ingress rule that is not exactly tcp 22-22 from MY_CIDR is revoked —
# another port, a range spanning 22, protocol -1, an IPv6 range, a prefix list,
# a security-group source, another IPv4 address. Then MY_CIDR is added if absent.
OK_RULE="IpProtocol=='tcp' && FromPort==\`22\` && ToPort==\`22\` && CidrIpv4=='$MY_CIDR'"
stray="$(ask '' ec2 describe-security-group-rules --filters "Name=group-id,Values=$SG_ID" \
  --query "SecurityGroupRules[?IsEgress==\`false\` && !($OK_RULE)].SecurityGroupRuleId")" ||
  die "cannot read the rules of $SG_ID"
stray="$(printf '%s' "$stray" | tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//')"
if [ -n "$stray" ] && [ "$stray" != "None" ]; then
  say "revoking ingress rule(s) other than tcp/22 from $MY_CIDR: $stray"
  # shellcheck disable=SC2086 # a space-separated rule-id list, on purpose
  act ec2 revoke-security-group-ingress --group-id "$SG_ID" --security-group-rule-ids $stray >/dev/null ||
    die "revoke failed for $stray"
fi
have_me="$(ask 0 ec2 describe-security-group-rules --filters "Name=group-id,Values=$SG_ID" \
  --query "length(SecurityGroupRules[?IsEgress==\`false\` && $OK_RULE])")" || die "cannot read the rules of $SG_ID"
if [ "$have_me" = 0 ]; then
  act ec2 authorize-security-group-ingress --group-id "$SG_ID" \
    --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=$MY_CIDR,Description=operator}]" >/dev/null ||
    die "authorize failed for $MY_CIDR"
  say "SSH admitted from $MY_CIDR only"
fi

# --- the user-data (cloud-init) --------------------------------------------
UD="$(mktemp "${TMPDIR:-/tmp}/pmj-userdata.XXXXXX")" || die "mktemp failed"
trap 'rm -f "$UD"' EXIT
{
  cat <<'EOF'
#cloud-config
# multivenue pmj (slot 11) — first boot: UTC, AWS Time Sync, the engine core's
# kernel arguments, one reboot. Packages and users are host-setup.sh's.
timezone: Etc/UTC
ssh_pwauth: false
package_update: false
write_files:
  - path: /etc/chrony/sources.d/aws-time-sync.sources
    permissions: '0644'
    content: |
      # AWS Time Sync Service (link-local; the IPv6 one answers on Nitro with IPv6).
      server 169.254.169.123 prefer iburst minpoll 4 maxpoll 4
      server fd00:ec2::123 iburst minpoll 4 maxpoll 4
  - path: /usr/local/sbin/multivenue-pmj-cores
    permissions: '0755'
    content: |
EOF
  sed 's/^/      /' <<'EOF'
#!/bin/bash
# The engine core map, from sysfs (README "Core map"): the physical core with
# the highest core_id on package 0 is isolated whole (both hyperthreads); the
# engine loop is pinned to its first thread at every start (pmj-pin.sh); the
# rest is housekeeping (kernel, IRQs, ingress threads, systemd, the daily lane).
set -eu
mkdir -p /etc/multivenue-pmj
out=/etc/multivenue-pmj/cores.env
expand() { local IFS=, p; for p in $1; do case "$p" in *-*) seq "${p%-*}" "${p#*-}" ;; *) echo "$p" ;; esac; done; }
n="$(nproc --all)"
best=-1; bestcpu=""
for d in /sys/devices/system/cpu/cpu[0-9]*; do
  [ -r "$d/topology/core_id" ] || continue
  [ "$(cat "$d/topology/physical_package_id")" = 0 ] || continue
  c="$(cat "$d/topology/core_id")"
  if [ "$c" -gt "$best" ]; then best="$c"; bestcpu="${d##*cpu}"; fi
done
if [ "$n" -lt 4 ] || [ -z "$bestcpu" ]; then
  printf 'ENGINE_CPU=\nISOLATED_CPUS=\nHOUSEKEEPING_CPUS=0-%d\n' "$((n - 1))" >"$out"
  echo "multivenue-pmj-cores: $n CPUs — no isolation" >&2
  exit 0
fi
sib="$(cat "/sys/devices/system/cpu/cpu$bestcpu/topology/thread_siblings_list")"
iso="$(expand "$sib" | sort -n | paste -sd, -)"
eng="$(expand "$sib" | sort -n | head -1)"
hk="$(seq 0 $((n - 1)) | grep -vxF -f <(expand "$sib") | paste -sd, -)"
printf 'ENGINE_CPU=%s\nISOLATED_CPUS=%s\nHOUSEKEEPING_CPUS=%s\n' "$eng" "$iso" "$hk" >"$out"
cat >/etc/default/grub.d/90-multivenue-pmj.cfg <<GRUB
# multivenue pmj: the engine core (written by multivenue-pmj-cores at first boot)
GRUB_CMDLINE_LINUX="\${GRUB_CMDLINE_LINUX} isolcpus=managed_irq,domain,$iso nohz_full=$iso rcu_nocbs=$iso irqaffinity=$hk"
GRUB
update-grub
echo "multivenue-pmj-cores: engine CPU $eng, isolated $iso, housekeeping $hk" >&2
EOF
  cat <<'EOF'
runcmd:
  - [ /usr/local/sbin/multivenue-pmj-cores ]
  - [ systemctl, restart, chrony ]
power_state:
  mode: reboot
  message: multivenue pmj first boot — kernel arguments applied, rebooting once
  timeout: 60
  condition: true
EOF
} >"$UD"
if [ "$DRY" = 1 ]; then
  say "user-data (cloud-init) follows:"
  sed 's/^/    | /' "$UD" >&2
fi

# --- the instance ----------------------------------------------------------
read -r IID IAZ ISTATE < <(ask 'None None None' ec2 describe-instances --filters "${TAG_FILTERS[@]}" \
  Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[0].Instances[0].[InstanceId,Placement.AvailabilityZone,State.Name]') ||
  die "cannot list instances"
if nonnull "${IID:-}"; then
  [ "$IAZ" = "$AZ" ] || die "the tagged instance $IID is in $IAZ, not $AZ — nothing changed"
  say "instance $IID exists ($ISTATE, $IAZ) — keeping it"
else
  BDM="[{\"DeviceName\":\"$ROOT_DEV\",\"Ebs\":{\"VolumeSize\":$ROOT_GIB,\"VolumeType\":\"gp3\",\"Encrypted\":true,\"DeleteOnTermination\":true}}]"
  RUN=(ec2 run-instances --image-id "$AMI" --instance-type "$INSTANCE_TYPE" --count 1
    --key-name "$KEY_NAME" --subnet-id "$SUBNET_ID" --security-group-ids "$SG_ID"
    --block-device-mappings "$BDM"
    --metadata-options "HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled"
    --disable-api-termination --instance-initiated-shutdown-behavior stop
    --monitoring "Enabled=false" --user-data "file://$UD"
    --tag-specifications "ResourceType=instance,Tags=[$TAGS]" "ResourceType=volume,Tags=[$TAGS]"
    "ResourceType=network-interface,Tags=[$TAGS]"
    --query 'Instances[0].InstanceId')
  if [ "$DRY" = 1 ]; then
    act "${RUN[@]}"
    IID="<new-instance>"
  else
    IID="$("${AWS_BASE[@]}" "${RUN[@]}")" || die "run-instances failed"
    nonnull "$IID" || die "run-instances answered no instance id"
  fi
  say "instance $IID launched ($INSTANCE_TYPE, $AZ); waiting for running"
fi
case "${ISTATE:-}" in
  stopping | stopped)
    say "$IID is $ISTATE — not starting it (yours to start: aws ec2 start-instances --instance-ids $IID)"
    ;;
  *) act ec2 wait instance-running --instance-ids "$IID" || die "$IID did not reach running" ;;
esac
act ec2 modify-instance-attribute --instance-id "$IID" --disable-api-termination >/dev/null ||
  die "could not assert termination protection on $IID"
act ec2 modify-instance-metadata-options --instance-id "$IID" --http-tokens required \
  --http-put-response-hop-limit 1 --http-endpoint enabled >/dev/null ||
  die "could not assert IMDSv2 on $IID"
# The instance must carry exactly this one security group (no other group may
# open it to anything): verified, never changed here.
groups="$(ask "$SG_ID" ec2 describe-instances --instance-ids "$IID" \
  --query 'Reservations[0].Instances[0].NetworkInterfaces[].Groups[].GroupId')" || die "cannot read $IID's groups"
groups="$(printf '%s' "$groups" | tr -s '[:space:]' '\n' | grep -v '^$' | sort -u | paste -sd' ' -)"
[ "$groups" = "$SG_ID" ] ||
  die "$IID carries security group(s) '$groups', not exactly $SG_ID — fix by hand: aws ec2 modify-instance-attribute --instance-id $IID --groups $SG_ID"

# --- the Elastic IP --------------------------------------------------------
read -r ALLOC EIP EINST < <(ask 'None None None' ec2 describe-addresses --filters "${TAG_FILTERS[@]}" \
  --query 'Addresses[0].[AllocationId,PublicIp,InstanceId]') || die "cannot list addresses"
if ! nonnull "${ALLOC:-}"; then
  if [ "$DRY" = 1 ]; then
    act ec2 allocate-address --domain vpc --tag-specifications "ResourceType=elastic-ip,Tags=[$TAGS]" \
      --query '[AllocationId,PublicIp]'
    ALLOC="<eipalloc>"
    EIP="<elastic-ip>"
  else
    read -r ALLOC EIP < <("${AWS_BASE[@]}" ec2 allocate-address --domain vpc \
      --tag-specifications "ResourceType=elastic-ip,Tags=[$TAGS]" --query '[AllocationId,PublicIp]') ||
      die "allocate-address failed"
    nonnull "${ALLOC:-}" || die "allocate-address answered no allocation"
  fi
  EINST="None"
  say "Elastic IP $EIP allocated ($ALLOC)"
fi
if [ "${EINST:-None}" = "$IID" ]; then
  say "Elastic IP $EIP is on $IID"
elif nonnull "${EINST:-}"; then
  die "Elastic IP $EIP is associated with $EINST, not $IID — nothing re-associated"
else
  act ec2 associate-address --instance-id "$IID" --allocation-id "$ALLOC" --no-allow-reassociation >/dev/null ||
    die "associate-address failed"
  say "Elastic IP $EIP associated with $IID"
fi

cat <<EOF

instance   $IID ($INSTANCE_TYPE, $AZ)
elastic IP $EIP
first boot reboots once (kernel arguments); give it ~3 minutes.

# ~/.ssh/config
Host pmj-ec2
  HostName $EIP
  User ubuntu
  IdentityFile $IDENT
  IdentitiesOnly yes
  IgnoreUnknown UseKeychain
  UseKeychain yes
  AddKeysToAgent yes
  ServerAliveInterval 30
  # never -L 9191: the Mac's 9191 is main's engine — use 19191 (README)

# before the first ssh, compare the host keys with the console's copy:
aws --region $REGION${PROFILE:+ --profile $PROFILE} ec2 get-console-output --instance-id $IID --latest --output text \\
  | sed -n '/BEGIN SSH HOST KEY FINGERPRINTS/,/END SSH HOST KEY FINGERPRINTS/p'
EOF
