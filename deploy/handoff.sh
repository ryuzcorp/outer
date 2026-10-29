#!/usr/bin/env bash
#
# Outer zero-downtime handoff helpers. Sourced (not executed) by
# deploy/release.sh and deploy/rollback.sh inside their remote heredocs —
# every function runs on the VPS, where /srv/outer is the state dir.
#
# How a handoff works: the newcomer container binds 443 with SO_REUSEPORT
# while the old one is still serving (the kernel splits new connections),
# the old container drains on SIGTERM (refuses new deploys, exits once
# in-flight ones finish), and the compose service is recreated last onto the
# same image, overlapping the newcomer the same way. At no point is 443
# without a listener, and no in-flight deploy dies with its process.
#
# Coordination is the handoff flag file (/srv/outer/data/handoff): the
# release script writes the heartbeat epoch into it every minute while the
# handoff is open, and holds it until outer-next is stopped. outer refuses
# new deploys (retryable, 60 s backoff) and delays its background writers
# while the flag is fresh; a flag older than 180 s reads as inactive, so a
# crashed release heals itself. Recovery from a crashed release is
# `rm /srv/outer/data/handoff` plus `docker rm -f outer-next`, then a fresh
# release.
#
# shellcheck disable=SC2034

HANDOFF_FILE=/srv/outer/data/handoff
HEARTBEAT_PID=""

# Write one heartbeat epoch into the flag file, atomically: readers must
# never see a truncated file (an empty read deletes the flag and unblocks
# the newcomer mid-handoff).
handoff_flag() {
    date +%s >"$HANDOFF_FILE.tmp" && mv -f "$HANDOFF_FILE.tmp" "$HANDOFF_FILE"
}

# Open the handoff and keep the heartbeat fresh in the background. The
# subshell's stdio is detached so a lingering sleep never holds the ssh
# channel open after the script exits.
handoff_heartbeat_start() {
    handoff_flag
    (
        while true; do
            sleep 60
            handoff_flag
        done
    ) >/dev/null 2>&1 </dev/null &
    HEARTBEAT_PID=$!
}

# Stop the heartbeat; idempotent.
handoff_heartbeat_stop() {
    if [[ -n "$HEARTBEAT_PID" ]]; then
        kill "$HEARTBEAT_PID" 2>/dev/null || true
        wait "$HEARTBEAT_PID" 2>/dev/null || true
    fi
    HEARTBEAT_PID=""
}

# Close the handoff: stop the heartbeat and remove the flag. Idempotent.
handoff_clear() {
    handoff_heartbeat_stop
    rm -f "$HANDOFF_FILE"
}

# Abort a failed handoff: clear the flag, and remove outer-next only while
# a compose-managed outer still serves. Past the old container's removal
# outer-next may be the only listener on 443, so it stays up and the
# operator recovers with a fresh release.
handoff_abort() {
    handoff_clear
    local old
    old="$(docker compose --env-file /srv/outer/release.env \
        -f /srv/outer/compose.yaml ps -q outer 2>/dev/null | head -n 1 || true)"
    if [[ -n "$old" ]] && docker logs "$old" 2>&1 | grep -q 'listening on'; then
        docker rm -f outer-next >/dev/null 2>&1 || true
    else
        echo "remote: handoff aborted with no healthy compose outer; outer-next is left running — recover with a fresh release" >&2
    fi
}

# wait_for_listening <container> <image>: the container is running on
# <image> and outer logged its "listening on" line. mTLS admits no
# unauthenticated HTTP probe, so the log line is the signal (same as the
# old stop-start rollout). 60 s deadline, then failure.
wait_for_listening() {
    local name="$1" image="$2"
    local cid deadline
    deadline=$((SECONDS + 60))
    while true; do
        cid="$(docker ps -q -f "name=^${name}$")"
        if [[ -n "$cid" ]] \
            && [[ "$(docker inspect -f '{{.State.Running}} {{.Config.Image}}' "$cid")" == "true $image" ]] \
            && docker logs "$cid" 2>&1 | grep -q 'listening on'; then
            echo "remote: $name on $image up ($cid)"
            return 0
        fi
        if [[ $SECONDS -ge $deadline ]]; then
            echo "remote: $name did not come up; last log lines:" >&2
            docker logs --tail 50 "$name" 2>&1 || true
            return 1
        fi
        sleep 2
    done
}

# In-flight deploys (non-terminal rows) block nothing by themselves — the
# old container drains them on SIGTERM — but an old container that predates
# the drain still dies with them, so wait for an idle moment first (bounded:
# the drain and the newcomer's catch-up cover whatever starts after).
wait_for_idle_deploys() {
    local deadline count
    deadline=$((SECONDS + 900))
    while true; do
        count="$(sqlite3 /srv/outer/data/outer.sqlite \
            "SELECT COUNT(*) FROM deploy WHERE status NOT IN ('success', 'failed', 'blocked');")"
        if [[ "$count" == "0" ]]; then
            return 0
        fi
        if [[ $SECONDS -ge $deadline ]]; then
            echo "remote: $count deploy(s) still in flight after 15 min; proceeding (the drain covers them)" >&2
            return 0
        fi
        sleep 10
    done
}

