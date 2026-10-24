#!/bin/sh
# M1 acceptance against a live Outer (spec section 19, "Done when").
#
# Usage: scripts/m1-acceptance.sh --base https://outer.now --key-file KEY_FILE --app SLUG [--only CHECKS] [--page PATH]
#   KEY_FILE  file holding the Bearer API key (account owning APP)
#   APP       app slug to exercise (must already exist and be deployed)
#   CHECKS    comma-separated subset of: build,profanity,allowed,visibility,refresh (default: all)
#   PATH      override the page file probed by the build/allowed checks
#
# Checks (each prints PASS/FAIL with evidence; any FAIL exits non-zero):
#   build      syntax-error edit -> deploy.status=failed, failed_step=build,
#              non-empty log tail, previous sha still serves; then revert.
#   profanity  plain / leetspeak / Portuguese / SQL-seed probes -> blocked
#              with path+line findings, previous sha still serves; each probe
#              file is deleted by the next commit (a blocked commit IS on
#              main, spec section 15, so the text must be removed after).
#   allowed    "Damn, the cock crowed at dawn" in a page deploys and is served.
#   visibility unlisted -> X-Robots-Tag: noindex + disallowing robots.txt;
#              public -> neither; original visibility restored. No commits.
#   refresh    INFO only: refresh-token reuse is covered by
#              crates/outer/src/auth/tests.rs full_authorize_flow_mints_a_working_token.
#
# Probe words stay obfuscated (base64, decoded at runtime) so this repo
# itself is never flagged by tooling. The Bearer key travels in a 0600
# header file (`curl -H @file`), never in argv.
#
# Commit budget: build=2, profanity=8, allowed=2, visibility=0. Total 12
# against the Free daily quota of 50. whoami usage before/after is printed.
set -eu

BASE="https://outer.now"
KEY_FILE=""
APP=""
ONLY="build,profanity,allowed,visibility,refresh"
PAGE_OVERRIDE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --base) BASE=${2:?--base needs a value}; shift 2 ;;
        --key-file) KEY_FILE=${2:?--key-file needs a value}; shift 2 ;;
        --app) APP=${2:?--app needs a value}; shift 2 ;;
        --only) ONLY=${2:?--only needs a value}; shift 2 ;;
        --page) PAGE_OVERRIDE=${2:?--page needs a value}; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$KEY_FILE" ] && [ -n "$APP" ] || { echo "usage: $0 --base URL --key-file KEY_FILE --app SLUG [--only CHECKS] [--page PATH]" >&2; exit 2; }

command -v curl >/dev/null || { echo "need curl" >&2; exit 2; }
command -v python3 >/dev/null || { echo "need python3" >&2; exit 2; }

# --- probe strings, base64 so the repo itself stays clean (decoded at runtime) ---
B64_PLAIN="dGhpcyBsaXN0IGlzIGZ1Y2tpbmcgYXdlc29tZQ=="
B64_LEET="dGhpcyBsaXN0IGlzIGZ1Y2sxbmcgNHdlc29tZQ=="
B64_PT="ZXNzYSBsaXN0YSDDqSBkbyBjYXJhbGhv"
B64_SQL="SU5TRVJUIElOVE8gaXRlbXMgVkFMVUVTICgnZiB1IGMgaycpOw=="
B64_ALLOWED="RGFtbiwgdGhlIGNvY2sgY3Jvd2VkIGF0IGRhd24="
b64d() { printf '%s' "$1" | base64 -d; }

KEY=$(tr -d '[:space:]' < "$KEY_FILE")
OUT=$(mktemp); HDR=$(mktemp); ARGS=$(mktemp); RESP=$(mktemp)
RESULTS=$(mktemp); BOD1=$(mktemp); BOD2=$(mktemp)
chmod 600 "$HDR"
printf 'Authorization: Bearer %s' "$KEY" > "$HDR"
unset KEY
ORIG_VIS=""; VIS_CHANGED=0
PASS_N=0; FAIL_N=0; COMMITS_N=0

