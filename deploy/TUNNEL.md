# Tunnel cutover runbook (open beta)

Production is served ONLY through a Cloudflare Tunnel: `cloudflared` runs
on the host (not in Docker, so SSH survives a broken container), every
inbound port is closed (no 22, no 443, nothing), and SSH rides the tunnel
behind a Cloudflare Access self-hosted app. The platform origin listens on
loopback only (`OUTER_LISTEN=127.0.0.1:8443`, plain HTTP).

## (a) One-time cutover order

Each step stays reversible until it is verified: direct SSH and direct 443
keep working through step 6, and nothing closes them until the tunnel path,
the release and DNS are all proven. Keep a direct-SSH session open
throughout; only step 7 changes what the network admits, and established
connections survive even that.

1. **Tunnel + cloudflared only.** Create the tunnel via the API
   (`POST /accounts/{id}/cfd_tunnel`, locally-managed, name e.g.
   `outer-prod`), or `cloudflared tunnel create` with a cert as fallback.
   Then, over direct SSH, install just the tunnel client (no firewall, no
   sshd change — fully reversible with
   `systemctl disable --now cloudflared`):
   ```
   ssh root@HOST 'install -m 755 -d /etc/cloudflared'
   scp <tunnel-id>.json root@HOST:/etc/cloudflared/<tunnel-id>.json
   ssh root@HOST 'chmod 600 /etc/cloudflared/<tunnel-id>.json'
   scp -r deploy root@HOST:/root/outer-deploy
   ssh root@HOST 'bash -s -- cloudflared' < deploy/provision.sh
   ```
   (`provision.sh cloudflared` installs the signed repo, the package, the
   unprivileged `cloudflared` user, renders `/etc/cloudflared/config.yml`
   from the credentials filename, and enables + starts `cloudflared`.)
2. **Access + tunnel SSH, verified.** In the dashboard: add a self-hosted
   app for `ssh.outer.now` with a service-token policy, mint the token
   (stored per (c)), and point `ssh.outer.now` at the tunnel (proxied
   CNAME to `<tunnel-id>.cfargotunnel.com`). App hostnames stay on direct
   DNS until step 5. Then, in a NEW terminal with the direct session still
   open, `ssh outer-vps` (ProxyCommand per (c)). If this fails, direct SSH
   still works — fix the tunnel/Access before continuing.
3. **Switch the workstation ssh config.** Make (c) permanent. From here on,
   `<ssh-target>` in every deploy script means `outer-vps`.
4. **Release the tunnel image.** Set `OUTER_LISTEN=127.0.0.1:8443` and
   delete the TLS vars in `/srv/outer/outer.env`, then
   `deploy/release.sh outer-vps` from a clean tree. Cloudflare →
   origin:443 starts failing HERE — the blip begins. Reversible until the
   DNS flip: `deploy/rollback.sh outer-vps` restores the 443 listener.
5. **Flip the app DNS records immediately** (keeps the blip to minutes):
   repoint `outer.now`, `*.outer.now` and `customers.outer.now` to
   proxied CNAMEs `<tunnel-id>.cfargotunnel.com`.
6. **Verify https://outer.now** (health/ready, one app page) from anywhere.
7. **ONLY THEN lock down**, over tunnel SSH:
   ```
   ssh outer-vps 'bash -s -- lockdown' < deploy/provision.sh
   ```
   This applies the closed firewall (INPUT drop: lo/established/ICMP/DHCP
   only; OUTPUT rejects everyone but uid 0 and `cloudflared` on
   127.0.0.1:8443) and moves sshd to `ListenAddress 127.0.0.1` via
   `daemon-reload + restart ssh.socket` (see the sshd note in
   deploy/provision.sh). The pre-cutover direct session survives
   (established connections), but no NEW direct connection is possible
   after this — that is the point.
8. **Final verification** (over `outer-vps`):
   ```
   nft list table inet outer_fw        # == deploy/firewall.nft
   systemctl cat ssh.socket            # ListenStream 127.0.0.1:22 only
   ss -tln | grep -E ':(22|443|8443)'   # 127.0.0.1:22, 127.0.0.1:8443 only
   systemctl is-active cloudflared firewall
   ```

## (b) Break-glass (tunnel or Access down)

Nothing on the box is reachable from the internet by design. Use the
netcup Server Control Panel: the VNC console gives a root shell without
the network, and the rescue system boots a recovery Linux if the host
itself is broken (mount the disk, fix `/etc/ssh/sshd_config.d/` or the
nft ruleset, reboot). Keep netcup credentials + 2FA recovery codes
off-machine; without them there is no third way in.

## (c) Workstation SSH via service token

No per-shell or per-boot steps: the secret lives in one root-only file
that the ProxyCommand itself sources on every connection.

`~/.config/outer/cf-access.env` (`chmod 600`):
```
export TUNNEL_SERVICE_TOKEN_ID=…
export TUNNEL_SERVICE_TOKEN_SECRET=…
```
`~/.ssh/config`:
```
Host outer-vps
  HostName ssh.outer.now
  User root
  ProxyCommand sh -c '. ~/.config/outer/cf-access.env && exec cloudflared access ssh --hostname "$1"' sh %h
```
Nothing secret appears in argv (`ps` shows only the hostname): cloudflared
reads the token from exactly `TUNNEL_SERVICE_TOKEN_ID` and
`TUNNEL_SERVICE_TOKEN_SECRET` — confirmed in cloudflared source
(`cmd/cloudflared/access/cmd.go`: the `access tcp/ssh` subcommand binds
`EnvVars: []string{"TUNNEL_SERVICE_TOKEN_ID"}` to `--service-token-id`
and `EnvVars: []string{"TUNNEL_SERVICE_TOKEN_SECRET"}` to
`--service-token-secret`). Rotate the token in the Access dashboard when
anyone with access leaves; no host change needed.

## (d) Post-cutover cleanup

Only after (a.8) is green:

1. Dashboard → SSL/TLS → Origin Server: turn OFF Authenticated Origin
   Pulls (nothing does mTLS anymore).
2. Dashboard → SSL/TLS → Origin Server → Origin Certificates: revoke the
   Origin CA cert.
3. On the host: `rm /srv/outer/tls/origin.pem /srv/outer/tls/origin.key
   /srv/outer/tls/origin-pull-ca.pem`. Keep `/srv/outer/tls/` itself —
   `review-logins` still lives there, still mode 600, still mounted
   read-only at `/tls`.

This section is the only place that may mention 443, TLS, or origin
pulls — every script, unit, and compose file in deploy/ and docker/
is tunnel-only.
