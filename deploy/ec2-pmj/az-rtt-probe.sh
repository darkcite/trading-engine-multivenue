#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# az-rtt-probe.sh — which eu-west-1 AZ is closest to slot 11's venues? Run ON
# THE MAC by the operator with his AWS credentials, BEFORE provision.sh.
#
#   deploy/ec2-pmj/az-rtt-probe.sh [--profile P] [--samples N]
#                                  [--azs "eu-west-1a eu-west-1b eu-west-1c"]
#                                  [--instance-type t3.micro] [--dry-run]
#
# It launches ONE tiny instance per AZ (Ubuntu 24.04, the SSM AMI, the default
# VPC's default subnet, and a security group of its own with NO ingress rule —
# created for the run, deleted after it; no key pair, no instance profile,
# IMDSv2 required), each with
# `instance-initiated-shutdown-behavior terminate` and `shutdown -h +15` as
# the first line of its user-data, so an instance this script loses track of
# still terminates itself within 15 minutes. Each runs rtt.sh (this
# directory; embedded in the user-data) against api.jup.ag,
# the Polymarket hosts (clob, ws-subscriptions-clob, ws-live-data, gamma-api),
# api.mainnet-beta.solana.com, api.hyperliquid.xyz and the Binance hosts
# (stream, fstream, api, fapi) — TCP and TLS handshakes only, no API key, no
# request body (rtt.sh's list) — and writes the table to its
# serial console between markers. This script reads the tables back with
# `aws ec2 get-console-output --latest`, prints them, terminates the instances
# and deletes the group (on any exit, Ctrl-C included).
#
# Tagged Project=multivenue Lane=pmj Purpose=az-rtt-probe. Cost: three
# t3.micro for under 15 minutes. It decides nothing: pick the AZ, then
# provision.sh --az <it>. A region or a VM is never a way around a venue's
# geoblock (README).
set -u -o pipefail

REGION="eu-west-1"
AZS="eu-west-1a eu-west-1b eu-west-1c"
ITYPE="t3.micro"
SAMPLES=30
PROFILE="${AWS_PROFILE:-}"
DRY=0
AMI_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
WAIT_S=900

say() { echo "az-rtt-probe: $*" >&2; }
die() { echo "az-rtt-probe: $*" >&2; exit 1; }
need() { [ $# -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --profile) need "$@"; PROFILE="$2"; shift 2 ;;
    --samples) need "$@"; SAMPLES="$2"; shift 2 ;;
    --azs) need "$@"; AZS="$2"; shift 2 ;;
    --instance-type) need "$@"; ITYPE="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    -h | --help) sed -n '6,29p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1 (--help)" ;;
  esac
done
[[ "$SAMPLES" =~ ^[0-9]+$ ]] && [ "$SAMPLES" -ge 1 ] && [ "$SAMPLES" -le 200 ] || die "--samples must be 1..200"
[[ "$ITYPE" =~ ^[a-z0-9]+\.[a-z0-9]+$ ]] || die "--instance-type '$ITYPE' is not an instance type"
for az in $AZS; do
  case "$az" in eu-west-1[a-c]) ;; *) die "'$az' is not eu-west-1a/b/c" ;; esac
done

KIT="$(cd "$(dirname "$0")" && pwd -P)"
[ -f "$KIT/rtt.sh" ] || die "$KIT/rtt.sh is missing"

AWS_BASE=(aws --region "$REGION" --output text)
if [ -n "$PROFILE" ]; then
  AWS_BASE+=(--profile "$PROFILE")
fi
show() {
  printf '[dry-run]' >&2
  printf ' %q' "$@" >&2
  printf '\n' >&2
}
awsx() {
  if [ "$DRY" = 1 ]; then
    show "${AWS_BASE[@]}" "$@"
    return 0
  fi
  "${AWS_BASE[@]}" "$@"
}

TAGS="{Key=Project,Value=multivenue},{Key=Lane,Value=pmj},{Key=Purpose,Value=az-rtt-probe},{Key=Name,Value=pmj-az-rtt-probe}"

if [ "$DRY" = 0 ]; then
  command -v aws >/dev/null 2>&1 || die "the AWS CLI v2 is not on PATH"
  "${AWS_BASE[@]}" sts get-caller-identity --query Account >/dev/null || die "no working AWS credentials"
  AMI="$("${AWS_BASE[@]}" ssm get-parameter --name "$AMI_PARAM" --query Parameter.Value)" || die "cannot resolve the AMI"
  VPC="$("${AWS_BASE[@]}" ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId')" || die "cannot list VPCs"
  [ -n "$VPC" ] && [ "$VPC" != None ] || die "no default VPC in $REGION"
else
  show "${AWS_BASE[@]}" ssm get-parameter --name "$AMI_PARAM" --query Parameter.Value
  show "${AWS_BASE[@]}" ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId'
  AMI="<ami-from-ssm>"
  VPC="<default-vpc>"
fi

UD="$(mktemp "${TMPDIR:-/tmp}/pmj-rtt-userdata.XXXXXX")" || die "mktemp failed"
RTT_B64="$(base64 <"$KIT/rtt.sh" | tr -d '\n')"
cat >"$UD" <<EOF
#!/bin/bash
# az-rtt-probe (multivenue pmj): measure, print to the serial console, die.
shutdown -h +15 "az-rtt-probe: self-terminating" || true
console=/dev/ttyS0
[ -w "\$console" ] || console=/dev/console
tok="\$(curl -fsS -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 120' http://169.254.169.254/latest/api/token)"
az="\$(curl -fsS -H "X-aws-ec2-metadata-token: \$tok" http://169.254.169.254/latest/meta-data/placement/availability-zone)"
echo '$RTT_B64' | base64 -d >/tmp/rtt.sh
{
  echo "=== AZ-RTT-BEGIN \$az"
  bash /tmp/rtt.sh --samples $SAMPLES --label "\$az"
  echo "=== AZ-RTT-END \$az"
} >"\$console" 2>&1
EOF

