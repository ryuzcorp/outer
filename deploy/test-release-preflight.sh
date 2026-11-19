#!/usr/bin/env bash
#
# Fail-before test for deploy/release.sh's preflight gate: stub the ssh and
# engine binaries on PATH, run release.sh against a scratch git repo, and
# assert a failing `check-config` aborts the release before any prod-changing
# step (no scp upload, no rollout heredoc, so no release-backup/handoff).
#
#   bash deploy/test-release-preflight.sh
#
# RELEASE_SH points at the script under test (default: this repo's
# deploy/release.sh); HEAD's copy fails case 1 (it has no preflight).
# STUB_KEEP=1 keeps the scratch dir for inspection.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASE_SH="${RELEASE_SH:-$SCRIPT_DIR/release.sh}"
TMP="$(mktemp -d)"
trap '[[ "${STUB_KEEP:-0}" == 1 ]] || rm -rf "$TMP"' EXIT
BIN="$TMP/bin"
LOG="$TMP/log"
mkdir -p "$BIN" "$LOG"

# --- stubs ---------------------------------------------------------------
# ssh: log every call; fake the prod migration query (no rows); the
# check-config one-off exits $STUB_CHECK_CONFIG_EXIT and the podman retag
# exits $STUB_RETAG_EXIT; everything else exits 0 without executing (the
# rollout heredoc is recorded, never run).
cat >"$BIN/ssh" <<'STUB'
#!/usr/bin/env bash
n=0
while [[ -e "$STUB_LOG/stdin-$n.bin" ]]; do n=$((n + 1)); done
if [ ! -t 0 ]; then cat >"$STUB_LOG/stdin-$n.bin"; else : >"$STUB_LOG/stdin-$n.bin"; fi
printf 'SSH: %s\n' "$*" >>"$STUB_LOG/ssh.log"
case "$*" in
*sqlite3*) exit 0 ;;
*outer.sqlite*) exit "${STUB_DB_EXISTS:-1}" ;;
*check-config*) exit "${STUB_CHECK_CONFIG_EXIT:-0}" ;;
*docker\ tag\ localhost*) exit "${STUB_RETAG_EXIT:-0}" ;;
*) exit 0 ;;
esac
STUB
# scp: log and succeed. Engines: log and succeed (save streams nothing; the
# pipe still exercises the `save | ssh load` shape).
cat >"$BIN/scp" <<'STUB'
#!/usr/bin/env bash
printf 'SCP: %s\n' "$*" >>"$STUB_LOG/scp.log"
STUB
cat >"$BIN/docker" <<'STUB'
#!/usr/bin/env bash
printf 'DOCKER: %s\n' "$*" >>"$STUB_LOG/engine.log"
exit 0
STUB
sed 's/DOCKER:/PODMAN:/' "$BIN/docker" >"$BIN/podman-stub"
chmod +x "$BIN/ssh" "$BIN/scp" "$BIN/docker" "$BIN/podman-stub"

export STUB_LOG="$LOG"
# Hermetic PATH: symlinks to exactly the real tools the test and release.sh
# need, so a workstation podman/docker can never leak in and the cases
# control engine presence by installing stubs.
for tool in bash git sha384sum cut grep sed head mktemp rm mkdir chmod cp cat dirname sort; do
    ln -s "$(command -v "$tool")" "$BIN/$tool"
done
export PATH="$BIN"

# --- scratch repo (release.sh refuses a dirty tree and reads HEAD) --------
FIX="$TMP/fixture"
mkdir -p "$FIX"
git -C "$FIX" init -q
git -C "$FIX" -c user.email=t@t -c user.name=t commit -q --allow-empty -m fixture
fail() {
    echo "test-release-preflight: FAIL: $1" >&2
    sed 's/^/  release-err: /' "$LOG/err.log" >&2 || true
    exit 1
}

# $1: comma-separated stubs to install ("docker", "docker,podman").
reset_case() {
    rm -f "$LOG"/ssh.log "$LOG"/scp.log "$LOG"/engine.log "$LOG"/stdin-*.bin "$LOG"/out.log "$LOG"/err.log
    rm -f "$BIN/podman"
    if [[ "$1" == *podman* ]]; then cp "$BIN/podman-stub" "$BIN/podman"; fi
    chmod +x "$BIN/ssh" "$BIN/scp" "$BIN/docker"
    [[ -f "$BIN/podman" ]] && chmod +x "$BIN/podman" || true
    unset OUTER_ENGINE || true
    export STUB_CHECK_CONFIG_EXIT=0 STUB_RETAG_EXIT=0 STUB_DB_EXISTS=1
}

run_release() {
    (cd "$FIX" && bash "$RELEASE_SH" fake-target </dev/null >"$LOG/out.log" 2>"$LOG/err.log"; echo "$?")
}

