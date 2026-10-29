#!/usr/bin/env bash
#
# Outer rollback: point the service back at OUTER_PREVIOUS_IMAGE.
#
#   deploy/rollback.sh <ssh-target>
#
# Points OUTER_IMAGE at OUTER_PREVIOUS_IMAGE in /srv/outer/release.env
# (recording the abandoned image as OUTER_ROLLED_BACK_FROM, and leaving no
# rollback target behind), then hands that image out with the same
# zero-downtime flow as a release (deploy/handoff.sh): the replacement
# container just runs the previous image, so the rollback itself drops no
# connection and kills no in-flight deploy.
#
# No secrets in args or logs (only image tags travel in release.env).

set -euo pipefail

TARGET="${1:?usage: deploy/rollback.sh <ssh-target>}"

ssh "$TARGET" bash <<'EOF'
set -euo pipefail
cd /srv/outer

[[ -f /srv/outer/release.env ]] || {
    echo "remote: /srv/outer/release.env missing (nothing ever released?)" >&2
    exit 1
}
# Sourced, NOT exported: an exported OUTER_IMAGE would override
# --env-file below and compose would keep the current image.
# shellcheck disable=SC1091
source /srv/outer/release.env
# shellcheck disable=SC1091
source /srv/outer/handoff.sh
# One-way: a retry after a failed rollout re-applies the same target
# instead of flipping back. Going forward again is a fresh release.sh.
if [[ -n "${OUTER_PREVIOUS_IMAGE:-}" ]]; then
    target="$OUTER_PREVIOUS_IMAGE"
    tmp="$(mktemp)"
    grep -v -E '^(OUTER_IMAGE|OUTER_PREVIOUS_IMAGE|OUTER_ROLLED_BACK_FROM)=' \
        /srv/outer/release.env >"$tmp" || true
    {
        cat "$tmp"
        echo "OUTER_IMAGE=$target"
        echo "OUTER_ROLLED_BACK_FROM=${OUTER_IMAGE:-}"
    } > /srv/outer/release.env
    rm -f "$tmp"
elif [[ -n "${OUTER_ROLLED_BACK_FROM:-}" ]]; then
    target="$OUTER_IMAGE"
    echo "remote: already rolled back from $OUTER_ROLLED_BACK_FROM; re-applying $target"
else
    echo "remote: OUTER_PREVIOUS_IMAGE is unset (nothing to roll back to)" >&2
    exit 1
fi
echo "remote: rolling back to $target"
# The per-app egress table lives in the host netns and outlives the
# container. A build that predates the firewall would never manage it, so
# drop it here; a build that has the firewall reinstalls it at boot.
nft delete table inet outer 2>/dev/null || true

trap 'handoff_clear; docker rm -f outer-next >/dev/null 2>&1 || true' EXIT
handoff_to "$target"
trap - EXIT
EOF
