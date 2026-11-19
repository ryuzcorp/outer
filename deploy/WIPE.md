# Production wipe runbook (Cello 0.2.0 clean break)

Outer has no real users yet: production restarts from a wiped database,
a wiped bucket, and a fresh `outer.env`. There is no rollback after a
wipe — `OUTER_PREVIOUS_IMAGE` points at a build that cannot read the new
DB/snapshot, and the pre-wipe SQLite copy below is kept only until the
new prod has taken its first snapshot + backup cycle.

Do not run any of this until the Cello 0.2.0 image pin is in
(`docker/Dockerfile`: `CELLO_VERSION`/`CELLO_DIGEST`); the box must boot
the renamed build first time up. Nothing here is automated on purpose:
every step is an explicit operator command.

## What gets deleted, what stays

Live state (verified against `config.rs`, `compose.yaml`, `store/*`):

- DB: `/srv/outer/data/outer.sqlite*` (`OUTER_DATA` → `/data` in the
  container, host-mounted at `/srv/outer/data`; `config.rs` joins
  `data_dir` + `"outer.sqlite"`).
- Handoff flag: `/srv/outer/data/handoff` (`handoff.sh` `HANDOFF_FILE`,
  `handoff.rs` `FLAG` under the data dir).
- Local fleet/work state, rebuilt from the bucket: everything else under
  `/srv/outer/data` (fleet dirs `fleets/{slug}`, worktrees, mirrors,
  caches). List first; keep the dir itself.
- Bucket `outer` (`OUTER_R2_BUCKET`, default `outer`): prefixes
  `fleets/` (live app state + `fleets/{slug}--preview/` previews),
  `git/` (repo WAL/bundles), `outer/state/` (snapshot `outer.sqlite.zst`
  + `outer.sqlite.meta.json` + `owner.json` claim + `history/`),
  `outer/backups/` (per-app D1 dumps under `outer/backups/{slug}/`),
  `outer/exports/` (Pro code exports under `outer/exports/{slug}/`).
  No other bucket is used by Outer itself.
- Minted R2 credentials (default TTL 7 d, `OUTER_R2_CREDENTIAL_TTL_S`)
  are orphaned by the wipe; stopping the container kills the fleets that
  hold them, and the fresh boot mints new ones.

Kept untouched:

- `/srv/outer/outer.env` — replaced beforehand with a FRESH copy from
  `deploy/outer.env.example` (fill blanks, `chmod 600`). Outer no longer
  reads any `CELLD_*` or `OUTER_CELLD_BIN` key; a fresh copy keeps the
  file free of dead config.
- `/srv/outer/release.env`, `/srv/outer/tls/` (`origin.pem`,
  `origin.key`, `origin-pull-ca.pem`), the firewall units/table
  (`inet outer_fw`).

## Steps

1. Freeze: announce maintenance; refuse new deploys (set the handoff
   flag or stop the container after step 2). Record image tags:
   `grep -E 'OUTER_(PREVIOUS_)?IMAGE' /srv/outer/release.env`.
2. Safety copy (off-box, optional): 
   `sqlite3 /srv/outer/data/outer.sqlite ".backup '/root/outer-prewipe-$(date +%F).sqlite'"`.
   Keep until the new prod has snapshotted + backed up once.
3. Stop the world. Stop the compose service, and also `outer-next`, which a
   failed earlier handoff can leave running with the data mount and bucket
   access. Stop here if any command fails:
   ```
   docker compose --env-file /srv/outer/release.env -f /srv/outer/compose.yaml stop -t 900 outer
   docker stop -t 900 outer-next 2>/dev/null || true
   docker ps --filter name=outer --format '{{.Names}}'   # must print nothing
   ps aux | grep '[c]ello'                               # must print nothing
   ```
4. Wipe the bucket (`outer`, via the S3-compatible endpoint in
   `OUTER_R2_ENDPOINT`; `EP` = that endpoint, credentials from the
   matching `OUTER_R2_*` keys):
   ```
   for p in fleets/ git/ outer/state/ outer/backups/ outer/exports/; do
     aws s3 rm --recursive --endpoint-url "$EP" "s3://outer/$p"
     aws s3 ls --endpoint-url "$EP" "s3://outer/$p"  # must list nothing
   done
   ```
5. Wipe host state. Everything under `/srv/outer/data` is state or derived
   from it: the DB, the handoff flag, `fleets/`, `work/`, `cache/`, `git/`
   (repo mirrors), `maps/` (tenant sourcemaps), `checks/` (build scratch),
   `examples/`, and `secret-sidecars/` (transient plaintext secret files).
   Empty the directory itself; TLS and env live outside it:
   ```
   find /srv/outer/data -mindepth 1 -delete
   ls -A /srv/outer/data   # must print nothing
   ```

6. Cut the code: from a clean tree at the renamed commit,
   `deploy/release.sh <ssh-target> --fresh-install`. The flag requires
   the remote DB to be absent (refuses if it exists) and skips the
   migration-checksum gate, which cannot read a prod DB that does not
   exist; the rollout then brings the compose service up directly
   instead of a handoff. The preflight `check-config` still validates
   the fresh `outer.env` before anything changes prod.

7. Verify empty:
   - `/health` + `/ready` (200 once the fresh DB and bucket answer; the
     first snapshot writes a fresh `owner.json` claim under `outer/state/`).
   - `docs` tool lists `cello`, not `celld`; `outer check-config` clean.
   - Create one scratch app, deploy, `GET /.well-known/cello/health`
     (synthetic, no wake), stop the fleet, delete the app (exercises the
     `fleets/`, `git/`, `outer/backups/`, `outer/exports/` delete paths),
     confirm the bucket prefixes are empty again; delete the scratch data.
   - Purge the Cloudflare cache for the test host if one was used.
