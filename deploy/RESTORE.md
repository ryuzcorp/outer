# Host rebuild + state restore runbook (tunnel setup)

Production sits behind a Cloudflare Tunnel: `cloudflared` runs on the host
(not in Docker), every inbound port is closed, SSH rides the tunnel behind a
Cloudflare Access app, and the origin listens on loopback only
(`OUTER_LISTEN=127.0.0.1:8443`, plain HTTP). Rebuilding the host means
recreating that shape, then letting the platform state flow back from the
bucket. The bucket holds every byte of state; the host holds only config and
secrets that are never committed.

Terminology: `<ssh-target>` below is `outer-vps` (workstation `~/.ssh/config`
per `deploy/TUNNEL.md` (c)); `<SHA>` is the prod image tag from
`/srv/outer/release.env` (`OUTER_IMAGE=outer:<SHA>`).

## 0. What must exist before anything breaks

These live only on the workstation and the host — if they are gone, a
rebuild starts with break-glass (netcup VNC, `deploy/TUNNEL.md` (b)) and
re-created secrets:

- Tunnel credentials: the `<tunnel-id>.json` created at cutover (workstation
  backup at `~/.config/outer/`), which lands at
  `/etc/cloudflared/<tunnel-id>.json` (mode 600, owner `cloudflared`).
- Workstation SSH: `~/.config/outer/cf-access.env` (service token, 600) and
  the `outer-vps` host entry (`deploy/TUNNEL.md` (c)).
- A backup of `/srv/outer/outer.env` (mode 600): every production secret.
- A backup of `/srv/outer/legal/`: `terms.html`, `privacy.html`,
  `terms.pl.html`, `privacy.pl.html`, `checkout-notice.txt`. Host-only files,
  never in the repo — there is no other copy.
- A backup of `/srv/outer/tls/review-logins` (the reviewer password file).

## 1. Reach the box

```bash
ssh <ssh-target> true   # tunnel SSH; if this fails, use the netcup VNC console
```

## 2. Provision (fresh host only)

Copy the deploy dir over and run the provisioner. No argument runs
everything: Docker, sshd loopback lockdown, data/tls/legal dirs, nftables
tooling, cgroup check, cloudflared install, closed firewall. On an existing
host this step is a no-op (cloudflared, firewall and sshd are already in
place) — skip it.

```bash
scp -r deploy <ssh-target>:/root/outer-deploy
scp ~/.config/outer/<tunnel-id>.json <ssh-target>:/etc/cloudflared/<tunnel-id>.json
ssh <ssh-target> 'chmod 600 /etc/cloudflared/<tunnel-id>.json && chown cloudflared:cloudflared /etc/cloudflared/<tunnel-id>.json || true'
ssh <ssh-target> 'bash -s' < deploy/provision.sh
```

`provision.sh` enables `cloudflared` only once the credentials file is in
place — if the unit stayed down, re-run the script after copying the JSON.
Order note: installing the firewall before tunnel SSH works costs the box;
on a fresh host the console is the way in until `outer-vps` answers.

## 3. Restore host files

```bash
ssh <ssh-target> 'install -m 0700 -d /srv/outer/data /srv/outer/tls && install -m 0755 -d /srv/outer/legal'
# outer.env (600), the five legal files, and review-logins from backup:
scp outer.env-backup <ssh-target>:/srv/outer/outer.env
scp terms.html privacy.html terms.pl.html privacy.pl.html checkout-notice.txt <ssh-target>:/srv/outer/legal/
scp review-logins-backup <ssh-target>:/srv/outer/tls/review-logins
ssh <ssh-target> 'chmod 600 /srv/outer/outer.env /srv/outer/tls/review-logins'
```

`check-config` (and the release preflight) refuses a missing legal dir or an
over-length checkout notice, so a bad copy fails before anything restarts.

## 4. Roll out and let the state flow back

Pick one path:

**A. Data dir is empty (wiped or fresh disk).** Release with
`--fresh-install`: the flag requires the remote DB to be absent and skips the
migration-checksum gate, which cannot read a database that does not exist.
Boot restores the live snapshot automatically when the volume is empty, then
claims the bucket and resumes snapshots:

```bash
deploy/release.sh <ssh-target> --fresh-install
```