cleanup() {
    if [ "$VIS_CHANGED" = "1" ] && [ -n "$ORIG_VIS" ]; then
        mcp_set_visibility "$APP" "$ORIG_VIS" >/dev/null 2>&1 || true
        VIS_CHANGED=0
    fi
    rm -f "$OUT" "$HDR" "$ARGS" "$RESP" "$RESULTS" "$BOD1" "$BOD2"
}
trap cleanup EXIT INT TERM

pass() { PASS_N=$((PASS_N + 1)); printf 'PASS  %-28s %s\n' "$1" "$2" | tee -a "$RESULTS"; }
fail() { FAIL_N=$((FAIL_N + 1)); printf 'FAIL  %-28s %s\n' "$1" "$2" | tee -a "$RESULTS"; }
info() { printf 'INFO  %-28s %s\n' "$1" "$2" | tee -a "$RESULTS"; }

# mcp_call <tool> : $ARGS holds the arguments JSON object; response JSON -> $RESP; echoes HTTP code.
mcp_call() {
    tool=$1
    python3 - "$tool" "$ARGS" >"$OUT" <<'EOF'
import json, sys
args = json.load(open(sys.argv[2]))
print(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                  "params": {"name": sys.argv[1], "arguments": args}}))
EOF
    curl -sS -o "$RESP" -w '%{http_code}' --max-time 150 \
        -H @"$HDR" -H 'Content-Type: application/json' \
        -d @"$OUT" "$BASE/mcp" || echo CURL_FAIL
}
# jget <respfile> <python expr on `v`> : prints the value (raw for str/int, JSON otherwise).
jget() {
    M1_F=$1 M1_X=$2 python3 <<'EOF'
import json, os
v = json.load(open(os.environ["M1_F"]))
x = eval(os.environ["M1_X"])
print(x if isinstance(x, str) else json.dumps(x))
EOF
}
jok() { # jok <respfile> : 1 when the MCP call returned structuredContent without isError
    M1_F=$1 python3 <<'EOF'
import json, os
v = json.load(open(os.environ["M1_F"]))
sc = v.get("result", {}).get("structuredContent")
print("1" if (sc is not None and not v.get("result", {}).get("isError")) else "0")
EOF
}

mcp_set_visibility() {
    M1_APP=$1 M1_VIS=$2 python3 >"$ARGS" <<'EOF'
import json, os
print(json.dumps({"app": os.environ["M1_APP"], "visibility": os.environ["M1_VIS"]}))
EOF
    mcp_call "set_visibility" >/dev/null
}

commit_edits() { # app message path old new
    M1_APP=$1 M1_MSG=$2 M1_P=$3 M1_O=$4 M1_N=$5 python3 >"$ARGS" <<'EOF'
import json, os
e = os.environ
print(json.dumps({"app": e["M1_APP"], "message": e["M1_MSG"], "wait": True,
                  "edits": [{"path": e["M1_P"], "old": e["M1_O"], "new": e["M1_N"]}]}))
EOF
    code=$(mcp_call "commit")
    echo "$code"
}
commit_write() { # app message path content
    M1_APP=$1 M1_MSG=$2 M1_P=$3 M1_C=$4 python3 >"$ARGS" <<'EOF'
import json, os
e = os.environ
print(json.dumps({"app": e["M1_APP"], "message": e["M1_MSG"], "wait": True,
                  "writes": [{"path": e["M1_P"], "content": e["M1_C"]}]}))
EOF
    code=$(mcp_call "commit")
    echo "$code"
}
commit_delete() { # app message path
    M1_APP=$1 M1_MSG=$2 M1_P=$3 python3 >"$ARGS" <<'EOF'
import json, os
e = os.environ
print(json.dumps({"app": e["M1_APP"], "message": e["M1_MSG"], "wait": True,
                  "deletes": [e["M1_P"]]}))
EOF
    code=$(mcp_call "commit")
    echo "$code"
}

