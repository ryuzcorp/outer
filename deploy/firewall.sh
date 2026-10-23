#!/usr/bin/env bash
#
# Outer host firewall: an nftables ruleset in its own table (`inet outer_fw`).
#
# Inbound policy is drop. Allowed in: loopback, established/related,
# ICMP/ICMPv6 essentials, tcp/22 from anywhere (SSH, key-only after
# provision.sh hardens sshd), tcp/443 only from Cloudflare's published ranges
# (https://www.cloudflare.com/ips-v4 and /ips-v6). Everything else inbound is
# dropped. Outbound is untouched.
#
# The table is the only thing this script manages: outer's own in-container
# nft egress table and Docker's iptables-nft rules (filter/nat chains) live
# elsewhere, and Docker port publishes are unaffected because outer runs on
# the host network (`network_mode: host`), so its 443 listener is filtered by
# the INPUT hook like any host process.
#
# Modes:
#   firewall.sh               first install: fetch Cloudflare's ranges and
#                             atomically replace the table. Fails closed: any
#                             fetch/validation failure exits nonzero and the
#                             old table (if any) is left alone.
#   firewall.sh --refresh     same, but a fetch/validation failure keeps the
#                             old table and exits 0 (for the weekly timer). If
#                             no table exists there is nothing to keep, so it
#                             still fails.
#   firewall.sh --apply-saved re-apply the last-good ruleset saved next to
#                             this script (for boot, before network is up).
#
# No secrets, no args beyond the mode flag. Needs root (nft) and curl.

set -euo pipefail

TABLE="outer_fw"
FAMILY="inet"
V4_URL="https://www.cloudflare.com/ips-v4"
V6_URL="https://www.cloudflare.com/ips-v6"
STATE_FILE="${FIREWALL_STATE:-/srv/outer/firewall.nft}"

mode="install"
if [[ "${1:-}" == "--refresh" ]]; then
    mode="refresh"
elif [[ "${1:-}" == "--apply-saved" ]]; then
    mode="apply"
elif [[ -n "${1:-}" ]]; then
    echo "usage: firewall.sh [--refresh|--apply-saved]" >&2
    exit 2
fi

if [[ "$mode" == "apply" ]]; then
    [[ -f "$STATE_FILE" ]] || {
        echo "firewall.sh: no saved ruleset at $STATE_FILE" >&2
        exit 1
    }
    /usr/sbin/nft --check -f "$STATE_FILE"
    /usr/sbin/nft -f "$STATE_FILE"
    echo "applied $STATE_FILE"
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Nothing has been applied when this runs: an existing table (refresh) stays
# as it was, and a first install leaves 443 unopened.
fail_closed() {
    echo "firewall.sh ($mode): $1; ruleset unchanged" >&2
    exit 1
}

table_exists() {
    /usr/sbin/nft list table "$FAMILY" "$TABLE" >/dev/null 2>&1
}
fetch() { # $1 = url, $2 = family (v4|v6), $3 = outfile
    curl -fsSL --retry 3 --max-time 30 "$1" | tr -d '\r' >"$3" ||
        fail_closed "fetching $1 failed"
    # fail_closed always exits, so the lines below only run on success.
    [[ -s "$3" ]] || fail_closed "empty response from $1"
    local bad
    bad="$(grep -Ev -e '^([0-9]{1,3}\.){3}[0-9]{1,3}/([0-9]|[12][0-9]|3[0-2])$' "$3" || true)"
    if [[ "$2" == "v6" ]]; then
        bad="$(grep -Ev -e '^[0-9a-fA-F:]+/(12[0-8]|1[01][0-9]|[1-9]?[0-9])$' "$3" || true)"
    fi
    [[ -z "$bad" ]] || fail_closed "unexpected lines from $1: $(head -n 3 <<<"$bad" | tr '\n' ' ')"
}

fetch "$V4_URL" v4 "$work/ips-v4"
fetch "$V6_URL" v6 "$work/ips-v6"

# One comma-separated line per family for the set literals.
v4_list="$(tr '\n' ',' <"$work/ips-v4")"
v4_list="${v4_list%,}"
v6_list="$(tr '\n' ',' <"$work/ips-v6")"
v6_list="${v6_list%,}"

rules="$work/rules.nft"
{
    if table_exists; then
        # One file = one nft transaction: delete + recreate is atomic, so
        # there is no window with an empty (fail-open) table.
        printf 'delete table %s %s\n' "$FAMILY" "$TABLE"
    fi
    printf 'table %s %s {\n' "$FAMILY" "$TABLE"
    printf '\tset cf4 {\n\t\ttype ipv4_addr\n\t\tflags interval\n\t\telements = { %s }\n\t}\n' "$v4_list"
    printf '\tset cf6 {\n\t\ttype ipv6_addr\n\t\tflags interval\n\t\telements = { %s }\n\t}\n' "$v6_list"
    cat <<'EOF'
	chain input {
		type filter hook input priority 0; policy drop;
		iifname "lo" accept
		ct state established,related accept
		icmp type { echo-request, echo-reply, destination-unreachable, time-exceeded, parameter-problem } accept
		icmpv6 type { echo-request, echo-reply, destination-unreachable, packet-too-big, time-exceeded, parameter-problem, nd-neighbor-solicit, nd-neighbor-advert, nd-router-advert } accept
		udp sport 67 udp dport 68 accept
		udp sport 547 udp dport 546 accept
		tcp dport 22 accept
		tcp dport 443 ip saddr @cf4 accept
		tcp dport 443 ip6 saddr @cf6 accept
	}
}
EOF
} >"$rules"

/usr/sbin/nft --check -f "$rules"
/usr/sbin/nft -f "$rules"
install -m 600 "$rules" "$STATE_FILE"
echo "table $FAMILY $TABLE installed ($(wc -l <"$work/ips-v4") v4 + $(wc -l <"$work/ips-v6") v6 Cloudflare ranges)"
