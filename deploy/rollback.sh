#!/usr/bin/env bash
#
# Outer rollback: point the service back at OUTER_PREVIOUS_IMAGE.
#
#   deploy/rollback.sh <ssh-target>
#
# Swaps OUTER_IMAGE and OUTER_PREVIOUS_IMAGE in /srv/outer/release.env, runs
# `docker compose up -d`, and waits for the same signal as release.sh
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
# shellcheck disable=SC1091
set -a
. /srv/outer/release.env
set +a
[[ -n "${OUTER_PREVIOUS_IMAGE:-}" ]] || {
    echo "remote: OUTER_PREVIOUS_IMAGE is unset (nothing to roll back to)" >&2
    exit 1
}

tmp="$(mktemp)"
grep -v -E '^(OUTER_IMAGE|OUTER_PREVIOUS_IMAGE)=' /srv/outer/release.env >"$tmp" || true
{
    cat "$tmp"
    echo "OUTER_IMAGE=$OUTER_PREVIOUS_IMAGE"
    echo "OUTER_PREVIOUS_IMAGE=${OUTER_IMAGE:-}"
} > /srv/outer/release.env
rm -f "$tmp"
echo "remote: rolling back to $OUTER_PREVIOUS_IMAGE"

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
    if [[ "$(docker inspect -f '{{.State.Running}}' "$cid")" == "true" ]] \
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
echo "remote: rolled back to $OUTER_PREVIOUS_IMAGE ($cid)"
EOF