# deploy_of : prints status, failed_step, log_tail_len, findings JSON, sha,
# url on separate lines for the last commit response.
deploy_of() {
    M1_F=$1 python3 <<'EOF'
import json, os
d = json.load(open(os.environ["M1_F"]))["result"]["structuredContent"]["deploy"]
print(d.get("status", "") or "")
print(d.get("failed_step", "") or "")
print(len(d.get("log_tail", "") or ""))
print(json.dumps(d.get("findings", [])))
print(d.get("sha", "") or "")
print(d.get("url", "") or "")
EOF
}
dep_line() { printf '%s' "$2" | sed -n "${1}p"; }
# settle_deploy : $DEPLOY holds deploy_of output; if still building, poll to
# a terminal state. Sets $ST (and refreshes $DEPLOY/$RESP on poll).
settle_deploy() {
    ST=$(dep_line 1 "$DEPLOY")
    if [ "$ST" = "building" ]; then
        await_terminal "$APP" >/dev/null; cp "$OUT.deploy" "$RESP" 2>/dev/null || true
        DEPLOY=$(deploy_of "$RESP"); ST=$(dep_line 1 "$DEPLOY")
    fi
}

# await_terminal <app> : commit waits server-side, but a 50 s tool wait can
# return status=building; poll deploy_status (latest) until terminal.
await_terminal() {
    i=0
    while [ "$i" -lt 8 ]; do
        M1_APP=$1 python3 >"$ARGS" <<'EOF'
import json, os
print(json.dumps({"app": os.environ["M1_APP"], "wait": True}))
EOF
        code=$(mcp_call "deploy_status")
        if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
            st=$(jget "$RESP" 'v["result"]["structuredContent"].get("deploy", {}).get("status", "")')
            case "$st" in
                success|failed|blocked) cp "$RESP" "$OUT.deploy"; echo "$st"; return 0 ;;
            esac
        fi
        i=$((i + 1))
    done
    echo "poll-timeout"
}

want() { case ",$ONLY," in *,"$1,"*) return 0 ;; *) return 1 ;; esac; }


# --- baseline: whoami quota + app identity ---
M1_DUMMY=x python3 >"$ARGS" <<'EOF'
print("{}")
EOF
code=$(mcp_call "whoami")
[ "$code" = "200" ] || { fail "setup" "whoami HTTP $code"; exit 1; }
USED_BEFORE=$(jget "$RESP" 'v["result"]["structuredContent"]["usage"]["code_changes_today"]')
QUOTA_LIMIT=$(jget "$RESP" 'v["result"]["structuredContent"]["limits"]["code_changes_per_day"]')
info "quota" "code_changes_today=$USED_BEFORE limit=$QUOTA_LIMIT (before run)"

M1_APP=$APP python3 >"$ARGS" <<'EOF'
import json, os
print(json.dumps({"app": os.environ["M1_APP"]}))
EOF
code=$(mcp_call "get_app")
[ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ] || { fail "setup" "get_app HTTP $code"; exit 1; }
APP_URL=$(jget "$RESP" 'v["result"]["structuredContent"]["url"]')
ORIG_VIS=$(jget "$RESP" 'v["result"]["structuredContent"]["visibility"]')
BASE_SHA=$(jget "$RESP" 'v["result"]["structuredContent"].get("last_deploy", {}).get("sha", "")')
if [ -z "$APP_URL" ] || [ "$APP_URL" = "None" ]; then
    HOST=$(printf '%s' "$BASE" | python3 -c 'import sys, urllib.parse; print(urllib.parse.urlsplit(sys.stdin.read()).hostname)')
    APP_URL="https://$APP.$HOST"
fi
info "setup" "app=$APP url=$APP_URL visibility=$ORIG_VIS last_deploy_sha=$BASE_SHA"
curl -sS --max-time 60 -o "$BOD1" "$APP_URL" || { fail "setup" "could not fetch $APP_URL"; exit 1; }
BASE_HASH=$(sha256sum "$BOD1" | cut -d' ' -f1)
info "setup" "baseline body sha256=$BASE_HASH"

