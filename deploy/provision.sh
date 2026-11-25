#!/usr/bin/env bash
#
# Outer VPS provisioning: run once as root on a fresh Ubuntu Server.
#
#   scp -r deploy root@HOST:/root/outer-deploy
#   ssh root@HOST 'bash -s' < deploy/provision.sh [cloudflared|lockdown]
#
# Phase argument (the tunnel cutover in deploy/TUNNEL.md runs them in order):
# `cloudflared` installs only the tunnel client (repo, package, user,
# config, unit) and starts it — no firewall, no sshd change, reversible;
# `lockdown` applies only the closed firewall and the loopback sshd (run
# last, after tunnel SSH, the release and the DNS flip are verified). No
# argument runs everything: the fresh-host path.
# (The phase travels as an argv, not stdin: `ssh root@HOST 'bash -s --
# <phase>' < deploy/provision.sh`, or set PHASE in the environment.)
#
# After this: create the tunnel via the API and copy its credentials JSON
# to /etc/cloudflared/<tunnel-id>.json (mode 600, owner cloudflared), put
# review-logins in /srv/outer/tls/ (mode 600), copy
# deploy/outer.env.example to /srv/outer/outer.env and fill it, then run
# deploy/release.sh from the dev box. Re-run this script once the
# credentials JSON is in place: it enables cloudflared only then.
#
# No secrets in args or logs.

set -euo pipefail

DEPLOY_SRC="${DEPLOY_SRC:-/root/outer-deploy}"
PHASE="${PHASE:-${1:-all}}"
if [[ "$PHASE" != all && "$PHASE" != cloudflared && "$PHASE" != lockdown ]]; then
    echo "provision.sh: usage: provision.sh [cloudflared|lockdown]" >&2
    exit 2
fi
FW_TABLE_EXISTS=0
if command -v nft >/dev/null 2>&1 && nft list table inet outer_fw >/dev/null 2>&1; then
    FW_TABLE_EXISTS=1
fi

# --- 0. Preconditions -------------------------------------------------------
[[ "$(id -u)" -eq 0 ]] || {
    echo "provision.sh: run as root" >&2
    exit 1
}
# shellcheck disable=SC1091
. /etc/os-release
[[ "${ID:-}" == "ubuntu" || "${ID:-}" == "debian" ]] || {
    echo "provision.sh: needs Ubuntu/Debian with systemd (found ID=${ID:-unknown})" >&2
    exit 1
}

export DEBIAN_FRONTEND=noninteractive
CODENAME="$VERSION_CODENAME"
ARCH="$(dpkg --print-architecture)"

# --- 1. Docker CE + compose plugin from Docker's apt repo --------------------
if [[ ! -f /etc/apt/sources.list.d/docker.list ]]; then
    apt-get update -qq
    apt-get install -y -qq ca-certificates curl gnupg
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL "https://download.docker.com/linux/${ID}/gpg" \
        -o /etc/apt/keyrings/docker.asc
    chmod a+r /etc/apt/keyrings/docker.asc
    echo "deb [arch=${ARCH} signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/${ID} ${CODENAME} stable" \
        >/etc/apt/sources.list.d/docker.list
fi
apt-get update -qq
apt-get install -y -qq docker-ce docker-ce-cli containerd.io \
    docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker

# --- 2. Unattended security upgrades ------------------------------------------
apt-get install -y -qq unattended-upgrades
cat >/etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF

# --- 3. sshd: loopback only, key-only, no forwarding (lockdown phase) --------
if [[ "$PHASE" != "cloudflared" ]]; then
# Access rides the tunnel (cloudflared → 127.0.0.1:22, gated by a Cloudflare
# Access self-hosted app), so sshd listens on loopback and nothing else.
# Ubuntu socket-activates ssh: ssh.socket's ListenStream is generated from
# sshd_config by sshd-socket-generator, so the drop-in below plus
# `daemon-reload && restart ssh.socket` is what makes 127.0.0.1-only real
# (no ssh.service override needed). Reload never kills sessions: the
# operator's pre-cutover connection survives the flip.
# The account that will log in afterwards must hold a key: the sudo caller's,
# or root's when run as root directly.
admin_home="$(getent passwd "${SUDO_USER:-root}" | cut -d: -f6)"
[[ -s "$admin_home/.ssh/authorized_keys" ]] || {
    echo "provision.sh: refusing to harden sshd: no authorized_keys for ${SUDO_USER:-root}" >&2
    exit 1
}
# sshd takes the FIRST value it sees: 01- sorts ahead of cloud-init's 50-.
rm -f /etc/ssh/sshd_config.d/60-outer.conf
cat >/etc/ssh/sshd_config.d/01-outer.conf <<'EOF'
ListenAddress 127.0.0.1
PubkeyAuthentication yes
PasswordAuthentication no
ChallengeResponseAuthentication no
PermitRootLogin prohibit-password
AllowTcpForwarding no
X11Forwarding no
AllowAgentForwarding no
PermitTunnel no
MaxAuthTries 3
LoginGraceTime 20
EOF
sshd -t # validate before touching the running daemon
systemctl daemon-reload # re-run the socket generator against the new ListenAddress
if systemctl list-unit-files | grep -q '^ssh\.socket'; then
    systemctl restart ssh.socket
