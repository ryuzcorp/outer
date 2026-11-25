#!/usr/bin/env bash
#
# Outer release: build the image from the current HEAD and roll it out with
# no downtime (deploy/handoff.sh).
#
#   deploy/release.sh <ssh-target> [--fresh-install]
#
#   `--fresh-install` is the documented fresh path for a wiped box (see
#   deploy/WIPE.md): the migration-checksum gate below cannot read a prod DB
#   that does not exist, so the flag skips it — and refuses when the remote
#   DB is present, so a fresh release can never run over live state. The
#   rollout then brings the compose service up directly instead of a
#   handoff (there is no old container to drain).
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
#   read-only /tls and /data mounts, same OUTER_HOST_ID handling) — BEFORE anything
#   changes prod. A non-zero exit aborts the release here with prod untouched:
#   no uploads, no backup, no handoff. Only then uploads compose.yaml + the
#   firewall files + handoff.sh to /srv/outer, then hands the new image out:
#   the newcomer shares the loopback origin port via SO_REUSEPORT while the
#   old container drains (refuses new deploys, exits once in-flight ones
#   finish), and the compose service is recreated last onto the same image.
#   At no point is the origin without a listener, and no in-flight deploy
#   dies with its process.
#
# Health check from the box: outer serves plain HTTP on loopback behind the
# tunnel, so no edge probe applies — the check is: the container is running
# on the new image and outer logged its "listening on" line. Rollback:
# deploy/rollback.sh <ssh-target> (same handoff, previous image).
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
TARGET="${1:?usage: deploy/release.sh <ssh-target> [--fresh-install]}"
FRESH_INSTALL=0
if [[ "${2:-}" == "--fresh-install" ]]; then
    FRESH_INSTALL=1
elif [[ -n "${2:-}" ]]; then
    echo "release.sh: usage: deploy/release.sh <ssh-target> [--fresh-install]" >&2
    exit 1
fi
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
# On `--fresh-install` the gate is skipped instead: a wiped box has no DB
# to read, and the missing DB is required (a surviving DB refuses below,
# so a fresh release can never run over live state).
MIGRATIONS="crates/outer/migrations"
if [[ "$FRESH_INSTALL" == 1 ]]; then
    if ssh "$TARGET" "test -e /srv/outer/data/outer.sqlite" 2>/dev/null; then
        echo "release.sh: --fresh-install refuses: /srv/outer/data/outer.sqlite exists on $TARGET (run a normal release)" >&2
        exit 1
    fi
else
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
fi
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
    # keeps that name. Require the retag to the plain name here, before the
    # preflight and the rollout consume it: a failed retag aborts (set -e),
    # never letting a stale outer:$SHA pass the preflight. Retagging changes
    # nothing that is running.
    # SHA is hex-validated above; expanding it here on the workstation is
    # intended, same as the rollout heredoc below.
    podman build --format docker -t "$IMAGE" -f docker/Dockerfile .
    podman save "$IMAGE" | ssh "$TARGET" docker load
    # shellcheck disable=SC2029
    ssh "$TARGET" docker tag "localhost/outer:$SHA" "outer:$SHA"
    # shellcheck disable=SC2029
    ssh "$TARGET" docker rmi "localhost/outer:$SHA" >/dev/null
else
    docker build -t "$IMAGE" -f docker/Dockerfile .
    docker save "$IMAGE" | ssh "$TARGET" docker load
fi