pick_page() { # echoes the page path to probe
    if [ -n "$PAGE_OVERRIDE" ]; then echo "$PAGE_OVERRIDE"; return 0; fi
    M1_APP=$APP python3 >"$ARGS" <<'EOF'
import json, os
print(json.dumps({"app": os.environ["M1_APP"]}))
EOF
    code=$(mcp_call "list_files")
    [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ] || { echo ""; return 1; }
    jget "$RESP" '(([(f["path"]) for f in v["result"]["structuredContent"]["files"] if f["path"] == "pages/index.tsx"] + [f["path"] for f in v["result"]["structuredContent"]["files"] if f["path"].endswith(".tsx")] + [f["path"] for f in v["result"]["structuredContent"]["files"] if f["path"].endswith((".ts", ".jsx", ".js", ".mjs"))]) or ["None"])[0]'
}
read_page() { # $1 path -> prints file text to stdout
    M1_APP=$APP M1_P=$1 python3 >"$ARGS" <<'EOF'
import json, os
print(json.dumps({"app": os.environ["M1_APP"], "path": os.environ["M1_P"]}))
EOF
    code=$(mcp_call "read_file")
    [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ] || return 1
    jget "$RESP" 'v["result"]["structuredContent"]["text"]'
}

# ================= check 1: failing build =================
if want build; then
    PAGE=$(pick_page) || PAGE=""
    if [ -z "$PAGE" ] || [ "$PAGE" = "None" ]; then
        fail "build" "no .tsx page found to break"
    else
        PTEXT=$(read_page "$PAGE") || PTEXT=""
        ANCHOR=$(M1_T=$PTEXT python3 <<'EOF'
import os
seen, dup = {}, set()
for ln in os.environ["M1_T"].splitlines():
    if ln.strip():
        (dup.add(ln) if ln in seen else seen.__setitem__(ln, 1))
for ln in os.environ["M1_T"].splitlines():
    if ln.strip() and ln not in dup:
        print(ln); break
EOF
)
        if [ -z "$ANCHOR" ]; then
            fail "build" "no unique anchor line in $PAGE"
        else
            BROKEN="{{{M1-SYNTAX-ERROR {{{"
            BROKE_OK=0
            code=$(commit_edits "$APP" "M1: break build (reverted next)" "$PAGE" "$ANCHOR" "$BROKEN")
            if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
                BROKE_OK=1
                COMMITS_N=$((COMMITS_N + 1))
                DEPLOY=$(deploy_of "$RESP")
                ST=$(dep_line 1 "$DEPLOY")
                if [ "$ST" = "building" ]; then
                    await_terminal "$APP" >/dev/null; cp "$OUT.deploy" "$RESP" 2>/dev/null || true
                    DEPLOY=$(deploy_of "$RESP"); ST=$(dep_line 1 "$DEPLOY")
                fi
                FS=$(dep_line 2 "$DEPLOY"); LT=$(dep_line 3 "$DEPLOY")
                if [ "$ST" = "failed" ] && [ "$FS" = "build" ] && [ "$LT" -gt 0 ]; then
                    pass "build:failed" "status=failed failed_step=$FS log_tail_bytes=$LT"
                else
                    fail "build:failed" "status=$ST failed_step=$FS log_tail_bytes=$LT (want failed/build/non-empty)"
                fi
                curl -sS --max-time 60 -o "$BOD2" "$APP_URL" && H2=$(sha256sum "$BOD2" | cut -d' ' -f1) || H2="fetch-failed"
                if [ "$H2" = "$BASE_HASH" ]; then
                    pass "build:serves-previous" "app body sha256 unchanged ($H2)"
                else
                    fail "build:serves-previous" "body changed: $BASE_HASH -> $H2"
                fi
            else
                fail "build:failed" "commit HTTP $code"
            fi
            if [ "$BROKE_OK" = "1" ]; then
            code=$(commit_edits "$APP" "M1: revert broken build" "$PAGE" "$BROKEN" "$ANCHOR")
            if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
                COMMITS_N=$((COMMITS_N + 1))
                DEPLOY=$(deploy_of "$RESP")
                ST=$(dep_line 1 "$DEPLOY")
                if [ "$ST" = "building" ]; then
                    await_terminal "$APP" >/dev/null; cp "$OUT.deploy" "$RESP" 2>/dev/null || true
                    DEPLOY=$(deploy_of "$RESP"); ST=$(dep_line 1 "$DEPLOY")
                fi
                [ "$ST" = "success" ] && pass "build:revert" "revert deployed ($ST)" || fail "build:revert" "revert status=$ST"
            else
                fail "build:revert" "commit HTTP $code"
            fi
            fi
        fi
    fi