run_release_fresh() {
    (cd "$FIX" && bash "$RELEASE_SH" fake-target --fresh-install </dev/null >"$LOG/out.log" 2>"$LOG/err.log"; echo "$?")
}
reset_case "docker,podman"
export STUB_CHECK_CONFIG_EXIT=1
code="$(run_release)"
[[ "$code" != 0 ]] || fail "failing check-config exited 0 (expected abort)"
grep -q "check-config" "$LOG/ssh.log" || fail "preflight check-config never ran"
[[ ! -f "$LOG/scp.log" ]] || fail "scp upload ran after a failed preflight"
if grep -q "bash -s" "$LOG/ssh.log"; then fail "rollout ran after a failed preflight"; fi
grep -q "prod untouched" "$LOG/err.log" || fail "abort message does not say prod is untouched"
echo "case 1 ok: failed check-config aborts before scp/rollout (exit $code)"

# --- case 2: passing check-config proceeds, in order -----------------------
reset_case "docker,podman"
code="$(run_release)"
[[ "$code" == 0 ]] || fail "passing check-config exited $code"
pre="$(grep -n "check-config" "$LOG/ssh.log" | head -n1 | cut -d: -f1 || true)"
roll="$(grep -n "bash -s" "$LOG/ssh.log" | head -n1 | cut -d: -f1 || true)"
[[ -n "$pre" && -n "$roll" && "$pre" -lt "$roll" ]] || fail "preflight did not precede the rollout"
grep "check-config" "$LOG/ssh.log" | grep -q -- "--pull=never" || fail "preflight run lacks --pull=never"
grep -l "handoff_to" "$LOG"/stdin-*.bin >/dev/null || fail "rollout heredoc missing handoff_to"
grep -l "release-backup" "$LOG"/stdin-*.bin >/dev/null || fail "rollout heredoc missing release-backup"
echo "case 2 ok: passing check-config proceeds to upload then rollout (exit 0)"

# --- case 3: invalid engine refuses ----------------------------------------
reset_case "docker"
export OUTER_ENGINE=bogus
code="$(run_release)"
[[ "$code" != 0 ]] || fail "OUTER_ENGINE=bogus exited 0"
grep -q "OUTER_ENGINE=bogus" "$LOG/err.log" || fail "invalid engine message names no value"
[[ ! -f "$LOG/engine.log" ]] || fail "an engine ran despite an invalid OUTER_ENGINE"
echo "case 3 ok: invalid OUTER_ENGINE refuses"

# --- case 4: explicit engine absent from PATH refuses -----------------------
reset_case "docker"
export OUTER_ENGINE=podman
code="$(run_release)"
[[ "$code" != 0 ]] || fail "missing podman engine exited 0"
grep -q "OUTER_ENGINE=podman is not on PATH" "$LOG/err.log" || fail "missing engine message unclear"
echo "case 4 ok: explicit engine absent from PATH refuses"

# --- case 5: auto keeps today's behaviour (podman first, else docker) ------
reset_case "docker,podman"
code="$(run_release)"
[[ "$code" == 0 ]] || fail "auto with both engines exited $code"
head -n1 "$LOG/engine.log" | grep -q "^PODMAN: build" || fail "auto did not prefer podman"
reset_case "docker"
code="$(run_release)"
[[ "$code" == 0 ]] || fail "auto with docker only exited $code"
head -n1 "$LOG/engine.log" | grep -q "^DOCKER: build" || fail "auto did not fall back to docker"
echo "case 5 ok: auto prefers podman, falls back to docker"

# --- case 6: podman images are retagged before the preflight ---------------
# A podman build arrives on the host as localhost/outer:<sha>; release.sh
# must retag it to the plain name in its own ssh step before the preflight's
# `docker run … outer:<sha>` (the rollout heredoc is too late).
reset_case "docker,podman"
code="$(run_release)"
[[ "$code" == 0 ]] || fail "podman pass-through exited $code"
head -n1 "$LOG/engine.log" | grep -q "^PODMAN: build" || fail "case 6 did not build with podman"
tag="$(grep -n "docker tag.*localhost/outer" "$LOG/ssh.log" | head -n1 | cut -d: -f1 || true)"
pre6="$(grep -n "check-config" "$LOG/ssh.log" | head -n1 | cut -d: -f1 || true)"
[[ -n "$tag" && -n "$pre6" && "$tag" -lt "$pre6" ]] || fail "retag did not precede the preflight"
echo "case 6 ok: podman image retagged before check-config (exit 0)"

# --- case 7: a failed podman retag aborts before check-config ---------------
# The retag is required on the podman path (set -e, no swallowing): a stale
# outer:<sha> must never reach the preflight.
reset_case "docker,podman"
export STUB_RETAG_EXIT=1
code="$(run_release)"
[[ "$code" != 0 ]] || fail "failed retag exited 0 (expected abort)"
grep -q "docker tag.*localhost/outer" "$LOG/ssh.log" || fail "retag never ran"
if grep -q "check-config" "$LOG/ssh.log"; then fail "check-config ran after a failed retag"; fi
echo "case 7 ok: failed retag aborts before check-config (exit $code)"

