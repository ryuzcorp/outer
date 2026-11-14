#!/usr/bin/env bash
#
# Outer release: build the image from the current HEAD and roll it out with
# no downtime (deploy/handoff.sh).
#
#   deploy/release.sh <ssh-target>
#
#   Engine: OUTER_ENGINE=docker|podman selects the local container engine for
#   build and save. OUTER_ENGINE=auto (the default when unset or empty) keeps
#   today's behaviour: prefer podman, fall back to docker. Any other value
#   refuses at once. The VPS side always uses docker (provision.sh installs
#   it; compose and `docker load` run there).
#
#   Refuses a dirty tree. Builds the image, streams it with save|load (no
#   registry), then runs the new image's own `outer check-config` on the host
#   — as a `docker run --rm` one-off with exactly the environment and mounts
#   the compose `outer` service gets (`--env-file /srv/outer/outer.env`, the
#   read-only /tls mount, same OUTER_HOST_ID handling) — BEFORE anything
#   changes prod. A non-zero exit aborts the release here with prod untouched:
#   no uploads, no backup, no handoff. Only then uploads compose.yaml + the
#   firewall files + handoff.sh to /srv/outer, then hands the new image out:
#   the newcomer shares 443 via SO_REUSEPORT while the old container drains
#   (refuses new deploys, exits once in-flight ones finish), and the compose
#   service is recreated last onto the same image. At no point is 443 without
#   a listener, and no in-flight deploy dies with its process.
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
# migration needs an announced maintenance release, not this script. Every
# release also files a pre-release copy of the platform snapshot before the
# handoff (see below), the backstop when a migration misbehaves anyway.
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
# Local container engine for build and save. OUTER_ENGINE=auto (the default
# when unset or empty) keeps today's behaviour: prefer podman, fall back to
# docker. An explicit engine must exist on PATH; anything else refuses.
ENGINE="${OUTER_ENGINE:-auto}"
case "$ENGINE" in
auto)
    if command -v podman >/dev/null 2>&1; then
        ENGINE=podman
    elif command -v docker >/dev/null 2>&1; then
        ENGINE=docker
    else
        echo "release.sh: need podman or docker on PATH (or set OUTER_ENGINE)" >&2
        exit 1
    fi
    ;;
docker | podman) ;;
*)
    echo "release.sh: OUTER_ENGINE=$ENGINE is not one of: auto, docker, podman" >&2
    exit 1
    ;;
esac
if ! command -v "$ENGINE" >/dev/null 2>&1; then
    echo "release.sh: OUTER_ENGINE=$ENGINE is not on PATH" >&2
    exit 1
fi
IMAGE="outer:$SHA"
if [[ "$ENGINE" == "podman" ]]; then
    # podman names unqualified tags localhost/outer:<sha>, and docker load
    # keeps that name; the retag step below restores the plain name.
    podman build --format docker -t "$IMAGE" -f docker/Dockerfile .
    podman save "$IMAGE" | ssh "$TARGET" docker load
else
    docker build -t "$IMAGE" -f docker/Dockerfile .
    docker save "$IMAGE" | ssh "$TARGET" docker load
fi

# A podman-built image arrives named localhost/outer:<sha> (docker load
# keeps the name); retag it to the plain name now, before the preflight and
# the rollout consume it. Retagging changes nothing that is running. (A
# docker-built image is already plain-named, so the tag is a silent no-op
# there.)
# SHA is hex-validated above; expanding it here on the workstation is
# intended, same as the rollout heredoc below.
# shellcheck disable=SC2029
 if ssh "$TARGET" docker tag "localhost/outer:$SHA" "outer:$SHA" 2>/dev/null; then
     ssh "$TARGET" docker rmi "localhost/outer:$SHA" >/dev/null
 fi

 # Preflight: the new image proves it accepts the live config BEFORE anything
 # changes prod (no uploads, no backup, no handoff below). Same environment
 # and mounts the compose `outer` service gets: --env-file carries
 # /srv/outer/outer.env (including OUTER_HOST_ID when the operator set it;
 # otherwise outer defaults to the container hostname on both paths), and
 # /tls is the same read-only mount (OUTER_REVIEW_LOGINS_FILE and the TLS
 # triple resolve under it). check-config has no side effects (no DB,
 # no network, no sockets) and names variables, never values. --pull=never:
 # a missing local image must fail here, never pull a same-named registry
 # image and run it with the production env. A non-zero exit aborts the
 # release here with prod untouched: past aborts this must catch are a
 # malformed OUTER_REVIEW_LOGINS PHC, a review-logins path missing its /tls
 # mount, and STRIPE_* missing off localhost.
 if ! ssh "$TARGET" docker run --rm --pull=never --env-file /srv/outer/outer.env \
     -v /srv/outer/tls:/tls:ro "$IMAGE" check-config; then
    echo "release.sh: preflight check-config on $IMAGE failed; release aborted, prod untouched (fix the config or the image, then release again)" >&2
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

source /srv/outer/handoff.sh
# Whatever fails below, the heartbeat must not outlive the release: abort
# clears the flag and removes outer-next only while a compose outer serves.
trap 'handoff_abort' EXIT
# Lineage across the handoff needs no release-time step: the upload gate
# adopts a pre-lineage bucket meta naming this host on every attempt
# (re-applied while the bucket meta stays legacy, so the old container's
# last uploads during the overlap can't wedge the newcomer). Boot-time
# adoption is gone for the same reason. `state adopt-lineage` remains as a
# manual operator command.
# Pre-release copy of the platform state (audit O-4): file
# outer/state/history/release-<UTC timestamp>-<SHA>.sqlite.zst from the
# bucket's live snapshot before the newcomer can migrate it, so a bad
# migration reaching the live key can be restored from history. Skips
# (exit 0) when the bucket holds no snapshot yet. SHA is hex (validated
# above), so interpolating it into the docker args is safe. No data mount:
# the subcommand only reads config env and the bucket, never the local DB.
# validation reads files under /tls (e.g. OUTER_REVIEW_LOGINS_FILE).
# --pull=never: the image is local-only (same as the preflight above).
docker run --rm --pull=never --env-file /srv/outer/outer.env -v /srv/outer/tls:/tls:ro \
    "outer:$SHA" state release-backup --sha "$SHA"
handoff_to "outer:$SHA"
trap - EXIT
EOF

echo "released $IMAGE to $TARGET"