fi

# ================= check 2: profanity blocked x4 =================
if want profanity; then
    # kind|base64|probe-path, fed via heredoc so the loop runs in this shell
    # and PASS/FAIL/COMMITS counters survive. The sql probe is a seed-file
    # string literal; the rest are plain content files.
    while IFS='|' read -r KIND B64 PPATH; do
        [ -n "$KIND" ] || continue
        WORD=$(b64d "$B64")
        if [ "$KIND" = "sql" ]; then CONTENT="$WORD
"; else CONTENT="probe $KIND: $WORD
"; fi
        code=$(commit_write "$APP" "M1: profanity probe $KIND (removed next)" "$PPATH" "$CONTENT")
        if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
            COMMITS_N=$((COMMITS_N + 1))
            DEPLOY=$(deploy_of "$RESP")
            settle_deploy
            FJ=$(dep_line 4 "$DEPLOY")
            FHIT=$(M1_FJ=$FJ M1_P=$PPATH python3 <<'EOF'
import json, os
fs = json.loads(os.environ["M1_FJ"])
print("1" if any(isinstance(f, dict) and f.get("path") == os.environ["M1_P"]
                 and isinstance(f.get("line"), int) for f in (fs or [])) else "0")
EOF
)
            if [ "$ST" = "blocked" ] && [ "$FHIT" = "1" ]; then
                printf 'PASS  %-28s %s\n' "profanity:$KIND" "blocked, findings name $PPATH + line" | tee -a "$RESULTS"
            else
                printf 'FAIL  %-28s %s\n' "profanity:$KIND" "status=$ST findings=$FJ" | tee -a "$RESULTS"
            fi
            curl -sS --max-time 60 -o "$BOD2" "$APP_URL" && H2=$(sha256sum "$BOD2" | cut -d' ' -f1) || H2="fetch-failed"
            if [ "$H2" = "$BASE_HASH" ]; then
                printf 'PASS  %-28s %s\n' "profanity:$KIND:serves" "previous sha still serves" | tee -a "$RESULTS"
            else
                printf 'FAIL  %-28s %s\n' "profanity:$KIND:serves" "body changed: $BASE_HASH -> $H2" | tee -a "$RESULTS"
            fi
        else
            printf 'FAIL  %-28s %s\n' "profanity:$KIND" "commit HTTP $code" | tee -a "$RESULTS"
        fi
        # The blocked text is ON main: the next commit must remove it.
        code=$(commit_delete "$APP" "M1: remove profanity probe $KIND" "$PPATH")
        if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
            COMMITS_N=$((COMMITS_N + 1))
            DEPLOY=$(deploy_of "$RESP")
            settle_deploy
            if [ "$ST" = "success" ]; then
                printf 'PASS  %-28s %s\n' "profanity:$KIND:removed" "removal commit deploys" | tee -a "$RESULTS"
            else
                printf 'FAIL  %-28s %s\n' "profanity:$KIND:removed" "removal status=$ST" | tee -a "$RESULTS"
            fi
        else
            printf 'FAIL  %-28s %s\n' "profanity:$KIND:removed" "commit HTTP $code" | tee -a "$RESULTS"
        fi
    done <<PROBES_EOF
plain|$B64_PLAIN|m1-probe-plain.txt
leet|$B64_LEET|m1-probe-leet.txt
portuguese|$B64_PT|m1-probe-pt.txt
sql|$B64_SQL|db/seed.sql
PROBES_EOF
fi

