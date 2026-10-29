#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# rtt.sh — TCP and TLS handshake round trips from THIS host to every host slot
# 11's engine dials or its checks read: Jupiter, Polymarket (the CLOB REST and
# market sockets, RTDS, Gamma), public Solana RPC, Hyperliquid, Binance (spot
# and USDⓈ-M, WS and REST). Runs on the EC2 host (any user) and inside
# az-rtt-probe.sh's throwaway instances. It is NOT the CLAUDE.md latency law's
# measurement (feed delay, clock offset, the harness Δ table): that is
# `python -m claude_worker.latency_probe`, per host and per location (README).
#
#   rtt.sh [--samples N] [--label TEXT] [--hosts "h1 h2 ..."]
#
# Each sample is one fresh `curl -4 --http1.1 --noproxy "*" -I` to
# https://<host>/: a HEAD
# with no API key, no cookie and no body. Only the handshake timings are read
# (`curl -w`): TCP = time_connect - time_namelookup (one round trip), TLS =
# time_appconnect - time_connect (the TLS handshake). The HTTP answer, whatever
# its status, is discarded. Printed per host: the median and the p90
# (nearest rank) of each, in ms, the samples that completed a handshake, and
# the address the last one reached (the edge that served it).
#
# It measures; it decides nothing. The AZ is the operator's choice, and a
# region or a VM is never a way around a venue's geoblock (README).
set -u -o pipefail

SAMPLES=20
LABEL="$(hostname 2>/dev/null || echo host)"
HOSTS="api.jup.ag clob.polymarket.com ws-subscriptions-clob.polymarket.com ws-live-data.polymarket.com
gamma-api.polymarket.com api.mainnet-beta.solana.com api.hyperliquid.xyz stream.binance.com
fstream.binance.com api.binance.com fapi.binance.com"

while [ $# -gt 0 ]; do
  case "$1" in
    --samples) SAMPLES="${2:-}"; shift 2 ;;
    --label) LABEL="${2:-}"; shift 2 ;;
    --hosts) HOSTS="${2:-}"; shift 2 ;;
    -h | --help) sed -n '6,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "rtt: unknown argument $1" >&2; exit 64 ;;
  esac
done
[[ "$SAMPLES" =~ ^[0-9]+$ ]] && [ "$SAMPLES" -ge 1 ] && [ "$SAMPLES" -le 500 ] ||
  { echo "rtt: --samples must be 1..500" >&2; exit 64; }
command -v curl >/dev/null 2>&1 || { echo "rtt: curl is required" >&2; exit 69; }

# p50 and p90 (nearest rank) of the numbers on stdin, one per line: "p50 p90".
pct() {
  sort -n | awk '{ v[NR] = $1 } END {
    if (NR == 0) { print "- -"; exit }
    i50 = int((NR * 50 + 99) / 100); i90 = int((NR * 90 + 99) / 100)
    printf "%.1f %.1f\n", v[i50], v[i90]
  }'
}

printf '%-28s %-38s %8s %8s %8s %8s %5s  %s\n' label host tcp_p50 tcp_p90 tls_p50 tls_p90 ok remote_ip
for h in $HOSTS; do
  [[ "$h" =~ ^[A-Za-z0-9.-]+$ ]] || { echo "rtt: '$h' is not a host name" >&2; continue; }
  tcp=""
  tls=""
  ok=0
  rip="-"
  i=0
  while [ "$i" -lt "$SAMPLES" ]; do
    line="$(curl -4 --http1.1 --noproxy "*" -sS -o /dev/null -I --max-time 5 \
      -w '%{time_namelookup} %{time_connect} %{time_appconnect} %{remote_ip}' "https://$h/" 2>/dev/null)"
    read -r tn tc ta ip <<<"$line"
    if awk -v a="${ta:-0}" 'BEGIN { exit !(a > 0) }'; then
      tcp="$tcp$(awk -v n="$tn" -v c="$tc" 'BEGIN { printf "%.3f", (c - n) * 1000 }')"$'\n'
      tls="$tls$(awk -v c="$tc" -v a="$ta" 'BEGIN { printf "%.3f", (a - c) * 1000 }')"$'\n'
      ok=$((ok + 1))
      rip="${ip:--}"
    fi
    i=$((i + 1))
    sleep 0.2
  done
  read -r t50 t90 <<<"$(printf '%s' "$tcp" | grep -v '^$' | pct)"
  read -r s50 s90 <<<"$(printf '%s' "$tls" | grep -v '^$' | pct)"
  printf '%-28s %-38s %8s %8s %8s %8s %5s  %s\n' "$LABEL" "$h" "$t50" "$t90" "$s50" "$s90" "$ok/$SAMPLES" "$rip"
done
