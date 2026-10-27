#!/bin/sh
# Build-probe layer of the §12.7 hostile suite, runnable on the prod VPS
# WITHOUT cargo (neither the `outer-outer-1` container nor the host has a
# toolchain; the container does have bun/node, nft and setpriv).
#
# It mirrors `hostile_build_probes_are_contained` in
# `crates/outer/tests/hostile.rs`, which cannot run here: same fixture
# layout (sibling 0700 dirs owned by a sibling uid, a root-only canary, a
# data dir, a cgroup leaf with memory.max/pids.max under the Outer anchor
# `/sys/fs/cgroup/outer`), same `HOSTILE_*` env names, same verdict rule
# (every must-fail build probe in `e2e/hostile/manifest.json` contained,
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
# Safe on a live host: bounded (`timeout 300`, a 512 MiB / 512-pid leaf the
# probe's fork burst and memory hog die inside), read-only toward prod (the
# nft table is only listed, the database only selected from), the canary is
# a fresh temp file (not the real outer.sqlite), and a trap removes the
# leaf, the dirs and the listener. The uids need no cleanup: they are bare
# numbers (the image pre-creates the whole range in /etc/passwd), no users
# are created.
set -eu

log() { printf 'hostile-probe-vps: %s\n' "$*"; }
die() { printf 'hostile-probe-vps: %s\n' "$*" >&2; exit 1; }
skip() { printf 'hostile-probe-vps: SKIP: %s\n' "$*"; exit 0; }

case "$PROBE_UID" in ''|*[!0-9]*) die "PROBE_UID must be a non-negative integer" ;; esac
case "$SIBLING_UID" in ''|*[!0-9]*) die "SIBLING_UID must be a non-negative integer" ;; esac
[ "$PROBE_UID" != "$SIBLING_UID" ] || die "uids must be distinct"


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
cleanup() {
    if [ -n "$LISTENER_PID" ]; then kill "$LISTENER_PID" 2>/dev/null || true; fi
    if [ -n "$LEAF" ]; then
        echo 1 > "$LEAF/cgroup.kill" 2>/dev/null || true
        rmdir "$LEAF" 2>/dev/null || true
    fi
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
for spec in "memory.max 536870912" "pids.max 512"; do
    # shellcheck disable=SC2086
    set -- $spec
    echo "$2" > "$LEAF/$1" 2>/dev/null || skip "cannot write $LEAF/$1 (no delegation?)"
    [ "$(cat "$LEAF/$1")" = "$2" ] || skip "$1 did not apply (wrote $2, have $(cat "$LEAF/$1"))"
done
log "leaf $LEAF carries memory.max=536870912 pids.max=512"

# The child's explicit env, mirroring sandbox.rs: base vars plus the HOSTILE_*
# fixture pointers. Reserved platform names an app must never set are
# planted, then dropped by the same prefix list as is_reserved_env_name —
# what the env:reserved-dropped probe exercises.
sq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
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
        AWS_*|S3_*|CELLD_*|OUTER_*|CLOUDFLARE_*|R2_*|STRIPE_*|RESEND_*|JEV_*|LD_*|BUN_*)
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
    printf 'echo $$ > %s/cgroup.procs\n' "$(sq "$LEAF/cgroup.procs")"
    printf 'cd %s\n' "$(sq "$WORK")"
    # shellcheck disable=SC2086
    printf 'exec setpriv --reuid %s --regid %s --clear-groups env -i %s %s %s\n' \
        "$PROBE_UID" "$PROBE_UID" "$ASSIGN" "$(sq "$RUNTIME")" "$(sq "$WORK/build-probe.mjs")"
} > "$BASE/runner.sh"
chmod 0755 "$BASE/runner.sh"

log "running build-probe.mjs as uid $PROBE_UID in $LEAF"
if ! timeout 300 sh "$BASE/runner.sh" > "$BASE/stdout.log" 2> "$BASE/stderr.log"; then
    status="$?"
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
