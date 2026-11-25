#!/bin/sh
# Build-probe layer of the §12.7 hostile suite, runnable on the prod VPS
# WITHOUT cargo (neither the `outer-outer-1` container nor the host has a
# toolchain; the container does have bun/node, nft and setpriv).
#
# It mirrors `hostile_build_probes_are_contained` plus
# `hostile_cpu_spin_is_capped_and_neighbour_keeps_p95` in
# `crates/outer/tests/hostile.rs`, which cannot run here: same fixture
# layout (sibling 0700 dirs owned by a sibling uid, a root-only canary, a
# data dir, cgroup leaves with memory.max/pids.max/cpu.max under the Outer
# anchor `/sys/fs/cgroup/outer`), same `HOSTILE_*` env names, same verdict
# rule (every must-fail probe in `e2e/hostile/manifest.json` contained,
# every must-succeed control green).
#
# Run as root INSIDE the container (host networking lets the nft output
# path match what prod polices):
#
#   scp -r e2e/hostile outer-vps:/tmp/hostile-fixtures
#   ssh outer-vps 'docker cp /tmp/hostile-fixtures outer-outer-1:/tmp/hostile-fixtures'
#   ssh outer-vps "sqlite3 /srv/outer/data/outer.sqlite 'SELECT uid, slug FROM app ORDER BY uid;'"
#   ssh outer-vps 'docker exec -i outer-outer-1 bash -s 201022 201023' < scripts/hostile-probe-vps.sh
#
# Arguments: PROBE_UID SIBLING_UID [FIXTURE_DIR] (default /tmp/hostile-fixtures).
# Pick both uids at the TOP of OUTER_APP_UID_RANGE that no app owns (verify
# with the sqlite query above first): real apps allocate from the bottom up,
# and the uids must be inside the range or the `inet outer` policy never
# applies to them. The script re-checks inside the container when the
# database file is visible there, and always checks the installed nft table
# actually covers PROBE_UID, skipping loudly otherwise.
#
# Verdicts: prints the probe's JSON report. Exit 0 when every must-fail
# probe stayed contained and every control passed; exit 1 when any
# must-fail probe leaked. Missing prerequisites print SKIP and exit 0 —
# a skip is a gap, not a pass.
#
# Safe on a live host: bounded (the build layer under `timeout 300` in a
# 512 MiB / 512-pid leaf; the cpu layer under `timeout 90` per probe,
# settling in ~25 s), read-only toward prod (the nft table is only listed,
# the database only selected from), the canary is a fresh temp file (not
# the real outer.sqlite), and a trap removes the leaves, the dirs, the
# listener and both cpu runners. The uids need no cleanup: they are bare
# numbers (the image pre-creates the whole range in /etc/passwd), no users
# are created.
set -eu

log() { printf 'hostile-probe-vps: %s\n' "$*"; }
die() { printf 'hostile-probe-vps: %s\n' "$*" >&2; exit 1; }
skip() { printf 'hostile-probe-vps: SKIP: %s\n' "$*"; exit 0; }
# Single-quote a value for a generated script or env assignment.
sq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

# `bash -s PROBE_UID SIBLING_UID` (documented above); env vars also work.
PROBE_UID="${1:-${PROBE_UID:-}}"
SIBLING_UID="${2:-${SIBLING_UID:-}}"
FIXTURES="${3:-${FIXTURES:-/tmp/hostile-fixtures}}"
case "$PROBE_UID" in ''|*[!0-9]*) die "PROBE_UID must be a non-negative integer" ;; esac
case "$SIBLING_UID" in ''|*[!0-9]*) die "SIBLING_UID must be a non-negative integer" ;; esac
[ "$PROBE_UID" != "$SIBLING_UID" ] || die "uids must be distinct"