# ================= check 3: allowed sentence deploys =================
if want allowed; then
    PAGE=${PAGE:-$(pick_page)} || PAGE=""
    if [ -z "$PAGE" ] || [ "$PAGE" = "None" ]; then
        fail "allowed" "no page found"
    else
        PTEXT=$(read_page "$PAGE") || PTEXT=""
        ALLOWED=$(b64d "$B64_ALLOWED")
        ANCHOR2=$(M1_T=$PTEXT python3 <<'EOF'
import os, re
pat = re.compile(r'>[^<>{}]*[A-Za-z]{2,}[^<>{}]*<')
seen, dup = {}, set()
for ln in os.environ["M1_T"].splitlines():
    if ln.strip():
        (dup.add(ln) if ln in seen else seen.__setitem__(ln, 1))
for ln in os.environ["M1_T"].splitlines():
    if ln.strip() and ln not in dup and pat.search(ln):
        print(ln); break
EOF
)
        if [ -z "$ANCHOR2" ]; then
            fail "allowed" "no unique JSX-text line in $PAGE to extend"
        else
            WITH=$(M1_A=$ANCHOR2 M1_S=$ALLOWED python3 <<'EOF'
import os
a, s = os.environ["M1_A"], os.environ["M1_S"]
i = a.rfind("<")
print(a[:i] + " " + s + a[i:] if i > 0 else "")
EOF
)
            if [ -z "$WITH" ]; then
                fail "allowed" "could not splice sentence into anchor"
            else
                code=$(commit_edits "$APP" "M1: allowed sentence (reverted next)" "$PAGE" "$ANCHOR2" "$WITH")
                if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
                    COMMITS_N=$((COMMITS_N + 1))
                    DEPLOY=$(deploy_of "$RESP")
                    settle_deploy
                    if [ "$ST" = "success" ]; then
                        pass "allowed:deploy" "status=success"
                        sleep 2
                        if curl -sS --max-time 60 "$APP_URL" | grep -q "cock crowed at dawn"; then
                            pass "allowed:served" "sentence present in $APP_URL"
                        else
                            fail "allowed:served" "sentence missing from $APP_URL"
                        fi
                    else
                        fail "allowed:deploy" "status=$ST findings=$(dep_line 4 "$DEPLOY")"
                    fi
                else
                    fail "allowed:deploy" "commit HTTP $code"
                fi
                code=$(commit_edits "$APP" "M1: revert allowed sentence" "$PAGE" "$WITH" "$ANCHOR2")
                if [ "$code" = "200" ] && [ "$(jok "$RESP")" = "1" ]; then
                    COMMITS_N=$((COMMITS_N + 1))
                    DEPLOY=$(deploy_of "$RESP")
                    settle_deploy
                    [ "$ST" = "success" ] && pass "allowed:revert" "revert deployed" || fail "allowed:revert" "revert status=$ST"
                else
                    fail "allowed:revert" "commit HTTP $code"
                fi
            fi
        fi
    fi
fi

