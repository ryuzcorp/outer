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
# handoff is open. outer refuses new deploys (retryable, 60 s backoff) and
# delays its background writers while the flag is fresh; a flag older than
# 180 s reads as inactive, so a crashed release heals itself. Recovery from
# a crashed release is `rm /srv/outer/data/handoff` plus `docker rm -f
# outer-next`, then a fresh release.
#
# shellcheck disable=SC2034

HANDOFF_FILE=/srv/outer/data/handoff
HEARTBEAT_PID=""

# Write one heartbeat epoch into the flag file.
handoff_flag() {
    date +%s >"$HANDOFF_FILE"
}

# Open the handoff and keep the heartbeat fresh in the background.
handoff_heartbeat_start() {
    handoff_flag
    (
        while true; do
            sleep 60
            date +%s >"$HANDOFF_FILE"
        done
    ) &
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
    wait_for_listening outer-next "$image" || return 1
    # Drain the old container: SIGTERM refuses new deploys and the process
    # exits once in-flight ones finish. -t 900 bounds a wedged deploy with
    # SIGKILL; the newcomer's handoff catch-up closes whatever is left.
    old="$(docker compose --env-file /srv/outer/release.env \
        -f /srv/outer/compose.yaml ps -q outer)"
    [[ -n "$old" ]] || {
        echo "remote: no outer container to drain" >&2
        return 1
    }
    docker stop -t 900 "$old"
    docker rm "$old"
    record_image "$image"
    handoff_clear
    # Recreate the compose service onto the same image: it overlaps the
    # newcomer via SO_REUSEPORT (same-version, no schema skew), then the
    # newcomer drains and goes away. Steady state stays compose-managed.
    docker compose --env-file /srv/outer/release.env \
        -f /srv/outer/compose.yaml up -d
    wait_for_listening outer-outer-1 "$image" || return 1
    docker stop -t 300 outer-next
    docker rm outer-next
    # Keep the last 3 outer images, prune older (in-use images fail, ignored).
    docker images --format '{{.CreatedAt}} {{.Repository}}:{{.Tag}}' \
        | grep ' outer:' | sort -r | awk '{print $NF}' | tail -n +4 \
        | xargs -r docker rmi >/dev/null 2>&1 || true
    echo "remote: handed off to $image with no downtime"
}
