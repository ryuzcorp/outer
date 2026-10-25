#!/usr/bin/env bash
#
# Outer release: build the image from the current HEAD and roll it out.
#
#   deploy/release.sh <ssh-target>
#
# Refuses a dirty tree. Prefers podman, falls back to docker. Streams the
# image with save|load (no registry), uploads compose.yaml + the firewall
# files to /srv/outer, records OUTER_IMAGE (keeping the previous tag as
# OUTER_PREVIOUS_IMAGE), runs `docker compose up -d`, then health-checks.
#
# Health check from the box: outer serves TLS with required client
# certificates (mTLS), so no unauthenticated probe can pass the handshake —
# not even `curl --resolve ... https://outer.now/health` from here. The check
# is therefore: the container is running and outer logged its "listening on"
# line. Rollback: deploy/rollback.sh <ssh-target>.
#
# No secrets in args or logs (only image tags travel in release.env).

set -euo pipefail

TARGET="${1:?usage: deploy/release.sh <ssh-target>}"
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

[[ -z "$(git status --porcelain)" ]] || {
    echo "release.sh: refusing a dirty tree (commit or stash first)" >&2
    exit 1
}
SHA="$(git rev-parse --short HEAD)"
[[ "$SHA" =~ ^[0-9a-f]+$ ]] || {
    echo "release.sh: unexpected sha: $SHA" >&2
    exit 1
}

# Migrations are append-only. sqlx refuses to boot when an applied
# migration's file is gone or its bytes changed (its checksum is the
# SHA-384 of the file), and by then the old container is already stopped.
# So before building: every migration the production database has applied
# must exist here with the same SHA-384. New, unapplied files are fine.
MIGRATIONS="crates/outer/migrations"
APPLIED="$(ssh "$TARGET" "sqlite3 -separator ' ' /srv/outer/data/outer.sqlite \
    'SELECT version, lower(hex(checksum)) FROM _sqlx_migrations ORDER BY version'" 2>/dev/null)" || {
    echo "release.sh: could not read applied migrations from $TARGET (sqlite3 on the host?)" >&2
    exit 1
}
while read -r version checksum; do
    [[ -n "$version" ]] || continue
    file="$(printf '%s/%04d_' "$MIGRATIONS" "$version")"
    matches=("$file"*.sql)
    [[ -f "${matches[0]}" ]] || {
        echo "release.sh: migration $version is applied on $TARGET but missing from this tree" >&2
        exit 1
    }
    local_sum="$(sha384sum "${matches[0]}" | cut -d' ' -f1)"
    [[ "$local_sum" == "$checksum" ]] || {
        echo "release.sh: ${matches[0]} differs from the copy applied on $TARGET; migrations are append-only (restore it byte-for-byte)" >&2
        exit 1
    }
done <<<"$APPLIED"
IMAGE="outer:$SHA"
if command -v podman >/dev/null 2>&1; then
    # podman names unqualified tags localhost/outer:<sha>, and docker load
    # keeps that name; the remote script retags it to the plain name.
    podman build --format docker -t "$IMAGE" -f docker/Dockerfile .
    podman save "$IMAGE" | ssh "$TARGET" docker load
elif command -v docker >/dev/null 2>&1; then
    docker build -t "$IMAGE" -f docker/Dockerfile .
    docker save "$IMAGE" | ssh "$TARGET" docker load
else
    echo "release.sh: need podman or docker on PATH" >&2
    exit 1
fi

ssh "$TARGET" mkdir -p /srv/outer
scp deploy/compose.yaml deploy/firewall.sh deploy/firewall.service \
    deploy/firewall-refresh.service deploy/firewall-refresh.timer \
    "$TARGET:/srv/outer/"

# Remote rollout. SHA is hex (validated above), so interpolating it into the
# remote script is safe; the heredoc is otherwise quoted.
ssh "$TARGET" bash -s -- "$SHA" <<'EOF'
set -euo pipefail
SHA="$1"
cd /srv/outer

if docker image inspect "localhost/outer:$SHA" >/dev/null 2>&1; then
    docker tag "localhost/outer:$SHA" "outer:$SHA"
    docker rmi "localhost/outer:$SHA" >/dev/null
fi

[[ -f /srv/outer/outer.env ]] || {
    echo "remote: /srv/outer/outer.env missing (copy deploy/outer.env.example and fill it)" >&2
    exit 1
}
# Default TLS paths from outer.env.example; a custom layout needs this
# check updated, outer itself still validates at boot.
for f in /srv/outer/tls/origin.pem /srv/outer/tls/origin.key /srv/outer/tls/origin-pull-ca.pem; do
    [[ -f "$f" ]] || {
        echo "remote: $f missing" >&2
        exit 1
    }
done

# Record the new image, keeping the previous tag for rollback.
touch /srv/outer/release.env
chmod 600 /srv/outer/release.env
prev="$(grep -E '^OUTER_IMAGE=' /srv/outer/release.env | tail -n 1 | cut -d= -f2- || true)"
prev_rollback="$(grep -E '^OUTER_PREVIOUS_IMAGE=' /srv/outer/release.env | tail -n 1 | cut -d= -f2- || true)"
tmp="$(mktemp)"
grep -v -E '^(OUTER_IMAGE|OUTER_PREVIOUS_IMAGE|OUTER_ROLLED_BACK_FROM)=' \
    /srv/outer/release.env >"$tmp" || true
{
    cat "$tmp"
    echo "OUTER_IMAGE=outer:$SHA"
    if [[ -n "$prev" && "$prev" != "outer:$SHA" ]]; then
        echo "OUTER_PREVIOUS_IMAGE=$prev"
    elif [[ -n "$prev_rollback" ]]; then
        # Re-releasing the running image keeps the existing rollback target.
        echo "OUTER_PREVIOUS_IMAGE=$prev_rollback"
    fi
} > /srv/outer/release.env
rm -f "$tmp"

# outer runs on the host network: the ingress filter must be in place before
# it starts (a first install may predate it). firewall.sh exits nonzero on
# failure, which stops the rollout here.
if ! nft list table inet outer_fw >/dev/null 2>&1; then
    /srv/outer/firewall.sh
fi

docker compose --env-file /srv/outer/release.env \
    -f /srv/outer/compose.yaml up -d


# Wait for the rollout: container running + outer's boot line. mTLS means no
# HTTP probe is possible without a client key, so the log line is the signal.
cid="$(docker compose --env-file /srv/outer/release.env \
    -f /srv/outer/compose.yaml ps -q outer)"
[[ -n "$cid" ]] || {
    echo "remote: no outer container after up" >&2
    exit 1
}
deadline=$((SECONDS + 60))
while true; do
    # The image check catches a compose that kept the old container.
    if [[ "$(docker inspect -f '{{.State.Running}} {{.Config.Image}}' "$cid")" == "true outer:$SHA" ]] \
        && docker logs "$cid" 2>&1 | grep -q 'listening on'; then
        break
    fi
    if [[ $SECONDS -ge $deadline ]]; then
        echo "remote: outer did not come up; last log lines:" >&2
        docker logs --tail 50 "$cid" >&2 || true
        exit 1
    fi
    sleep 2
done
echo "remote: outer:$SHA up ($cid)"

# Keep the last 3 outer images, prune older (in-use images fail, ignored).
docker images --format '{{.CreatedAt}} {{.Repository}}:{{.Tag}}' \
    | grep ' outer:' | sort -r | awk '{print $NF}' | tail -n +4 \
    | xargs -r docker rmi >/dev/null 2>&1 || true
EOF

echo "released $IMAGE to $TARGET"
