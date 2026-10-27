#!/bin/sh
# The §12.7 hostile suite runner: what CI cannot do.
#
# CI (`.github/workflows/ci.yml`, job `hostile-gates`) runs the
# non-privileged layer on every PR touching `fleet/`, `deploy/`,
# `store/credentials.rs` or the Dockerfile. This script runs everything
# from a privileged host — the VPS, or a staging box shaped like it — and
# is the integrator's entry point for the M2 beta gate:
#
#   sudo scripts/hostile-suite.sh            # safe + privileged layers
#   sudo scripts/hostile-suite.sh --safe-only
#   sudo scripts/hostile-suite.sh --r2       # also the R2 scope layer
#
# What runs where:
# - Safe layer (any machine): `cargo test --locked hostile` asserts the
#   generated sandbox config for every fixture category — the nft ruleset
#   (SMTP blocked, loopback/RFC1918/metadata/CGNAT/ULA rejected), the
#   cgroup limits, the `fleets/{slug}/` credential shape, the reserved-env
#   filter, the loopback-only fleet command and the 0711/0700 data layout.
# - Privileged layer (needs root, cgroup v2 with delegation, the installed
#   `inet outer` nft table, bun or node): `build-probe.mjs` runs through
#   `run_sandboxed` as a scratch app uid in a 512 MiB / 512-pid leaf —
#   the same function `deploy::pipeline` runs install/build through — and
#   the suite fails unless every must-fail probe stayed contained. Probes
#   that cannot prove containment skip loudly instead of passing.
# - VPS container path (no cargo on the box): `scripts/hostile-probe-vps.sh`
#   runs the same build-probe layer as root inside `outer-outer-1` with bun
#   + setpriv, against the live `inet outer` table and the real
#   `/sys/fs/cgroup/outer` anchor. Fixtures ride in via scp + `docker cp`,
#   the two top-of-range uids travel as arguments (verified spare on the
#   host first). See that script's header for the exact commands.
# - R2 layer (`--r2`): `r2-probe.mjs` checks a *scoped* key round-trips
#   inside its own prefix and is refused (403/401) on a sibling prefix
#   and the bucket root. Mint it first (Cloudflare API, temporary
#   credentials on a fixture prefix) and export:
#     HOSTILE_S3_ENDPOINT HOSTILE_S3_BUCKET HOSTILE_S3_KEY
#     HOSTILE_S3_SECRET HOSTILE_S3_TOKEN HOSTILE_S3_PREFIX
#     HOSTILE_S3_SIBLING (default fleets/hostile-neighbour/)
# - Worker layer (manual, needs celld + a bucket): deploy the fixture as
#   two apps (`hostile`, `hostile-neighbour`), then
#     curl 'https://hostile.<apps-domain>/probes?neighbour=<port>&platform=<port>'
#   and confirm every worker probe is contained except `fetch:internet`.
#   Automating this needs a staging outer with a scratch bucket, which the
#   VPS (prod traffic) is not — so it stays a documented manual step.
#
# The fork-bomb and memory probes run bounded (300 short-lived spawns, a
# 1.5 GiB-touching child) inside the leaf's pids.max/memory.max, and the
# leaf is killed after the run. Do not run this on a host whose nft table
# is not installed: the tests refuse to pass vacuously and skip instead.
set -eu

log() { printf 'hostile-suite: %s\n' "$*"; }
die() { printf 'hostile-suite: %s\n' "$*" >&2; exit 1; }

if [ "${1:-all}" != "--safe-only" ] && [ "$(id -u)" != "0" ]; then
    die "run as root (the sandbox drops to app uids)"
fi
MODE="${1:-all}"
REPO="$(dirname "$(dirname "$(readlink -f "$0")")")"
cd "$REPO"
command -v cargo >/dev/null || die "cargo not on PATH"
[ -e /sys/fs/cgroup/cgroup.controllers ] || log "WARN: no cgroup v2 mount; privileged probes will skip"
if nft list table inet outer 2>/dev/null | grep -q skuid; then
    log "nft table inet outer is installed"
else
    log "WARN: nft table inet outer missing; egress probes will skip"
fi
if command -v bun >/dev/null; then
    log "js runtime: $(bun --version 2>/dev/null | head -1) (bun)"
elif command -v node >/dev/null; then
    log "js runtime: $(node --version 2>/dev/null) (node)"
else
    log "WARN: no bun or node; privileged probes will skip"
fi

log "safe layer: generated-config assertions"
cargo test --locked hostile

case "$MODE" in
    --safe-only) log "safe layer done (--safe-only)"; exit 0 ;;
    --r2) : ;;
    all) : ;;
    *) die "usage: $0 [--safe-only|--r2]" ;;
esac

if [ "$MODE" = "--r2" ]; then
    for name in HOSTILE_S3_ENDPOINT HOSTILE_S3_BUCKET HOSTILE_S3_KEY HOSTILE_S3_SECRET HOSTILE_S3_PREFIX; do
        eval "value=\${$name:-}"
        [ -n "$value" ] || die "$name is not exported (mint a scoped key first; see header)"
    done
fi

log "privileged layer: fixtures through the real sandbox (needs cgroupfs, nft, js runtime)"
cargo test --locked -- --ignored hostile

log "done. Anything marked SKIP above needs the prerequisite on this host;"
log "a skip is a gap, not a pass. The worker layer stays manual (see header)."
