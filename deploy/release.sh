#!/usr/bin/env bash
#
# Outer release: build the image from the current HEAD and roll it out with
# no downtime (deploy/handoff.sh).
#
#   deploy/release.sh <ssh-target>
#
# Refuses a dirty tree. Prefers podman, falls back to docker. Streams the
# image with save|load (no registry), uploads compose.yaml + the firewall
# files + handoff.sh to /srv/outer, then hands the new image out: the
# newcomer shares 443 via SO_REUSEPORT while the old container drains
# (refuses new deploys, exits once in-flight ones finish), and the compose
# service is recreated last onto the same image. At no point is 443 without
# a listener, and no in-flight deploy dies with its process.
#
# Health check from the box: outer serves TLS with required client
# certificates (mTLS), so no unauthenticated probe can pass the handshake —
# not even `curl --resolve ... https://outer.now/health` from here. The check
# is therefore: the container is running on the new image and outer logged
# its "listening on" line. Rollback: deploy/rollback.sh <ssh-target> (same
# handoff, previous image).
#
# Migrations are append-only within a release line: the newcomer runs them
# at boot while the old container is still serving, so a release's
# migrations must be additive (nullable columns, new tables). A destructive
# migration needs an announced maintenance release, not this script.
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
    echo "release.sh: unexpected short SHA: $SHA" >&2
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
    echo "release.sh: cannot read prod migrations from $TARGET (is outer up?)" >&2
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
scp deploy/compose.yaml deploy/handoff.sh deploy/firewall.sh deploy/firewall.service \
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

# outer runs on the host network: the ingress filter must be in place before
# it starts (a first install may predate it). firewall.sh exits nonzero on
# failure, which stops the rollout here.
if ! nft list table inet outer_fw >/dev/null 2>&1; then
    /srv/outer/firewall.sh
fi

# shellcheck disable=SC1091
source /srv/outer/handoff.sh
# Whatever fails below, the heartbeat must not outlive the release and the
# flag must not stay fresh: both would refuse deploys for no reason.
trap 'handoff_clear; docker rm -f outer-next >/dev/null 2>&1 || true' EXIT
handoff_to "outer:$SHA"
trap - EXIT
EOF

echo "released $IMAGE to $TARGET"