# --- case 8: compose and preflight agree on the outer service ---------------
# The single-service file must keep its identity (a past edit dropped
# `restart`), and the preflight's `-v` set must equal the compose `outer`
# volumes (modes ignored) with every mount read-only, so the two can't drift.
for key in "restart: unless-stopped" "pull_policy: never" "stop_grace_period:"; do
    grep -q "$key" "$SCRIPT_DIR/compose.yaml" || fail "compose.yaml lost '$key'"
done
compose_vols="$(grep -o -- '- /srv/outer/[^ :]*:[^ :]*' "$SCRIPT_DIR/compose.yaml" | sed 's/^- //; s/:\(ro\|rw\)$//' | sort -u)"
preflight_line="$(grep -- 'check-config' "$RELEASE_SH" || true)"
preflight_vols="$(echo "$preflight_line" | grep -o -- '-v /srv/outer/[^ ]*' | sed 's/^-v //; s/:\(ro\|rw\)$//' | sort -u)"
[[ -n "$preflight_vols" ]] || fail "no preflight -v mounts found in $RELEASE_SH"
[[ "$compose_vols" == "$preflight_vols" ]] || fail "compose volumes != preflight mounts: [$compose_vols] vs [$preflight_vols]"
if echo "$preflight_line" | grep -o -- '-v /srv/outer/[^ ]*' | grep -vq ':ro$'; then fail "a preflight mount is not read-only"; fi
echo "case 8 ok: compose and preflight agree on service identity and mounts"
# --- case 9: --fresh-install refuses when the remote DB exists --------------
# A fresh release over live state would orphan it: the DB-absent gate is the
# whole point of the flag, so it runs before any build output matters and
# aborts before scp or the rollout.
reset_case "docker,podman"
export STUB_DB_EXISTS=0
code="$(run_release_fresh)"
[[ "$code" != 0 ]] || fail "fresh-install over an existing DB exited 0 (expected refuse)"
grep -q "outer.sqlite exists" "$LOG/err.log" || fail "fresh-install refusal names no DB path"
[[ ! -f "$LOG/scp.log" ]] || fail "scp upload ran after a refused fresh-install"
if grep -q "bash -s" "$LOG/ssh.log"; then fail "rollout ran after a refused fresh-install"; fi
echo "case 9 ok: --fresh-install refuses when the remote DB exists (exit $code)"

# --- case 10: --fresh-install proceeds on a wiped box, skipping the gate ----
# No migration query runs (there is no DB to read); the preflight still
# runs, uploads still happen, and the rollout brings compose up directly
# instead of a handoff (no old container to drain).
reset_case "docker,podman"
export STUB_DB_EXISTS=1
code="$(run_release_fresh)"
[[ "$code" == 0 ]] || fail "fresh-install on a wiped box exited $code"
if grep -q "sqlite3" "$LOG/ssh.log"; then fail "migration gate ran on --fresh-install"; fi
grep -q "check-config" "$LOG/ssh.log" || fail "preflight check-config never ran on --fresh-install"
[[ -f "$LOG/scp.log" ]] || fail "uploads never ran on --fresh-install"
fresh_rollout="$(grep -l "FRESH_INSTALL" "$LOG"/stdin-*.bin 2>/dev/null || true)"
[[ -n "$fresh_rollout" ]] || fail "rollout heredoc missing on --fresh-install"
fresh_line="$(grep -n 'FRESH_INSTALL" == 1' "$fresh_rollout" | head -n1 | cut -d: -f1 || true)"
up_line="$(grep -n "compose.yaml up -d" "$fresh_rollout" | head -n1 | cut -d: -f1 || true)"
hand_line="$(grep -n 'handoff_to "outer' "$fresh_rollout" | head -n1 | cut -d: -f1 || true)"
[[ -n "$fresh_line" && -n "$up_line" && -n "$hand_line" && "$fresh_line" -lt "$up_line" && "$up_line" -lt "$hand_line" ]] \
    || fail "rollout does not compose-up first on the fresh branch (fresh=$fresh_line up=$up_line handoff=$hand_line)"
# The fresh branch must record the new image before compose reads release.env.
rec_line=""
while IFS=: read -r n _; do
    if (( n > fresh_line )); then rec_line="$n"; break; fi
done < <(grep -n 'record_image "outer:' "$fresh_rollout")
[[ -n "$rec_line" && "$rec_line" -lt "$up_line" ]] \
    || fail "fresh branch starts compose before recording the new image (record=$rec_line up=$up_line)"
echo "case 10 ok: --fresh-install skips the gate and composes up directly (exit 0)"

# --- case 11: an unknown second arg refuses ---------------------------------
reset_case "docker"
code="$(cd "$FIX" && bash "$RELEASE_SH" fake-target --bogus </dev/null >"$LOG/out.log" 2>"$LOG/err.log"; echo "$?")"
[[ "$code" != 0 ]] || fail "unknown flag exited 0 (expected refuse)"
grep -q "usage:" "$LOG/err.log" || fail "unknown flag prints no usage"
[[ ! -f "$LOG/ssh.log" ]] || fail "ssh ran after an unknown flag"
echo "case 11 ok: unknown second arg refuses before any ssh (exit $code)"

echo "test-release-preflight: all cases passed"