else
    # No socket activation (older layout): fall back to the service.
    if systemctl list-unit-files | grep -q '^ssh\.service'; then
        systemctl reload ssh
    else
        systemctl reload sshd
    fi
fi
fi # sshd runs in the lockdown phase (and the full run) only

# --- 4. Layout ----------------------------------------------------------------
install -m 0755 -d /srv/outer
install -m 0700 -d /srv/outer/data /srv/outer/tls
chmod 700 /srv/outer/data /srv/outer/tls
# /srv/outer/tls holds review-logins now (the Origin CA files are gone with
# direct ingress); whatever secrets land there stay root-only.
chmod 600 /srv/outer/tls/* 2>/dev/null || true

# --- 5. nftables ---------------------------------------------------------------
apt-get install -y -qq nftables curl
# nftables.service is left disabled on purpose: its /etc/nftables.conf starts
# with `flush ruleset`, which would wipe Docker's tables and outer's egress
# table on any restart. firewall.service applies only inet outer_fw.

# --- 6. cgroup v2 ---------------------------------------------------------------
if [[ ! -e /sys/fs/cgroup/cgroup.controllers ]]; then
    echo "provision.sh: no cgroup v2 controllers at /sys/fs/cgroup (need Ubuntu 24.04+ unified hierarchy)" >&2
    exit 1
fi
# No host-wide vm.overcommit/swap tuning: spec §12.2 bounds each fleet with
# memory.swap.max=0 in-container; nothing host-wide is asked for.

# --- 7. cloudflared (skipped by the lockdown phase) ------------------------------
if [[ "$PHASE" != "lockdown" ]]; then
# From Cloudflare's signed apt repo (https://pkg.cloudflare.com), kept
# current by unattended-upgrades (see the Origins-Pattern drop-in below).
# Runs as the unprivileged `cloudflared` system user, never as root.
if [[ ! -f /etc/apt/sources.list.d/cloudflared.list ]]; then
    apt-get install -y -qq ca-certificates curl gnupg
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg \
        -o /etc/apt/keyrings/cloudflare-main.gpg
    chmod a+r /etc/apt/keyrings/cloudflare-main.gpg
    # `any`, not the release codename: Cloudflare publishes one suite for
    # every Debian/Ubuntu release (there is no resolute/26.04 suite).
    echo "deb [signed-by=/etc/apt/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main" \
        >/etc/apt/sources.list.d/cloudflared.list
fi
apt-get update -qq
apt-get install -y -qq cloudflared
# The repo's Release file carries `Origin: cloudflared`, so this pattern is
# what lets unattended-upgrades take cloudflared with the security updates.
cat >/etc/apt/apt.conf.d/51unattended-cloudflared <<'EOF'
Unattended-Upgrade::Origins-Pattern {
        "o=cloudflared";
};
EOF
if ! id cloudflared >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin cloudflared
fi
# The tunnel credentials JSON is NOT created here: the integrator creates
# the tunnel via the API and copies /etc/cloudflared/<tunnel-id>.json in
# (mode 600, owner cloudflared) beforehand. Without it cloudflared is
# installed but left disabled; re-run this script once it is in place.
if [[ -f "$DEPLOY_SRC/cloudflared/config.yml" && -f "$DEPLOY_SRC/cloudflared/cloudflared.service" ]]; then
    creds=(/etc/cloudflared/*.json)
    if [[ "${#creds[@]}" -eq 1 && -f "${creds[0]}" ]]; then
        TUNNEL_ID="$(basename "${creds[0]}" .json)"
        install -m 0755 -d /etc/cloudflared
        sed -e "s/TUNNEL_ID/${TUNNEL_ID}/g" "$DEPLOY_SRC/cloudflared/config.yml" \
            >/etc/cloudflared/config.yml
        chmod 644 /etc/cloudflared/config.yml
        chown root:cloudflared /etc/cloudflared/config.yml
        chown cloudflared:cloudflared "${creds[0]}"
        chmod 600 "${creds[0]}"
        install -m 0644 "$DEPLOY_SRC/cloudflared/cloudflared.service" /etc/systemd/system/cloudflared.service
        systemctl daemon-reload
        systemctl enable --now cloudflared
    else
        echo "provision.sh: WARNING: cloudflared installed but no tunnel credentials" >&2
        echo "  copy /etc/cloudflared/<tunnel-id>.json onto the host and re-run this script (it is idempotent)" >&2
        MISSING_FILES=1
    fi
else
    echo "provision.sh: WARNING: $DEPLOY_SRC/cloudflared/ missing (config.yml + cloudflared.service)" >&2
    echo "  scp -r deploy root@HOST:$DEPLOY_SRC && re-run this script (it is idempotent)" >&2
    MISSING_FILES=1
fi
fi # cloudflared is installed by its own phase (and the full run) only

# --- 8. Firewall (lockdown phase) --------------------------------------------------
if [[ "$PHASE" != "cloudflared" ]]; then
# Needs the deploy files on the host (see header). Without them, install
# everything else and say exactly what is missing.
if [[ -f "$DEPLOY_SRC/firewall.sh" && -f "$DEPLOY_SRC/firewall.nft" ]]; then
    install -m 0755 "$DEPLOY_SRC/firewall.sh" /srv/outer/firewall.sh
    install -m 0644 "$DEPLOY_SRC/firewall.nft" /srv/outer/firewall.nft
    install -m 0644 "$DEPLOY_SRC/firewall.service" /etc/systemd/system/firewall.service
    # The Cloudflare-range refresh is gone with direct ingress: disable any
    # leftover units from before the tunnel cutover.
    systemctl disable --now firewall-refresh.timer 2>/dev/null || true
    rm -f /etc/systemd/system/firewall-refresh.service /etc/systemd/system/firewall-refresh.timer
    systemctl daemon-reload
    # Static ruleset, applied atomically; --check fails closed first.
    /srv/outer/firewall.sh
    systemctl enable firewall.service
else
    if [[ "$FW_TABLE_EXISTS" -eq 1 ]]; then
        echo "provision.sh: WARNING: $DEPLOY_SRC/firewall.sh missing; table inet outer_fw is active but units were not installed" >&2
    else
        echo "provision.sh: WARNING: firewall not installed ($DEPLOY_SRC/firewall.sh missing)" >&2
        echo "  scp -r deploy root@HOST:$DEPLOY_SRC && re-run this script (it is idempotent)" >&2
    fi
    MISSING_FILES=1
fi
fi # firewall runs in the lockdown phase (and the full run) only

# --- 9. Summary --------------------------------------------------------------------
echo "== outer provision summary =="
echo "phase:      $PHASE"
echo "os:         ${ID} ${VERSION_ID} (${CODENAME})"
echo "docker:     $(docker --version 2>/dev/null || echo MISSING)"
echo "compose:    $(docker compose version 2>/dev/null || echo MISSING)"
echo "cgroup v2:  $(tr ' ' '\n' </sys/fs/cgroup/cgroup.controllers | tr '\n' ' ')"
if [[ "$FW_TABLE_EXISTS" -eq 1 ]] || nft list table inet outer_fw >/dev/null 2>&1; then
    echo "firewall:   table inet outer_fw active"
else
    echo "firewall:   NOT INSTALLED"
fi
echo "sshd:       $(sshd -T 2>/dev/null | grep -Ei '^(listenaddress|passwordauthentication|permitrootlogin|pubkeyauthentication) ' | tr '\n' ' ')"
echo "ssh.sock:   $(systemctl show -p Listen ssh.socket 2>/dev/null || echo no-socket-unit)"
echo "dirs:       $(stat -c '%a %U %n' /srv/outer /srv/outer/data /srv/outer/tls | tr '\n' '; ')"
echo "tunnel:     $(systemctl is-active cloudflared 2>/dev/null || echo not-installed)"

if [[ "${MISSING_FILES:-0}" -eq 1 ]]; then
    exit 1
fi