# Harness-only canary for the secret:build-platform-env and
# secret:proc-environ probes. This stays in the HARNESS environ: the probe
# runs under `env -i` with an explicit assignment list (see runner.sh
# below), so the probe proves it cannot see either name. Never reuse a real
# platform secret here (OUTER_SECRET, STRIPE_SECRET_KEY, …): a harness leak
# would then cost money.
HOSTILE_PLATFORM_CANARY_VALUE="hostile-canary-$(date +%s)-$$-$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
export HOSTILE_PLATFORM_CANARY_VALUE
export HOSTILE_HARNESS_PID="$$"
export OUTER_HOSTILE_PROBE_CANARY="$HOSTILE_PLATFORM_CANARY_VALUE"


# The app range prod polices (config.rs default when the env is absent here).
RANGE="${OUTER_APP_UID_RANGE:-200000-201023}"
RANGE_LO="${RANGE%%-*}"
RANGE_HI="${RANGE##*-}"
case "$RANGE_LO$RANGE_HI" in *[!0-9]*) die "OUTER_APP_UID_RANGE=$RANGE is not lo-hi" ;; esac
if [ "$PROBE_UID" -lt "$RANGE_LO" ] || [ "$SIBLING_UID" -gt "$RANGE_HI" ]; then
    die "uids $PROBE_UID/$SIBLING_UID fall outside OUTER_APP_UID_RANGE $RANGE (pick spare top-of-range uids)"
fi

[ -f "$FIXTURES/build-probe.mjs" ] || die "$FIXTURES/build-probe.mjs missing (scp e2e/hostile first; see header)"
[ -f "$FIXTURES/manifest.json" ] || die "$FIXTURES/manifest.json missing (scp e2e/hostile first; see header)"
if command -v bun >/dev/null; then RUNTIME="bun"; elif command -v node >/dev/null; then RUNTIME="node"; else
    skip "no bun or node on PATH"
fi
command -v setpriv >/dev/null || skip "no setpriv (util-linux) on PATH"
command -v timeout >/dev/null || skip "no timeout (coreutils) on PATH"
command -v nft >/dev/null || skip "no nft on PATH"
[ -e /sys/fs/cgroup/cgroup.controllers ] || skip "no cgroup v2 mount at /sys/fs/cgroup"

# The uids must be spare: refuse ones the app table owns when the database
# is visible from in here (the integrator already checked from the host).
DB=""
for candidate in "${OUTER_DATA:-/data}/outer.sqlite" /srv/outer/data/outer.sqlite; do
    if [ -r "$candidate" ]; then DB="$candidate"; break; fi
done
if [ -n "$DB" ] && command -v sqlite3 >/dev/null; then
    OWNER="$(sqlite3 "$DB" "SELECT slug FROM app WHERE uid IN ($PROBE_UID,$SIBLING_UID) LIMIT 1;")"
    [ -z "$OWNER" ] || die "uid already owned by app $OWNER (pick spare uids)"
    log "uids $PROBE_UID/$SIBLING_UID are spare in $DB"
else
    log "WARN: app database not visible in here; uids taken as spare on your host check"
fi

# The installed table must cover the probe uid: a refused connection proves
# the policy, not an absent server, only then.
DUMP="$(nft list table inet outer 2>/dev/null)" || skip "nft table inet outer not installed (outer installs it at boot)"
COVERED=""
for token in $(printf '%s\n' "$DUMP" | grep -o 'skuid [0-9-][0-9-]*' | cut -d' ' -f2); do
    case "$token" in
        *-*) lo="${token%%-*}"; hi="${token##*-}" ;;
        *) lo="$token"; hi="$token" ;;
    esac
    case "$lo$hi" in *[!0-9]*) continue ;; esac
    if [ "$lo" -le "$PROBE_UID" ] && [ "$PROBE_UID" -le "$hi" ]; then COVERED="$token"; break; fi
done
[ -n "$COVERED" ] || skip "inet outer installed but no skuid rule covers uid $PROBE_UID (reinstall it for $RANGE)"
log "nft table inet outer covers uid $PROBE_UID via skuid $COVERED"