**B. Data dir has a stale or corrupt database.** Stop the container, restore
over it, then release (or `up -d`) normally. The restore refuses while a
server holds the database — the stop below is what releases it:

```bash
ssh <ssh-target> 'docker compose --env-file /srv/outer/release.env -f /srv/outer/compose.yaml stop -t 60 outer'
ssh <ssh-target> 'docker run --rm --pull=never --env-file /srv/outer/outer.env \
  -v /srv/outer/tls:/tls:ro -v /srv/outer/legal:/legal:ro \
  -v /srv/outer/data:/data "outer:<SHA>" state restore --force'
```

`state restore --force` prints what it did as JSON: the snapshot key
restored, where the previous database moved
(`outer.sqlite.pre-restore-<UTC timestamp>-<pid>`, never deleted), the byte
counts, and the sha256. The existing database (plus any `-wal`/`-shm`
sidecars) moves aside first; the downloaded bytes are checksum-verified
before anything is replaced. `--key` restores a history copy instead of the
live snapshot (pick one from `outer state list`); a snapshot whose metadata
records no checksum refuses unless `--no-verify` is also passed (a recorded
checksum that mismatches always refuses).

Every release also files a pre-release copy of the live snapshot under
`outer/state/history/release-<UTC timestamp>-<SHA>` before the handoff, so a
bad migration has a restore point even if nobody filed one by hand.

## 5. Point-in-time restores (history keys)

Restoring an *older* history copy over newer live state is a fork: the next
snapshot upload refuses on the lineage gate (the bucket holds newer bytes
than the local file), which is the tripwire working as intended. To make the
restored database the live one, clear the live keys from the bucket and
restart — the first snapshot then uploads the restored database as the new
live lineage (the generation counter restarts at 1):
```bash
EP="<OUTER_R2_ENDPOINT>"   # same endpoint pattern as deploy/WIPE.md
aws s3 rm --endpoint-url "$EP" s3://outer/outer/state/outer.sqlite.zst
aws s3 rm --endpoint-url "$EP" s3://outer/outer/state/outer.sqlite.meta.json
ssh <ssh-target> 'docker compose --env-file /srv/outer/release.env -f /srv/outer/compose.yaml restart -t 300 outer'
```

## 6. Verify

- `https://outer.now/health` + `/ready` (200 once the fresh DB and bucket answer).
- No lineage refusals in the logs (a refusing uploader trips `/ready` after
  30 consecutive refusals — that is the monitoring signal backups stopped).
- One app page loads; `outer state list` shows the live key and history.

## 7. Drill (scratch data dir, no prod touch)

Rehearse the restore quarterly on a scratch dir. The drill only reads the
bucket (snapshot GETs); it never writes the bucket, never touches
`/srv/outer/data`, and never contends with prod (its lock file lives in the
scratch dir). Run it while prod serves — no stop, no maintenance:

```bash
SHA="$(ssh <ssh-target> 'grep OUTER_IMAGE /srv/outer/release.env | cut -d= -f2')"
ssh <ssh-target> "mkdir -p /srv/outer/drill && docker run --rm --pull=never \
  --env-file /srv/outer/outer.env -e OUTER_DATA=/scratch \
  -v /srv/outer/tls:/tls:ro -v /srv/outer/legal:/legal:ro \
  -v /srv/outer/drill:/scratch \"outer:$SHA\" state list"
ssh <ssh-target> "docker run --rm --pull=never \
  --env-file /srv/outer/outer.env -e OUTER_DATA=/scratch \
  -v /srv/outer/tls:/tls:ro -v /srv/outer/legal:/legal:ro \
  -v /srv/outer/drill:/scratch \"outer:$SHA\" state restore --force"
ssh <ssh-target> 'ls -la /srv/outer/drill && rm -rf /srv/outer/drill'
```

Expected: `state list` prints the live key plus history keys with
timestamps; the restore prints its JSON report with `"backup": null` (the
scratch dir started empty); `ls` shows `outer.sqlite`,
`outer.sqlite.snapshot-sha` and `outer.sqlite.lock`. If `sqlite3` is handy,
`pragma integrity_check;` against the scratch file should read `ok`. Then
delete the scratch dir — the drill leaves nothing behind.