# Record the new image, keeping the previous tag for rollback. Same
# semantics as the old stop-start rollout: re-releasing the running image
# keeps the existing rollback target.
record_image() {
    local image="$1"
    local prev prev_rollback tmp
    touch /srv/outer/release.env
    chmod 600 /srv/outer/release.env
    prev="$(grep -E '^OUTER_IMAGE=' /srv/outer/release.env | tail -n 1 | cut -d= -f2- || true)"
    prev_rollback="$(grep -E '^OUTER_PREVIOUS_IMAGE=' /srv/outer/release.env | tail -n 1 | cut -d= -f2- || true)"
    tmp="$(mktemp)"
    grep -v -E '^(OUTER_IMAGE|OUTER_PREVIOUS_IMAGE|OUTER_ROLLED_BACK_FROM)=' \
        /srv/outer/release.env >"$tmp" || true
    {
        cat "$tmp"
        echo "OUTER_IMAGE=$image"
        if [[ -n "$prev" && "$prev" != "$image" ]]; then
            echo "OUTER_PREVIOUS_IMAGE=$prev"
        elif [[ -n "$prev_rollback" ]]; then
            # Re-releasing the running image keeps the existing rollback target.
            echo "OUTER_PREVIOUS_IMAGE=$prev_rollback"
        fi
    } >/srv/outer/release.env
    rm -f "$tmp"
}

# handoff_to <image>: roll <image> out with no 443 blackout.
handoff_to() {
    local image="$1"
    local old
    docker image inspect "$image" >/dev/null 2>&1 || {
        echo "remote: image $image not loaded" >&2
        return 1
    }
    wait_for_idle_deploys
    handoff_heartbeat_start
    docker rm -f outer-next >/dev/null 2>&1 || true
    # Same volumes, env, caps and host network as the compose service
    # (deploy/compose.yaml): the newcomer is the service, started by hand.
    docker run -d --name outer-next --network host \
        --cap-add NET_ADMIN --cap-add SETUID --cap-add SETGID --cap-add CHOWN --cap-add SYS_ADMIN \
        --security-opt apparmor:unconfined \
        --env-file /srv/outer/outer.env \
        -v /srv/outer/data:/data \
        -v /srv/outer/tls:/tls:ro \
        "$image" serve >/dev/null
    old="$(docker compose --env-file /srv/outer/release.env \
        -f /srv/outer/compose.yaml ps -q outer)"
    [[ -n "$old" ]] || {
        echo "remote: no outer container to drain" >&2
        handoff_abort
        return 1
    }
    if ! wait_for_listening outer-next "$image"; then
        # First-handoff fallback: an old container that predates SO_REUSEPORT
        # cannot share 443, so the newcomer bind fails with EADDRINUSE.
        # Stop/start once (seconds of blackout; the idle wait above keeps
        # in-flight deploys off the old build) — every later handoff
        # overlaps.
        if docker logs outer-next 2>&1 | grep -qi "address already in use" \
            && [[ "$(docker inspect -f '{{.State.Running}}' "$old")" == "true" ]]; then
            echo "remote: old container predates SO_REUSEPORT; one-time stop/start fallback" >&2
            docker stop -t 60 "$old"
            docker rm "$old"
            docker start outer-next >/dev/null
            wait_for_listening outer-next "$image" || {
                handoff_abort
                return 1
            }
            old=""
        else
            handoff_abort
            return 1
        fi
    fi
    # Drain the old container: SIGTERM refuses new deploys and the process
    # exits once in-flight ones finish. -t 900 bounds a wedged deploy with
    # SIGKILL; the newcomer's handoff catch-up closes whatever is left.
    # (Skipped on the fallback path above: the old container is already
    # gone.)
    if [[ -n "$old" ]]; then
        docker stop -t 900 "$old" || {
            handoff_abort
            return 1
        }
        docker rm "$old" || {
            handoff_abort
            return 1
        }
    fi
    record_image "$image"
    # Recreate the compose service onto the same image: it overlaps the
    # newcomer via SO_REUSEPORT (same-version, no schema skew), then the
    # newcomer drains and goes away. The flag stays up throughout, so the
    # recreated service skips boot bookkeeping and waits out its writers
    # until outer-next is gone. Steady state stays compose-managed.
    docker compose --env-file /srv/outer/release.env \
        -f /srv/outer/compose.yaml up -d || {
        handoff_abort
        return 1
    }
    wait_for_listening outer-outer-1 "$image" || {
        handoff_abort
        return 1
    }
    docker stop -t 300 outer-next || {
        handoff_abort
        return 1
    }
    docker rm outer-next || {
        handoff_abort
        return 1
    }
    handoff_clear
    # Keep the last 3 outer images, prune older (in-use images fail, ignored).
    docker images --format '{{.CreatedAt}} {{.Repository}}:{{.Tag}}' \
        | grep ' outer:' | sort -r | awk '{print $NF}' | tail -n +4 \
        | xargs -r docker rmi >/dev/null 2>&1 || true
    echo "remote: handed off to $image with no downtime"
}