BASE="$(mktemp -d /tmp/hostile-XXXXXX)"
LEAF=""
LISTENER_PID=""
SPIN_PID=""
SERVER_PID=""
CLIENT_PID=""
SPIN_LEAF=""
NEIGH_LEAF=""
cleanup() {
    if [ -n "$LISTENER_PID" ]; then kill "$LISTENER_PID" 2>/dev/null || true; fi
    if [ -n "$SPIN_PID" ]; then kill "$SPIN_PID" 2>/dev/null || true; fi
    if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
    if [ -n "$CLIENT_PID" ]; then kill "$CLIENT_PID" 2>/dev/null || true; fi
    for dead in "$LEAF" "$SPIN_LEAF" "$NEIGH_LEAF"; do
        if [ -n "$dead" ]; then
            echo 1 > "$dead/cgroup.kill" 2>/dev/null || true
            rmdir "$dead" 2>/dev/null || true
        fi
    done
    rm -rf "$BASE"
}
trap cleanup EXIT INT TERM

# Fail closed on internet: the positive control needs a network that
# reaches it, proven from this (unconfined) uid first. (A file, not -e:
# node runs -e as CJS, where top-level await is a syntax error.)
cat > "$BASE/check-net.mjs" <<'EOF'
const res = await fetch("https://example.com/", { signal: AbortSignal.timeout(8000) });
if (!res.ok) throw new Error(`status ${res.status}`);
EOF
if ! "$RUNTIME" "$BASE/check-net.mjs" >/dev/null 2>&1; then
    skip "no route to the internet for the positive control"
fi

ANCHOR="/sys/fs/cgroup/outer"
[ -d "$ANCHOR" ] || skip "$ANCHOR missing (outer serve creates it at boot; is serve running?)"
WORK="$BASE/work/hostile"
CACHE="$BASE/cache/hostile"
mkdir -p "$WORK" "$CACHE/bun" "$CACHE/home" "$CACHE/tmp" "$BASE/work/neighbour" "$BASE/fleets/neighbour"
printf 'neighbour code\n' > "$BASE/work/neighbour/index.js"
printf 'neighbour code\n' > "$BASE/fleets/neighbour/index.js"
CANARY="$BASE/outer.sqlite"
: > "$CANARY"
# Sibling dirs: real 0700 dirs owned by another in-range uid, each with a
# readable-looking file, so refusals prove isolation, not absence.
chown "$SIBLING_UID:$SIBLING_UID" "$BASE/work/neighbour" "$BASE/work/neighbour/index.js" "$BASE/fleets/neighbour" "$BASE/fleets/neighbour/index.js"
chmod 0700 "$BASE/work/neighbour" "$BASE/work/neighbour/index.js" "$BASE/fleets/neighbour" "$BASE/fleets/neighbour/index.js"
# Root-only canary: the stand-in for outer.sqlite and host keys.
chown 0:0 "$CANARY"
chmod 0600 "$CANARY"
# Data-dir layout: roots traverse but do not list (0711), app leaves private.
chown "$PROBE_UID:$PROBE_UID" "$WORK" "$CACHE" "$CACHE/bun" "$CACHE/home" "$CACHE/tmp"
chmod 0700 "$WORK" "$CACHE"
chmod 0711 "$BASE" "$BASE/work" "$BASE/cache" "$BASE/fleets"
cp "$FIXTURES/build-probe.mjs" "$WORK/build-probe.mjs"
chown "$PROBE_UID:$PROBE_UID" "$WORK/build-probe.mjs"

# A live loopback target the harness proves serves from another uid, so the
# sandbox uid's refusal proves the nft policy, not an absent server.
if [ "$RUNTIME" = "bun" ]; then
    LISTENER="$BASE/listener.mjs"
    cat > "$LISTENER" <<'EOF'
const server = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: () => new Response("ok") });
console.log(`PORT=${server.port}`);
await new Promise(() => {});
EOF
else
    LISTENER="$BASE/listener.cjs"
    cat > "$LISTENER" <<'EOF'
const http = require("node:http");
const server = http.createServer((req, res) => res.end("ok"));
server.listen(0, "127.0.0.1", () => console.log(`PORT=${server.address().port}`));
EOF
fi
"$RUNTIME" "$LISTENER" > "$BASE/port" 2> "$BASE/listener.log" &
LISTENER_PID="$!"
PORT=""
for _ in $(seq 1 50); do
    PORT="$(sed -n 's/^PORT=//p' "$BASE/port" 2>/dev/null | head -1)"
    [ -n "$PORT" ] && break
    sleep 0.2