IIDS=""
SG=""
cleanup() {
  rm -f "$UD"
  [ "$DRY" = 0 ] || return 0
  if [ -n "$IIDS" ]; then
    say "terminating$IIDS"
    # shellcheck disable=SC2086 # IIDS is a space-separated id list on purpose
    if "${AWS_BASE[@]}" ec2 terminate-instances --instance-ids $IIDS >/dev/null; then
      # shellcheck disable=SC2086
      "${AWS_BASE[@]}" ec2 wait instance-terminated --instance-ids $IIDS || true
    else
      say "terminate failed — they self-terminate within 15 minutes; check: aws ec2 describe-instances --filters Name=tag:Purpose,Values=az-rtt-probe"
    fi
  fi
  if [ -n "$SG" ]; then
    if "${AWS_BASE[@]}" ec2 delete-security-group --group-id "$SG" >/dev/null; then
      say "security group $SG deleted"
    else
      say "could not delete $SG (an instance still holds it?) — delete it by hand later"
    fi
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# A group of the probe's own: no ingress rule at all (AWS's default egress).
if [ "$DRY" = 1 ]; then
  awsx ec2 create-security-group --group-name "pmj-az-rtt-probe-$$" --vpc-id "$VPC" \
    --description "multivenue pmj az-rtt-probe: no ingress" \
    --tag-specifications "ResourceType=security-group,Tags=[$TAGS]" --query GroupId
  SGX="<probe-sg>"
else
  SG="$("${AWS_BASE[@]}" ec2 create-security-group --group-name "pmj-az-rtt-probe-$$" --vpc-id "$VPC" \
    --description "multivenue pmj az-rtt-probe: no ingress" \
    --tag-specifications "ResourceType=security-group,Tags=[$TAGS]" --query GroupId)" || die "create-security-group failed"
  SGX="$SG"
  say "security group $SG (no ingress)"
fi

for az in $AZS; do
  if [ "$DRY" = 1 ]; then
    awsx ec2 run-instances --image-id "$AMI" --instance-type "$ITYPE" --count 1 \
      --placement "AvailabilityZone=$az" --security-group-ids "$SGX" --instance-initiated-shutdown-behavior terminate \
      --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled \
      --user-data "file://$UD" \
      --tag-specifications "ResourceType=instance,Tags=[$TAGS]" "ResourceType=volume,Tags=[$TAGS]" \
      --query 'Instances[0].InstanceId'
    continue
  fi
  iid="$("${AWS_BASE[@]}" ec2 run-instances --image-id "$AMI" --instance-type "$ITYPE" --count 1 \
    --placement "AvailabilityZone=$az" --security-group-ids "$SGX" --instance-initiated-shutdown-behavior terminate \
    --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled \
    --user-data "file://$UD" \
    --tag-specifications "ResourceType=instance,Tags=[$TAGS]" "ResourceType=volume,Tags=[$TAGS]" \
    --query 'Instances[0].InstanceId')" || die "run-instances failed in $az (a default VPC with a default subnet per AZ is required)"
  IIDS="$IIDS $iid"
  say "$az: $iid launched"
done
if [ "$DRY" = 1 ]; then
  say "user-data follows (rtt.sh embedded, base64):"
  sed 's/^/    | /' "$UD" >&2
  show "${AWS_BASE[@]}" ec2 get-console-output --instance-id "<each>" --latest --query Output
  show "${AWS_BASE[@]}" ec2 terminate-instances --instance-ids "<each>"
  show "${AWS_BASE[@]}" ec2 wait instance-terminated --instance-ids "<each>"
  show "${AWS_BASE[@]}" ec2 delete-security-group --group-id "<probe-sg>"
  exit 0
fi

say "waiting for the tables (boot + $SAMPLES samples x 5 hosts; console output lags a minute or two)"
deadline=$((SECONDS + WAIT_S))
pending="$IIDS"
results=""
while [ -n "${pending// /}" ] && [ "$SECONDS" -lt "$deadline" ]; do
  sleep 20
  still=""
  for iid in $pending; do
    out="$("${AWS_BASE[@]}" ec2 get-console-output --instance-id "$iid" --latest --query Output 2>/dev/null || true)"
    if printf '%s\n' "$out" | grep -q '=== AZ-RTT-END'; then
      table="$(printf '%s\n' "$out" | tr -d '\r' | sed -n '/=== AZ-RTT-BEGIN/,/=== AZ-RTT-END/p' | grep -v '^=== AZ-RTT-')"
      results="$results$table"$'\n'
      say "$iid: table received"
    else
      still="$still $iid"
    fi
  done
  pending="$still"
done
[ -z "${pending// /}" ] || say "no table from:$pending (timed out after ${WAIT_S}s)"
echo
printf '%s' "$results" | awk 'NR == 1 || !/^label /'
echo
echo "ms, median / p90 of $SAMPLES handshakes per host. TLS p50 to api.jup.ag and the Polymarket hosts is what slot 11's order path pays; api.hyperliquid.xyz and the Binance hosts carry its nowcasts. Handshakes are not the CLAUDE.md latency law's measurement: run claude_worker.latency_probe on the chosen host (README)."