# Preflight: the new image proves it accepts the live config BEFORE anything
# changes prod (no uploads, no backup, no handoff below). Same environment
# and mounts the compose `outer` service gets: --env-file carries
# /srv/outer/outer.env (including OUTER_HOST_ID when the operator set it;
# otherwise outer defaults to the container hostname on both paths), and
# every compose volume read-only: /tls (OUTER_REVIEW_LOGINS_FILE lives under
# it) and /data (the live DB dir; check-config never opens it, and read-only
# keeps even a buggy validation from touching prod).
# check-config has no side effects (no DB, no network, no sockets) and names
# variables, never values. --pull=never: a missing local image must fail
# here, never pull a same-named registry image and run it with the
# production env. A non-zero exit aborts the release here with prod
# untouched: past aborts this must catch are a malformed OUTER_REVIEW_LOGINS
# PHC, a review-logins path missing its /tls mount, and STRIPE_* missing off
# localhost.
if ! ssh "$TARGET" docker run --rm --pull=never --env-file /srv/outer/outer.env \
    -v /srv/outer/tls:/tls:ro -v /srv/outer/data:/data:ro "$IMAGE" check-config; then
    echo "release.sh: preflight check-config on $IMAGE failed; release aborted, prod untouched (fix the config or the image, then release again)" >&2
    exit 1
fi

ssh "$TARGET" mkdir -p /srv/outer
scp deploy/compose.yaml deploy/handoff.sh deploy/firewall.sh deploy/firewall.nft \
    deploy/firewall.service "$TARGET:/srv/outer/"

# Remote rollout. SHA is hex (validated above), so interpolating it into the
# remote script is safe; the heredoc is otherwise quoted. FRESH_INSTALL is
# 0/1 from this script's own flag parsing, never operator input.
ssh "$TARGET" bash -s -- "$SHA" "$FRESH_INSTALL" <<'EOF'
set -euo pipefail
SHA="$1"
FRESH_INSTALL="${2:-0}"
cd /srv/outer

[[ -f /srv/outer/outer.env ]] || {
    echo "remote: /srv/outer/outer.env missing (copy deploy/outer.env.example and fill it)" >&2
    exit 1
}

# Outer runs on the host network behind the tunnel: the closed ingress filter
# must be in place before it starts. The unit file is reinstalled in case it
# changed, then firewall.sh applies the static ruleset atomically (--check
# fails closed first); either failure stops the rollout here.
install -m 0644 /srv/outer/firewall.service /etc/systemd/system/firewall.service
systemctl daemon-reload
/srv/outer/firewall.sh

source /srv/outer/handoff.sh
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
# (exit 0) when the bucket holds no snapshot yet — the fresh-box case — so
# this step runs on both paths. SHA is hex (validated
# above), so interpolating it into the docker args is safe. No data mount:
# the subcommand only reads config env and the bucket, never the local DB.
# validation reads files under /tls (e.g. OUTER_REVIEW_LOGINS_FILE).
# --pull=never: the image is local-only (same as the preflight above).
docker run --rm --pull=never --env-file /srv/outer/outer.env -v /srv/outer/tls:/tls:ro \
    "outer:$SHA" state release-backup --sha "$SHA"
if [[ "$FRESH_INSTALL" == 1 ]]; then
    # Fresh box: no old container to drain and no handoff heartbeat. Re-check
    # the DB is still absent right before starting (the local gate ran
    # minutes earlier, before the build), then bring the compose service up
    # directly on the new image.
    if test -e /srv/outer/data/outer.sqlite; then
        echo "release.sh: --fresh-install refuses: /srv/outer/data/outer.sqlite appeared during the release" >&2
        exit 1
    fi
    # Record first: compose reads OUTER_IMAGE from release.env, which the
    # wipe keeps, so starting before recording would boot the pre-wipe image
    # on the empty data dir.
    record_image "outer:$SHA"
    docker compose --env-file /srv/outer/release.env \
        -f /srv/outer/compose.yaml up -d
    wait_for_listening outer-outer-1 "outer:$SHA"
else
    # Whatever fails below, the heartbeat must not outlive the release: abort
    # clears the flag and removes outer-next only while a compose outer serves.
    trap 'handoff_abort' EXIT
    handoff_to "outer:$SHA"
    trap - EXIT
fi
EOF

echo "released $IMAGE to $TARGET"
