#!/usr/bin/env bash
#
# Outer rollback: point the service back at OUTER_PREVIOUS_IMAGE.
#
#   deploy/rollback.sh <ssh-target>
#
# Points OUTER_IMAGE at OUTER_PREVIOUS_IMAGE in /srv/outer/release.env
# (recording the abandoned image as OUTER_ROLLED_BACK_FROM, and leaving no
# rollback target behind), runs `docker compose up -d`, and waits for the same signal as release.sh
# (container running + outer's "listening on" log line — mTLS admits no
# unauthenticated HTTP probe).
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
. /srv/outer/release.env
# One-way: a retry after a failed rollout re-applies the same target instead
# of flipping back. Going forward again is a fresh release.sh.
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

docker compose --env-file /srv/outer/release.env \
    -f /srv/outer/compose.yaml up -d

cid="$(docker compose --env-file /srv/outer/release.env \
    -f /srv/outer/compose.yaml ps -q outer)"
[[ -n "$cid" ]] || {
    echo "remote: no outer container after up" >&2
    exit 1
}
deadline=$((SECONDS + 60))
while true; do
    # The image check catches a compose that kept the old container.
    if [[ "$(docker inspect -f '{{.State.Running}} {{.Config.Image}}' "$cid")" == "true $target" ]] \
        && docker logs "$cid" 2>&1 | grep -q 'listening on'; then
        break
    fi
    if [[ $SECONDS -ge $deadline ]]; then
        echo "remote: outer did not come back; last log lines:" >&2
        docker logs --tail 50 "$cid" >&2 || true
        exit 1
    fi
    sleep 2
done
echo "remote: rolled back to $target ($cid)"
EOF