done
[ -n "$PORT" ] || { cat "$BASE/listener.log" >&2; die "loopback listener never printed a port"; }
TARGET="http://127.0.0.1:$PORT/health"
{ printf 'const res = await fetch(%s);\n' "$(sq "$TARGET")"; printf 'if (!res.ok) throw new Error("status " + res.status);\n'; } > "$BASE/check-serve.mjs"
"$RUNTIME" "$BASE/check-serve.mjs" || die "the loopback target does not serve from this uid"
log "loopback target serves at $TARGET"

# A real cgroup leaf with kernel-enforced limits: write each limit and read
# it back, like cgroups::write_limit — a silent no-op (read-only mount, no
# delegation) skips instead of running uncapped.
LEAF="$ANCHOR/hostile-$(date +%s)-$$"
mkdir "$LEAF" || skip "cannot create cgroup leaf under $ANCHOR"
for spec in "memory.max 536870912" "pids.max 512" "cpu.max ${OUTER_BUILD_CPU_MAX:-200000 100000}"; do
    # shellcheck disable=SC2086
    set -- $spec
    # A bare quota would keep the kernel's default period; write the full
    # value so the leaf carries exactly the configured quota and period.
    echo "$2${3:+ $3}" > "$LEAF/$1" 2>/dev/null || skip "cannot write $LEAF/$1 (no delegation?)"
    [ "$(cat "$LEAF/$1")" = "$2${3:+ $3}" ] || skip "$1 did not apply (wrote $2, have $(cat "$LEAF/$1"))"
done
log "leaf $LEAF carries memory.max=536870912 pids.max=512 cpu.max=${OUTER_BUILD_CPU_MAX:-200000 100000}"

# The child's explicit env, mirroring sandbox.rs: base vars plus the HOSTILE_*
# fixture pointers. Reserved platform names an app must never set are
# planted, then dropped by the same prefix list as is_reserved_env_name —
# what the env:reserved-dropped probe exercises.
ASSIGN=""
putenv() { ASSIGN="$ASSIGN $1=$(sq "$2")"; }
putenv PATH "/usr/local/bin:/usr/bin:/bin"
putenv HOME "$CACHE/home"
putenv TMPDIR "$CACHE/tmp"
putenv LANG "C.UTF-8"
putenv CI "1"
putenv NO_COLOR "1"
putenv NODE_ENV "production"
putenv BUN_INSTALL_CACHE_DIR "$CACHE/bun"
putenv APP_KEY "allowed"
putenv AWS_ACCESS_KEY_ID "scoped-test-key"
putenv AWS_SESSION_TOKEN "scoped-test-token"
putenv HOSTILE_SIBLING_WORK "$BASE/work/neighbour"
putenv HOSTILE_SIBLING_DATA "$BASE/fleets/neighbour"
putenv HOSTILE_CANARY_PATH "$CANARY"
putenv HOSTILE_DATA_DIR "$BASE"
putenv HOSTILE_LOOPBACK_TARGET "$TARGET"
putenv HOSTILE_CGROUP_LEAF "$LEAF"
putenv HOSTILE_EXPECT_MEMORY_MAX "536870912"
# Planted platform names the filter must drop before the child starts.
for planted in "OUTER_SECRET planted" "STRIPE_SECRET_KEY planted" "AWS_SECRET_ACCESS_KEY planted" "CLOUDFLARE_API_TOKEN planted"; do
    # shellcheck disable=SC2086
    set -- $planted
    case "$1" in
        PORT|HOST|NODE_OPTIONS) continue ;;
        AWS_*|S3_*|CELLD_*|CELLO_*|OUTER_*|CLOUDFLARE_*|R2_*|STRIPE_*|RESEND_*|JEV_*|LD_*|BUN_*)
            [ "$1" = "BUN_INSTALL_CACHE_DIR" ] || continue ;;
    esac
    putenv "$1" "$2"
