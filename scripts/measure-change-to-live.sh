#!/bin/sh
# Measure commit-to-live latency for a warm app (spec §1.1 goal 2, §19 M1).
#
# Usage: scripts/measure-change-to-live.sh KEY_FILE APP N [BASE_URL]
#   KEY_FILE  file holding the Bearer API key (account owning APP)
#   APP       app slug
#   N         number of trivial commits (each rewrites .measure-ping.txt)
#   BASE_URL  platform origin (default https://outer.now)
#
# Each run posts one MCP `commit` (wait:true) and records wall-clock
# commit->success plus the server's deploy.duration_ms and per-step ms.
# Prints one line per run, then p50/p95 over successful runs.
set -eu

KEY_FILE=${1:?usage: measure-change-to-live.sh KEY_FILE APP N [BASE_URL]}
APP=${2:?usage: measure-change-to-live.sh KEY_FILE APP N [BASE_URL]}
N=${3:?usage: measure-change-to-live.sh KEY_FILE APP N [BASE_URL]}
BASE=${4:-https://outer.now}

KEY=$(tr -d '[:space:]' < "$KEY_FILE")
OUT=$(mktemp)
HDR=$(mktemp)
trap 'rm -f "$OUT" "$HDR"' EXIT INT TERM
# The Bearer key travels in a 0600 header file (`curl -H @file`), never in
# argv where any local user could read it off the process list.
printf 'Authorization: Bearer %s' "$KEY" > "$HDR"
chmod 600 "$HDR"
unset KEY

i=1
while [ "$i" -le "$N" ]; do
    body=$(jq -n --arg app "$APP" --arg msg "measure ping $i" --arg content "measure ping $i" \
        '{jsonrpc:"2.0", id:1, method:"tools/call",
          params:{name:"commit", arguments:{app:$app, message:$msg,
            writes:[{path:".measure-ping.txt", content:$content}], wait:true}}}')
    start=$(date +%s%3N)
    code=$(curl -sS -o "$OUT" -w '%{http_code}' --max-time 120 \
        -H @"$HDR" -H 'Content-Type: application/json' \
        -d "$body" "$BASE/mcp" || echo CURL_FAIL)
    end=$(date +%s%3N)
    wall=$((end - start))
    if [ "$code" = "200" ]; then
        status=$(jq -r '.result.structuredContent.deploy.status // .result.structuredContent.error // "NO_DEPLOY"' "$OUT")
        dur=$(jq -r '.result.structuredContent.deploy.duration_ms // -1' "$OUT")
        steps=$(jq -c '.result.structuredContent.deploy.steps // [] | map("\(.name)=\(.ms)ms" + (if .skipped then "*" else "" end)) | join(" ")' "$OUT")
    else
        status="HTTP_$code"
        dur=-1
        steps=""
    fi
    echo "run=$i wall_ms=$wall status=$status deploy_duration_ms=$dur steps=[$steps]"
    i=$((i + 1))
done | tee "$OUT.runs"
echo "---"
awk '$3 ~ /^status=success$/ {print $2}' "$OUT.runs" | sed 's/wall_ms=//' | python3 -c '
import sys, statistics
xs = sorted(int(l) for l in sys.stdin if l.strip())
n = len(xs)
if not n:
    print("no successful runs")
else:
    p50 = xs[min(n - 1, int(n * 0.50))]
    p95 = xs[min(n - 1, int(n * 0.95))]
    print(f"wall_ms n={n} min={xs[0]} p50={p50} p95={p95} max={xs[-1]}")
'
grep -c 'status=success' "$OUT.runs" | xargs echo "successful runs:" || true
