#!/usr/bin/env bash
#
# Outer VPS provisioning: run once as root on a fresh Ubuntu Server.
#
#   scp -r deploy root@HOST:/root/outer-deploy
#   ssh root@HOST 'bash -s' < deploy/provision.sh
#
# (or pipe it into sudo on the host). Idempotent: re-running finishes
# whatever a previous run left undone. The deploy files must be uploaded
# first — this script reaches the host through stdin, so it cannot bring
# them along. Without them it installs everything file-independent and exits
# nonzero with the upload command.
#
# After this: put the Cloudflare Origin CA files in /srv/outer/tls/
# (origin.pem, origin.key, origin-pull-ca.pem, mode 600), copy
# deploy/outer.env.example to /srv/outer/outer.env and fill it, then run
# deploy/release.sh from the dev box.
#
# No secrets in args or logs.

set -euo pipefail

DEPLOY_SRC="${DEPLOY_SRC:-/root/outer-deploy}"
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

# --- 3. sshd: key-only, no root password login -------------------------------
# The account that will log in afterwards must hold a key: the sudo caller's,
# or root's when run as root directly.
admin_home="$(getent passwd "${SUDO_USER:-root}" | cut -d: -f6)"
[[ -s "$admin_home/.ssh/authorized_keys" ]] || {
    echo "provision.sh: refusing to disable password auth: no authorized_keys for ${SUDO_USER:-root}" >&2
    exit 1
}
# sshd takes the FIRST value it sees: 01- sorts ahead of cloud-init's 50-.
rm -f /etc/ssh/sshd_config.d/60-outer.conf
cat >/etc/ssh/sshd_config.d/01-outer.conf <<'EOF'
PubkeyAuthentication yes
PasswordAuthentication no
ChallengeResponseAuthentication no
PermitRootLogin prohibit-password
EOF
sshd -t # validate before touching the running daemon; reload never kills sessions
if systemctl list-unit-files | grep -q '^ssh\.service'; then
    systemctl reload ssh
else
    systemctl reload sshd
fi

# --- 4. Layout ----------------------------------------------------------------
install -m 0755 -d /srv/outer
install -m 0700 -d /srv/outer/data /srv/outer/tls
chmod 700 /srv/outer/data /srv/outer/tls
for f in /srv/outer/tls/origin.pem /srv/outer/tls/origin.key /srv/outer/tls/origin-pull-ca.pem; do
    [[ -f "$f" ]] && chmod 600 "$f"
done

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

# --- 7. Firewall -----------------------------------------------------------------
# Needs the deploy files on the host (see header). Without them, install
# everything else and say exactly what is missing.
if [[ -f "$DEPLOY_SRC/firewall.sh" ]]; then
    install -m 0755 "$DEPLOY_SRC/firewall.sh" /srv/outer/firewall.sh
    install -m 0644 "$DEPLOY_SRC/firewall.service" /etc/systemd/system/firewall.service
    install -m 0644 "$DEPLOY_SRC/firewall-refresh.service" /etc/systemd/system/firewall-refresh.service
    install -m 0644 "$DEPLOY_SRC/firewall-refresh.timer" /etc/systemd/system/firewall-refresh.timer
    systemctl daemon-reload
    # First install fails closed (fetch failure aborts, old table kept);
    # refreshes keep the old set instead.
    /srv/outer/firewall.sh
    systemctl enable firewall.service
    systemctl enable --now firewall-refresh.timer
else
    if [[ "$FW_TABLE_EXISTS" -eq 1 ]]; then
        echo "provision.sh: WARNING: $DEPLOY_SRC/firewall.sh missing; table inet outer_fw is active but units were not installed" >&2
    else
        echo "provision.sh: WARNING: firewall not installed ($DEPLOY_SRC/firewall.sh missing)" >&2
        echo "  scp -r deploy root@HOST:$DEPLOY_SRC && re-run this script (it is idempotent)" >&2
    fi
    MISSING_FILES=1
fi

# --- 8. Summary --------------------------------------------------------------------
echo "== outer provision summary =="
echo "os:         ${ID} ${VERSION_ID} (${CODENAME})"
echo "docker:     $(docker --version 2>/dev/null || echo MISSING)"
echo "compose:    $(docker compose version 2>/dev/null || echo MISSING)"
echo "cgroup v2:  $(tr ' ' '\n' </sys/fs/cgroup/cgroup.controllers | tr '\n' ' ')"
if [[ "$FW_TABLE_EXISTS" -eq 1 ]] || nft list table inet outer_fw >/dev/null 2>&1; then
    echo "firewall:   table inet outer_fw active"
else
    echo "firewall:   NOT INSTALLED"
fi
echo "sshd:       $(sshd -T 2>/dev/null | grep -Ei '^(passwordauthentication|permitrootlogin|pubkeyauthentication) ' | tr '\n' ' ')"
echo "dirs:       $(stat -c '%a %U %n' /srv/outer /srv/outer/data /srv/outer/tls | tr '\n' '; ')"
echo "timer:      $(systemctl is-enabled firewall-refresh.timer 2>/dev/null || echo not-installed)"

if [[ "${MISSING_FILES:-0}" -eq 1 ]]; then
    exit 1
fi