done
# Move OURSELVES into the leaf while still root (like self_move_into_leaf:
# the move precedes the first fork by construction), then drop to the probe
# uid and exec the probe with the explicit env above. Values are
# single-quoted by sq(), so this split is safe.
{
    printf '#!/bin/sh\nset -eu\n'
    printf 'echo $$ > %s\n' "$(sq "$LEAF/cgroup.procs")"
    printf 'cd %s\n' "$(sq "$WORK")"
    # shellcheck disable=SC2086
    printf 'exec setpriv --reuid %s --regid %s --clear-groups env -i %s %s %s\n' \
        "$PROBE_UID" "$PROBE_UID" "$ASSIGN" "$(sq "$RUNTIME")" "$(sq "$WORK/build-probe.mjs")"
} > "$BASE/runner.sh"
chmod 0755 "$BASE/runner.sh"

log "running build-probe.mjs as uid $PROBE_UID in $LEAF"
status=0; timeout 300 sh "$BASE/runner.sh" > "$BASE/stdout.log" 2> "$BASE/stderr.log" || status="$?"
if [ "$status" != 0 ]; then
    tail -30 "$BASE/stderr.log" >&2
    die "probe run failed (exit $status); logs above"
fi

# Fail closed on the fixture contract, like assert_contained: every
# must-fail build probe present and contained (ok:false), every
# must-succeed control present and green. Unknown green probes leak too.
cat > "$BASE/eval.mjs" <<'EOF'
import { readFileSync } from "node:fs";
const manifest = JSON.parse(readFileSync(process.argv[2], "utf-8"));
const report = JSON.parse(readFileSync(process.argv[3], "utf-8"));
if (report.phase !== "build") {
  console.error(`report phase is ${report.phase}, want build`);
  process.exit(1);
}
const wanted = manifest.probes.filter((p) => p.phase === "build");
const missing = wanted.filter((p) => !(p.id in report.probes)).map((p) => p.id);
let leaked = [];
for (const probe of wanted) {
  const result = report.probes[probe.id];
  if (!result) continue;
  const bad = probe.must === "fail" ? result.ok : !result.ok;
  console.log(`${bad ? "LEAK" : "held"} ${probe.id} ok=${result.ok} ${result.detail ?? ""}`);
  if (bad) leaked.push(probe.id);
}
for (const [id, result] of Object.entries(report.probes)) {
  if (id.startsWith("env:reserved-dropped:")) continue;
  const known = wanted.find((p) => p.id === id);
  if (!known && result.ok) {
    console.log(`LEAK ${id} (unknown probe) ${result.detail ?? ""}`);
    leaked.push(id);
  }
}
if (missing.length > 0) {
  console.error(`probes went missing: ${missing.join(", ")}`);
  process.exit(1);
}
if (leaked.length > 0) {
  console.error(`LEAKED: ${leaked.join(", ")}`);
  process.exit(1);
}
console.log(`all ${wanted.length} build probes held`);
EOF
REPORT="$WORK/dist/probe.json"
[ -f "$REPORT" ] || REPORT=""
if [ -z "$REPORT" ]; then
    # Fall back to the JSON line the probe prints, like the harness does.
    LINE="$(grep '"phase"' "$BASE/stdout.log" | tail -1)"
    [ -n "$LINE" ] || { cat "$BASE/stdout.log" >&2; die "the probe printed no JSON report"; }
    printf '%s\n' "$LINE" > "$BASE/probe.json"
    REPORT="$BASE/probe.json"
fi
log "probe report:"
cat "$REPORT"
"$RUNTIME" "$BASE/eval.mjs" "$FIXTURES/manifest.json" "$REPORT" || die "a must-fail probe leaked (see LEAK lines above)"
log "done: every must-fail build probe stayed contained"

