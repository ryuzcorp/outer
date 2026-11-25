#!/usr/bin/env bash
#
# Outer host firewall: apply the static tunnel-only ruleset.
#
# The whole table loads as one nft transaction (`add table; delete table;
# table ...` in deploy/firewall.nft), valid whether or not the table
# exists, so there is never a window with no table or a stale one.
# `nft --check` runs first: a bad file fails closed and leaves the running
# table alone. The boot path (firewall.service, before network-pre) and the
# release path both call this script with no args.
#
# No secrets, no args, no network. Needs root (nft).

set -euo pipefail

RULES="${FIREWALL_RULES:-/srv/outer/firewall.nft}"

[[ -f "$RULES" ]] || {
    echo "firewall.sh: no ruleset at $RULES" >&2
    exit 1
}
/usr/sbin/nft --check -f "$RULES"
/usr/sbin/nft -f "$RULES"
echo "table inet outer_fw installed (closed inbound; loopback origin only)"