# ================= check 4: visibility (no commits) =================
if want visibility; then
    mcp_set_visibility "$APP" "unlisted" >/dev/null
    VIS_CHANGED=1
    code=$(mcp_call "get_app"); V=$(jget "$RESP" 'v["result"]["structuredContent"]["visibility"]')
    [ "$V" = "unlisted" ] || fail "visibility:unlisted-set" "get_app visibility=$V, want unlisted"
    HDRS=$(curl -sS --max-time 60 -D - -o /dev/null "$APP_URL")
    RT=$(curl -sS --max-time 60 "$APP_URL/robots.txt")
    if printf '%s' "$HDRS" | grep -qi '^x-robots-tag:.*noindex'; then
        pass "visibility:unlisted-header" "X-Robots-Tag noindex present"
    else
        fail "visibility:unlisted-header" "no noindex header; got: $(printf '%s' "$HDRS" | grep -i '^x-robots-tag' || echo none)"
    fi
    if printf '%s' "$RT" | grep -q 'Disallow: /'; then
        pass "visibility:unlisted-robots" "robots.txt disallows"
    else
        fail "visibility:unlisted-robots" "robots.txt: $(printf '%s' "$RT" | head -c 200)"
    fi
    mcp_set_visibility "$APP" "public" >/dev/null
    code=$(mcp_call "get_app"); V=$(jget "$RESP" 'v["result"]["structuredContent"]["visibility"]')
    [ "$V" = "public" ] || fail "visibility:public-set" "get_app visibility=$V, want public"
    HDRS=$(curl -sS --max-time 60 -D - -o /dev/null "$APP_URL")
    RT=$(curl -sS --max-time 60 "$APP_URL/robots.txt")
    if printf '%s' "$HDRS" | grep -qi '^x-robots-tag:'; then
        fail "visibility:public-header" "X-Robots-Tag still present: $(printf '%s' "$HDRS" | grep -i '^x-robots-tag')"
    else
        pass "visibility:public-header" "no X-Robots-Tag"
    fi
    if printf '%s' "$RT" | grep -q 'Disallow: /'; then
        # Distinguish a gate bug from a stale edge cache: a cache-busted URL
        # misses the edge cache and shows what the origin serves now.
        # A public->unlisted->public flip without a host cache purge (spec
        # 4.5 purges on hold/suspend/pause/expire/delete, not on visibility
        # change) keeps serving the cached override until its TTL expires.
        CB_RT=$(curl -sS --max-time 60 -D /tmp/m1cb.hdr "$APP_URL/robots.txt?m1cb=$$-$(date +%s%N)" 2>/dev/null || echo FETCH_FAIL)
        CB_CS=$(grep -i '^cf-cache-status' /tmp/m1cb.hdr 2>/dev/null || echo "cf-cache-status: unknown")
        if printf '%s' "$CB_RT" | grep -q 'Disallow: /'; then
            fail "visibility:public-robots" "robots.txt still disallows: $(printf '%s' "$RT" | head -c 200)"
        else
            fail "visibility:public-robots" "edge serves STALE unlisted override ($CB_CS; origin already passes through) — set_visibility purges no host cache"
        fi
        rm -f /tmp/m1cb.hdr
    else
        pass "visibility:public-robots" "robots.txt passes through to the app"
    fi
    if [ "$ORIG_VIS" != "unlisted" ] && [ "$ORIG_VIS" != "public" ]; then
        fail "visibility:restore" "unexpected original visibility: $ORIG_VIS"
    else
        mcp_set_visibility "$APP" "$ORIG_VIS" >/dev/null
        VIS_CHANGED=0
        code=$(mcp_call "get_app"); V2=$(jget "$RESP" 'v["result"]["structuredContent"]["visibility"]')
        [ "$V2" = "$ORIG_VIS" ] && pass "visibility:restore" "restored $ORIG_VIS" || fail "visibility:restore" "got $V2, want $ORIG_VIS"
    fi
fi

# ================= check 5: refresh reuse (live grant unavailable here) =================
if want refresh; then
    info "refresh-reuse" "covered by crates/outer/src/auth/tests.rs full_authorize_flow_mints_a_working_token (reuse of a rotated refresh token answers invalid_grant and revokes the grant)"
fi

# --- quota used + summary ---
M1_DUMMY=x python3 >"$ARGS" <<'EOF'
print("{}")
EOF
if [ "$(mcp_call "whoami")" = "200" ]; then
    USED_AFTER=$(jget "$RESP" 'v["result"]["structuredContent"]["usage"]["code_changes_today"]')
else
    USED_AFTER="unknown"
fi
echo "---"
echo "commits issued by this run: $COMMITS_N (budget: build=2 profanity=8 allowed=2 visibility=0)"
echo "quota: code_changes_today before=$USED_BEFORE after=$USED_AFTER limit=$QUOTA_LIMIT"
echo "--- summary ---"
cat "$RESULTS"
REAL_FAIL=$(grep -c '^FAIL' "$RESULTS" || true)
REAL_PASS=$(grep -c '^PASS' "$RESULTS" || true)
echo "passing=$REAL_PASS failing=$REAL_FAIL"
[ "$REAL_FAIL" -eq 0 ]