# --- CPU-spin and neighbour-p95 layer (spec §12.7 M3 gate) ---
#
# A hostile build busy-looping in a leaf capped at the build cpu.max quota
# (`OUTER_BUILD_CPU_MAX`, default two CPUs) must burn no more than the quota
# allows (the spinner fixture measures its own cpu.stat), while the neighbour
# server in its own fleet-capped leaf (`OUTER_FLEET_CPU_MAX`) keeps serving
# /health with a p95 within the probe's bound (at most twice the idle
# baseline, or the baseline plus 50 ms of shared-vCore scheduling slack).
# The spinner mirrors a build, the server a fleet — exactly the leaves
# production writes. The measuring client runs UNCONFINED as root, like real
# external traffic: the nft `inet outer` table lets an app uid reach loopback
# only on its own registered fleet ports and rejects everything else, so a
# client running as an app uid cannot fetch even its own listener (the first
# prod run failed exactly there: the baseline never completed).
# Bounded: each process runs under `timeout 90` but the layer settles in
# ~25 s; the EXIT trap above kills all three runners and removes both
# leaves. Read-only toward prod data: only the nft table is listed (above),
# the fixture dirs below are fresh temp dirs, and the uids need no cleanup.
# Handoff files live in the tenant-owned workdirs, never in $BASE (0711
# root-owned, which the sandbox uids cannot write): GO in the spinner's dir,
# the rest in the neighbour's.
BUILD_QUOTA="${OUTER_BUILD_CPU_MAX:-200000 100000}"
FLEET_QUOTA="${OUTER_FLEET_CPU_MAX:-50000 100000}"
SPIN_LEAF="$ANCHOR/hostile-spin-$(date +%s)-$$"
NEIGH_LEAF="$ANCHOR/hostile-neigh-$(date +%s)-$$"
mkleaf() {
    _leaf="$1"; _mem="$2"; _cpu="$3"
    for spec in "memory.max $_mem" "pids.max 512" "cpu.max $_cpu"; do
        # shellcheck disable=SC2086
        set -- $spec
        echo "$2${3:+ $3}" > "$_leaf/$1" 2>/dev/null || skip "cannot write $_leaf/$1 (no delegation?)"
        [ "$(cat "$_leaf/$1")" = "$2${3:+ $3}" ] || skip "$1 did not apply (wrote $2, have $(cat "$_leaf/$1"))"
    done
}
mkdir "$SPIN_LEAF" || skip "cannot create cgroup leaf under $ANCHOR"
mkdir "$NEIGH_LEAF" || skip "cannot create cgroup leaf under $ANCHOR"
mkleaf "$SPIN_LEAF" 2147483648 "$BUILD_QUOTA"
mkleaf "$NEIGH_LEAF" 536870912 "$FLEET_QUOTA"
log "spin leaf carries cpu.max=$BUILD_QUOTA, neighbour leaf cpu.max=$FLEET_QUOTA"
SPIN_WORK="$BASE/work/spin"
NEIGH_WORK="$BASE/work/neigh"
CLIENT_WORK="$BASE/work/client"
mkdir -p "$SPIN_WORK" "$NEIGH_WORK" "$CLIENT_WORK" "$BASE/home-spin" "$BASE/tmp-spin" "$BASE/home-neigh" "$BASE/tmp-neigh" "$BASE/home-client" "$BASE/tmp-client"
cp "$FIXTURES/cpu-spin.mjs" "$SPIN_WORK/cpu-spin.mjs"
cp "$FIXTURES/neighbour-server.mjs" "$NEIGH_WORK/neighbour-server.mjs"
cp "$FIXTURES/neighbour-probe.mjs" "$CLIENT_WORK/neighbour-probe.mjs"
chown "$PROBE_UID:$PROBE_UID" "$SPIN_WORK" "$SPIN_WORK/cpu-spin.mjs" "$BASE/home-spin" "$BASE/tmp-spin"
chown "$SIBLING_UID:$SIBLING_UID" "$NEIGH_WORK" "$NEIGH_WORK/neighbour-server.mjs" "$BASE/home-neigh" "$BASE/tmp-neigh"
chmod 0700 "$SPIN_WORK" "$NEIGH_WORK"
GO="$SPIN_WORK/spin.go"
PORT="$NEIGH_WORK/port"
READY="$NEIGH_WORK/spin.ready"
DONE="$NEIGH_WORK/baseline.done"
STOP="$NEIGH_WORK/stop"
rm -f "$GO" "$PORT" "$READY" "$DONE" "$STOP"
# One runner per sandboxed tenant, same shape as the build runner above: move
# into the leaf while still root, then drop to the tenant uid with an explicit
# env. The CPU fixtures need no egress, so the env stays small. The client
# runs as root with its own minimal env — no runner, no leaf.
ASSIGN=""
putenv PATH "/usr/local/bin:/usr/bin:/bin"
putenv HOME "$BASE/home-spin"
putenv TMPDIR "$BASE/tmp-spin"
putenv HOSTILE_CGROUP_LEAF "$SPIN_LEAF"
putenv HOSTILE_SPIN_MS "15000"
putenv HOSTILE_SPIN_GO "$GO"
SPIN_ASSIGN="$ASSIGN"
{
    printf '#!/bin/sh\nset -eu\n'
    printf 'echo $$ > %s\n' "$(sq "$SPIN_LEAF/cgroup.procs")"
    printf 'cd %s\n' "$(sq "$SPIN_WORK")"
    # shellcheck disable=SC2086
    printf 'exec setpriv --reuid %s --regid %s --clear-groups env -i %s %s %s\n' \
        "$PROBE_UID" "$PROBE_UID" "$SPIN_ASSIGN" "$(sq "$RUNTIME")" "$(sq "$SPIN_WORK/cpu-spin.mjs")"
} > "$BASE/spin-runner.sh"
ASSIGN=""
putenv PATH "/usr/local/bin:/usr/bin:/bin"
putenv HOME "$BASE/home-neigh"
putenv TMPDIR "$BASE/tmp-neigh"
putenv HOSTILE_PORT_FILE "$PORT"
putenv HOSTILE_STOP_FILE "$STOP"
SERVER_ASSIGN="$ASSIGN"
{
    printf '#!/bin/sh\nset -eu\n'
    printf 'echo $$ > %s\n' "$(sq "$NEIGH_LEAF/cgroup.procs")"
    printf 'cd %s\n' "$(sq "$NEIGH_WORK")"
    # shellcheck disable=SC2086
    printf 'exec setpriv --reuid %s --regid %s --clear-groups env -i %s %s %s\n' \
        "$SIBLING_UID" "$SIBLING_UID" "$SERVER_ASSIGN" "$(sq "$RUNTIME")" "$(sq "$NEIGH_WORK/neighbour-server.mjs")"
} > "$BASE/server-runner.sh"
chmod 0755 "$BASE/spin-runner.sh" "$BASE/server-runner.sh"
log "running cpu-spin.mjs as uid $PROBE_UID in $SPIN_LEAF, neighbour-server.mjs as uid $SIBLING_UID in $NEIGH_LEAF, neighbour-probe.mjs unconfined"
timeout 90 sh "$BASE/server-runner.sh" > "$BASE/server-stdout.log" 2> "$BASE/server-stderr.log" &
SERVER_PID="$!"
timeout 90 sh "$BASE/spin-runner.sh" > "$BASE/spin-stdout.log" 2> "$BASE/spin-stderr.log" &
SPIN_PID="$!"
timeout 90 env -i PATH="/usr/local/bin:/usr/bin:/bin" HOME="$BASE/home-client" TMPDIR="$BASE/tmp-client" HOSTILE_PORT_FILE="$PORT" HOSTILE_SPIN_READY="$READY" HOSTILE_BASELINE_DONE="$DONE" HOSTILE_NEIGHBOUR_COUNT="200" "$RUNTIME" "$CLIENT_WORK/neighbour-probe.mjs" > "$BASE/client-stdout.log" 2> "$BASE/client-stderr.log" &
CLIENT_PID="$!"
# Driver: the server publishes its port, the client takes its unloaded
# baseline, then the spin starts; release the loaded loop once the workers
# ramped up, then stop the server once the client reported.
deadline=$(( $(date +%s) + 60 ))
while [ ! -f "$PORT" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || die "neighbour server never published its port"
    sleep 0.2
done
while [ ! -f "$DONE" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || die "neighbour probe never finished its baseline"
    sleep 0.2
done
: > "$GO"
sleep 2
: > "$READY"
status=0; wait "$SPIN_PID" || status="$?"
[ "$status" = 0 ] || { tail -30 "$BASE/spin-stderr.log" >&2; die "spinner run failed (exit $status)"; }
SPIN_PID=""
status=0; wait "$CLIENT_PID" || status="$?"
[ "$status" = 0 ] || { tail -30 "$BASE/client-stderr.log" >&2; die "neighbour client run failed (exit $status)"; }
CLIENT_PID=""
: > "$STOP"
status=0; wait "$SERVER_PID" || status="$?"
[ "$status" = 0 ] || { tail -30 "$BASE/server-stderr.log" >&2; die "neighbour server run failed (exit $status)"; }
SERVER_PID=""
# Fail closed on the cpu fixture contract, like assert_contained: the
# must-fail spin probe contained (ok:false), the must-succeed neighbour
# control green. Two reports merge into one verdict.
cat > "$BASE/eval-cpu.mjs" <<'EOF'
import { readFileSync } from "node:fs";
const manifest = JSON.parse(readFileSync(process.argv[2], "utf-8"));
const reports = process.argv.slice(3).map((f) => JSON.parse(readFileSync(f, "utf-8")));
for (const r of reports) {
  if (r.phase !== "cpu") {
    console.error(`report phase is ${r.phase}, want cpu`);
    process.exit(1);
  }
}
const merged = Object.assign({}, ...reports.map((r) => r.probes));
const wanted = manifest.probes.filter((p) => p.phase === "cpu");
const missing = wanted.filter((p) => !(p.id in merged)).map((p) => p.id);
let leaked = [];
for (const probe of wanted) {
  const result = merged[probe.id];
  if (!result) continue;
  const bad = probe.must === "fail" ? result.ok : !result.ok;
  console.log(`${bad ? "LEAK" : "held"} ${probe.id} ok=${result.ok} ${result.detail ?? ""}`);
  if (bad) leaked.push(probe.id);
}
for (const [id, result] of Object.entries(merged)) {
  const known = wanted.find((p) => p.id === id);
  if (!known && result.ok) {
    console.log(`LEAK ${id} (unknown probe) ${result.detail ?? ""}`);
    leaked.push(id);
  }
}
if (missing.length > 0) {
  console.error(`probes went missing: ${missing.join(", ")}`);
  process.exit(1);
}
if (leaked.length > 0) {
  console.error(`LEAKED: ${leaked.join(", ")}`);
  process.exit(1);
}
console.log(`all ${wanted.length} cpu probes held`);
EOF
SPIN_REPORT="$SPIN_WORK/dist/cpu-spin.json"
NEIGH_REPORT="$CLIENT_WORK/dist/neighbour.json"
[ -f "$SPIN_REPORT" ] || { LINE="$(grep '"phase"' "$BASE/spin-stdout.log" | tail -1)"; [ -n "$LINE" ] && { printf '%s\n' "$LINE" > "$BASE/cpu-spin.json"; SPIN_REPORT="$BASE/cpu-spin.json"; }; }
[ -f "$NEIGH_REPORT" ] || { LINE="$(grep '"phase"' "$BASE/client-stdout.log" | tail -1)"; [ -n "$LINE" ] && { printf '%s\n' "$LINE" > "$BASE/neighbour.json"; NEIGH_REPORT="$BASE/neighbour.json"; }; }
[ -f "$SPIN_REPORT" ] || { cat "$BASE/spin-stdout.log" >&2; die "the spinner printed no JSON report"; }
[ -f "$NEIGH_REPORT" ] || { cat "$BASE/client-stdout.log" >&2; die "the neighbour client printed no JSON report"; }
log "cpu probe reports:"
cat "$SPIN_REPORT" "$NEIGH_REPORT"
"$RUNTIME" "$BASE/eval-cpu.mjs" "$FIXTURES/manifest.json" "$SPIN_REPORT" "$NEIGH_REPORT" || die "a cpu probe leaked (see LEAK lines above)"
log "done: the spin stayed within quota and the neighbour kept its p95"
