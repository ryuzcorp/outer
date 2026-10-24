//! D1 migrations for the deploy pipeline (spec §10.1 steps 6-7).
//!
//! After the gate passes, the pipeline applies pending
//! `NNNN_description.sql` files with `celld d1 migrations apply` under the
//! app's scoped credentials. For an app with a live version the apply runs
//! BEFORE the `celld deploy` upload: celld offers no staged upload (the
//! upload publishes `deploy/current.json` and every fleet adopts it on its
//! own poll), so ordering is the only gate — when the new pointer becomes
//! visible the schema is already ready, and a migrate failure uploads
//! nothing, so no fleet can ever serve new code against an unmigrated
//! schema. For a first deploy the order reverses: the apply needs a running
//! node (found through the bucket's node leases) and nothing is published
//! yet, so the upload goes first, the fleet wakes, and the migrations apply
//! after — and only a migrate success marks the app live. The proxy refuses
//! to forward for an app with no successful deploy, so the uploaded-but-
//! unmigrated code is never served. celld reads the files relative to the
//! deploy root, orders them by numeric prefix, and records applied
//! files by name in the app's own `d1_migrations` table (the same table and
//! columns `wrangler d1 migrations` uses, so the two never re-apply each
//! other's files). Outer additionally pins the sha256 it saw in
//! `d1_migration`, so a file edited after it was applied fails the deploy
//! instead of silently drifting from what the database runs.
//!
//! Security model: everything celld reads or Outer copies here is
//! attacker-influenced (the app's built config and build output are written
//! by the app uid). The reviewed source of truth is the worktree at the
//! deployed sha; `stage_migrations` copies from there into the deploy root
//! because celld reads migration files relative to the deploy root, and
//! prunes any other `*.sql` the build may have planted. `dir` values are
//! validated to plain relative segments, symlinks are refused, and joins
//! are contained under their base, so a hostile config cannot make Outer
//! read or write outside the deploy root.
//!
//! Cross-app reachability (verified against celld 0.6.2 `d1 --help` and
//! https://celld.dev/docs/services/d1): `celld d1 migrations apply` finds a
//! node through the *bucket's* node leases and signs each request with the
//! fleet's shared secret read from the bucket. Outer passes
//! `--bucket s3://{bucket}/fleets/{slug}` with R2 temporary credentials
//! scoped to `fleets/{slug}/` (see `store::credentials`), so celld only
//! ever sees this app's leases, secret, and D1 files: another app's
//! database is unreachable (its leases are invisible from this prefix and
//! its nodes would reject our fleet secret). Database names are therefore
//! per-fleet namespaces, as celld documents ("Two Workers that name one
//! identity reach one database" — within one fleet; a typo opens an empty
//! database in *this* fleet, never another app's). No per-app registry: the
//! declared names in the built config are the apply set. Localhost dev
//! (`CredentialSource::Static`) uses the unscoped parent keys, so this
//! isolation holds only with the production R2Temp minter.

use std::collections::HashSet;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// Migration staging caps: at most this many files per directory, each at
/// most this big, and this much in total. Checked from file metadata before
/// any content is read, hashed, or copied, so a hostile tree of giant files
/// fails the migrate step instead of exhausting the Outer process.
pub const MAX_MIGRATION_FILES: usize = 1_000;
/// Per-file cap (1 MiB).
pub const MAX_MIGRATION_FILE_BYTES: u64 = 1 << 20;
/// Total cap (16 MiB).
pub const MAX_MIGRATION_TOTAL_BYTES: u64 = 16 << 20;
/// `migrations_dir` values longer than this are refused (far above any
/// legitimate dir, far below anything worrisome to join or log).
pub const MAX_MIGRATIONS_DIR_LEN: usize = 256;
/// `database_name` values longer than this are refused.
pub const MAX_DATABASE_NAME_LEN: usize = 64;
/// `database_id` values longer than this are refused (Cloudflare ids are
/// UUIDs; the cap only stops absurd config values from reaching the DB).
pub const MAX_DATABASE_ID_LEN: usize = 256;

/// One `d1_databases` entry of the built wrangler config. `database_id` is
/// the stable identity celld uses when the declaration has one (otherwise
/// celld uses `database_name`); Outer keys `d1_migration` by the same
/// effective identity, so swapping the id with the same name re-applies to
/// the new (empty) database instead of skipping as "already applied".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct D1Database {
    pub binding: String,
    pub database_name: String,
    pub database_id: Option<String>,
    pub migrations_dir: Option<String>,
}

impl D1Database {
    /// The database celld resolves: `database_id` when declared, else
    /// `database_name` (matches `celld d1 --help` and the D1 docs).
    pub fn effective_identity(&self) -> &str {
        self.database_id.as_deref().unwrap_or(&self.database_name)
    }
}

/// Parse `d1_databases` from the built config value. Entries without a
/// string `binding` or `database_name` fail the step naming the entry, as do
/// option-shaped `database_name` values and escaping `migrations_dir` values
/// (both come from the app's built config, so both are validated here before
/// anything is staged or passed to celld).
pub fn d1_databases(value: &serde_json::Value) -> anyhow::Result<Vec<D1Database>> {
    let Some(list) = value.get("d1_databases") else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("d1_databases must be an array, got {list}"))?;
    list.iter()
        .enumerate()
        .map(|(i, entry)| {
            let obj = entry
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("d1_databases[{i}] must be an object"))?;
            let binding = obj
                .get("binding")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("d1_databases[{i}] needs a string binding"))?;
            let database_name = obj
                .get("database_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("d1_databases[{i}] needs a string database_name"))?;
            let database_id = match obj.get("database_id") {
                None => None,
                Some(v) => {
                    let id = v.as_str().ok_or_else(|| {
                        anyhow::anyhow!("d1_databases[{i}] needs a string database_id")
                    })?;
                    validate_database_id(id)
                        .map_err(|e| anyhow::anyhow!("d1_databases[{i}]: {e:#}"))?;
                    Some(id.to_string())
                }
            };
            let migrations_dir = obj
                .get("migrations_dir")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            validate_database_name(database_name)
                .map_err(|e| anyhow::anyhow!("d1_databases[{i}]: {e:#}"))?;
            if let Some(dir) = migrations_dir.as_deref() {
                validate_migrations_dir(dir)
                    .map_err(|e| anyhow::anyhow!("d1_databases[{i}]: {e:#}"))?;
            }
            Ok(D1Database {
                binding: binding.to_string(),
                database_name: database_name.to_string(),
                database_id,
                migrations_dir,
            })
        })
        .collect()
}

/// The directory celld reads an entry's files from, relative to the deploy
/// root. celld's own default is `migrations`.
pub fn migrations_dir_name(entry: &D1Database) -> &str {
    entry.migrations_dir.as_deref().unwrap_or("migrations")
}

/// A `database_name` is passed positionally to
/// `celld d1 migrations apply`, so it must never parse as an option: only
/// letters, digits, `_` and `-`, 1–64 characters, never starting with `-`.
/// (Argv is passed without a shell, so spaces and metacharacters are inert;
/// the leading-dash rule closes option injection. No `--` separator: celld
/// 0.6.2's leaf parsers don't honor `--help`, let alone `--`, so there is
/// no `--` contract to rely on — `list -- foo` still reports "needs a
/// database name".)
pub fn validate_database_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > MAX_DATABASE_NAME_LEN {
        anyhow::bail!(
            "database name must be 1-{MAX_DATABASE_NAME_LEN} characters, got {}",
            name.len()
        );
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!("database name {name:?} may only contain letters, digits, '_' and '-'");
    }
    if name.starts_with('-') {
        anyhow::bail!("database name {name:?} must not start with '-'");
    }
    Ok(())
}

/// A `database_id` never reaches argv (celld resolves it from the wrangler
/// config in the deploy root, which Outer hands over as PROJECT) and is
/// only ever a bound SQLite value, so it needs no option-shape charset —
/// just non-empty and bounded, so a typo'd id is a loud deploy failure
/// rather than a silent identity change.
pub fn validate_database_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > MAX_DATABASE_ID_LEN {
        anyhow::bail!(
            "database id must be 1-{MAX_DATABASE_ID_LEN} characters, got {}",
            id.len()
        );
    }
    Ok(())
}

/// A `migrations_dir` is joined under the deploy root and the worktree by
/// the Outer process, so it must be a relative path of plain segments:
/// nothing absolute, no `.`/`..`/empty segments, no NUL or backslash, and
/// bounded in length. Anything else fails the migrate step naming the dir.
pub fn validate_migrations_dir(dir: &str) -> anyhow::Result<()> {
    if dir.is_empty() {
        anyhow::bail!("migrations dir must not be empty");
    }
    if dir.len() > MAX_MIGRATIONS_DIR_LEN {
        anyhow::bail!(
            "migrations dir is too long (max {MAX_MIGRATIONS_DIR_LEN} bytes), got {}",
            dir.len()
        );
    }
    if dir.starts_with('/') {
        anyhow::bail!("migrations dir {dir:?} must be relative, not absolute");
    }
    if dir.contains('\0') {
        anyhow::bail!("migrations dir must not contain NUL");
    }
    if dir.contains('\\') {
        anyhow::bail!("migrations dir must not contain '\\\\'");
    }
    for segment in dir.split('/') {
        if segment.is_empty() {
            anyhow::bail!("migrations dir {dir:?} has an empty segment");
        }
        if segment == "." || segment == ".." {
            anyhow::bail!("migrations dir {dir:?} must not contain '.' or '..'");
        }
    }
    Ok(())
}

/// Join a validated `dir` under `base`, refusing anything that could escape
/// `base` or follow an attacker-planted symlink (the build runs as the app
/// uid and can plant symlinks anywhere under the deploy root or worktree).
/// Refuses when any existing component at or under `base` is a symlink, and
/// confirms the deepest existing ancestor canonicalizes inside `base`.
fn join_within(base: &Path, dir: &str) -> anyhow::Result<PathBuf> {
    validate_migrations_dir(dir)?;
    let mut path = base.to_path_buf();
    for segment in dir.split('/') {
        path.push(segment);
    }
    let mut cursor = base.to_path_buf();
    for segment in dir.split('/') {
        cursor.push(segment);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing migrations dir {dir:?}: {} is a symlink",
                    cursor.display()
                );
            }
            Ok(_) | Err(_) => {}
        }
    }
    let canonical_base = base
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve migrations base {}: {e:#}", base.display()))?;
    let mut ancestor: Option<&Path> = Some(&path);
    while let Some(current) = ancestor {
        if let Ok(canonical) = current.canonicalize() {
            if !canonical.starts_with(&canonical_base) {
                anyhow::bail!(
                    "refusing migrations dir {dir:?}: escapes {}",
                    base.display()
                );
            }
            return Ok(path);
        }
        ancestor = current.parent();
    }
    anyhow::bail!(
        "refusing migrations dir {dir:?}: escapes {}",
        base.display()
    )
}

/// One migration file: its file name and the sha256 Outer saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFile {
    pub name: String,
    pub sha256: String,
}

/// Sorted `*.sql` files in `dir`, with content hashes. The extension matches
/// ASCII case-insensitively, like celld does (`celld d1 --help`: "The `sql`
/// extension is ASCII case-insensitive"). Only a missing dir holds no files:
/// any other discovery failure — an unreadable dir, a bad entry, a
/// non-UTF-8 name, a stat or read error — fails, so the deploy can never
/// proceed "with no migrations" over a tree it failed to see. Symlinks and
/// non-regular files are skipped without following them. Over-cap trees
/// fail before any content is read: at most `MAX_MIGRATION_FILES` files,
/// `MAX_MIGRATION_FILE_BYTES` each, `MAX_MIGRATION_TOTAL_BYTES` in total.
/// Reads open with `O_NOFOLLOW`, so a path swapped for a symlink between
/// the stat and the read fails instead of following it.
pub fn list_migration_files(dir: &Path) -> anyhow::Result<Vec<MigrationFile>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            anyhow::bail!("cannot list migration files in {}: {e:#}", dir.display());
        }
    };
    // Stat first: names and sizes without reading contents or following links.
    let mut staged: Vec<(String, u64)> = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| {
            anyhow::anyhow!("cannot list migration files in {}: {e:#}", dir.display())
        })?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => {
                anyhow::bail!(
                    "migration file name in {} is not UTF-8; rename it",
                    dir.display()
                );
            }
        };
        if !name.to_ascii_lowercase().ends_with(".sql") {
            continue;
        }
        let meta = std::fs::symlink_metadata(entry.path()).map_err(|e| {
            anyhow::anyhow!(
                "cannot stat migration file {name} in {}: {e:#}",
                dir.display()
            )
        })?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_MIGRATION_FILE_BYTES {
            anyhow::bail!(
                "migration file {} in {} exceeds {} bytes",
                name,
                dir.display(),
                MAX_MIGRATION_FILE_BYTES
            );
        }
        staged.push((name, meta.len()));
    }
    staged.sort_by(|a, b| a.0.cmp(&b.0));
    if staged.len() > MAX_MIGRATION_FILES {
        anyhow::bail!(
            "too many migration files in {}: {} over the {} cap",
            dir.display(),
            staged.len(),
            MAX_MIGRATION_FILES
        );
    }
    let total: u64 = staged.iter().map(|(_, size)| size).sum();
    if total > MAX_MIGRATION_TOTAL_BYTES {
        anyhow::bail!(
            "migration files in {} total {total} bytes over the {MAX_MIGRATION_TOTAL_BYTES} cap",
            dir.display(),
        );
    }
    // Only now read and hash: everything above came from metadata alone, and
    // each open refuses to follow symlinks.
    let mut out = Vec::with_capacity(staged.len());
    for (name, _) in staged {
        let path = dir.join(&name);
        let mut file = open_no_follow(&path)?;
        let mut bytes = Vec::new();
        use std::io::Read as _;
        file.read_to_end(&mut bytes)
            .map_err(|e| anyhow::anyhow!("cannot read migration file {}: {e:#}", path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        out.push(MigrationFile {
            name,
            sha256: hex::encode(hasher.finalize()),
        });
    }
    Ok(out)
}

/// Open a migration file for reading without following a trailing symlink:
/// a path swapped between the stat and the open fails with a clear error
/// instead of handing Outer someone else's bytes.
fn open_no_follow(path: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| anyhow::anyhow!("cannot open migration file {}: {e:#}", path.display()))
}

/// Outer stages as root under the sandbox's `077` umask, but `celld d1
/// migrations apply` runs as the app uid and must read what was staged:
/// set `mode` on the open fd (never through a path that could be swapped).
/// Read-only for the app: it can't rewrite reviewed bytes after hashing.
fn readable_by_app(file: &std::fs::File, path: &Path, mode: libc::mode_t) -> anyhow::Result<()> {
    use std::os::unix::io::AsRawFd as _;
    // SAFETY: fchmod on a file descriptor we own; no memory is touched.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        anyhow::bail!(
            "cannot make staged migration {} readable: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Make every directory from `root` down to `target` that Outer itself
/// created (owned by this process's euid) `0755`, so the app uid can reach
/// the staged files. Directories the app owns are left alone: it can
/// already traverse them. Each is opened `O_DIRECTORY | O_NOFOLLOW`, so a
/// component swapped for a symlink fails instead of being followed.
fn traversable_by_app(root: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let rel = target
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("{} is not under {}", target.display(), root.display()))?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let mut dir = root.to_path_buf();
    for component in rel.components() {
        dir.push(component);
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&dir)
            .map_err(|e| anyhow::anyhow!("cannot open staged dir {}: {e:#}", dir.display()))?;
        if handle.metadata()?.uid() == euid {
            readable_by_app(&handle, &dir, 0o755)?;
        }
    }
    Ok(())
}

/// Where an entry's files come from: the first conventional repo dir that
/// holds `*.sql` — `<work>/<dir>`, then Void's `db/migrations` when
/// `dir == "migrations"`. The winner is copied into `<root>/<dir>` because
/// celld reads migration files relative to the deploy root, and any other
/// `*.sql` already staged there (unreviewed build output) is removed first,
/// so only reviewed worktree bytes reach celld. Build output is never a
/// source: files that exist only under `<root>/<dir>` are ignored, not
/// applied. Returns `None` when no files exist anywhere.
pub fn stage_migrations(
    root: &Path,
    work: &Path,
    dir: &str,
) -> anyhow::Result<Option<Vec<MigrationFile>>> {
    let target = join_within(root, dir)?;
    let mut candidates = vec![join_within(work, dir)?];
    // Void's conventional layout keeps generated SQL out of the way.
    if dir == "migrations" {
        candidates.push(join_within(work, "db/migrations")?);
    }
    for candidate in candidates {
        let files = list_migration_files(&candidate)?;
        if files.is_empty() {
            continue;
        }
        if candidate == target {
            // The deploy root is the repo dir itself: reviewed files already
            // in place, nothing to copy and nothing unreviewed to prune.
            return Ok(Some(files));
        }
        prepare_target(&target, &files)?;
        for file in &files {
            let src_path = candidate.join(&file.name);
            let mut src = open_no_follow(&src_path)?;
            // `O_NOFOLLOW` with `O_CREAT`: a staged path swapped for a
            // symlink between the prune and the copy fails instead of
            // writing through the link.
            let dst_path = target.join(&file.name);
            let mut dst = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&dst_path)
                .map_err(|e| {
                    anyhow::anyhow!("cannot stage migration file {}: {e:#}", dst_path.display())
                })?;
            std::io::copy(&mut src, &mut dst).map_err(|e| {
                anyhow::anyhow!("cannot stage migration file {}: {e:#}", dst_path.display())
            })?;
            readable_by_app(&dst, &dst_path, 0o644)?;
        }
        traversable_by_app(root, &target)?;
        // The target itself is re-checked after the copy: a swapped-in
        // symlink fails the step before celld ever reads through it.
        // (Component swaps above the target remain a residual TOCTOU; the
        // build that could plant them no longer runs by this point.)
        if std::fs::symlink_metadata(&target)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            anyhow::bail!(
                "refusing to stage migrations into {}: it became a symlink",
                target.display()
            );
        }
        return Ok(Some(list_migration_files(&target)?));
    }
    Ok(None)
}

/// Make `target` a real directory Outer just verified, then delete any
/// `*.sql` in it that the reviewed source set does not contain: those are
/// unreviewed build output (or planted files) that celld would otherwise
/// read and apply. Refuses when `target` itself is a symlink, a
/// non-directory, or holds a subdirectory shaped like a migration.
fn prepare_target(target: &Path, files: &[MigrationFile]) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!(
                "refusing to stage migrations into {}: it is a symlink",
                target.display()
            );
        }
        Ok(meta) if !meta.is_dir() => {
            anyhow::bail!(
                "refusing to stage migrations into {}: not a directory",
                target.display()
            );
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(target)?;
        }
        Err(e) => return Err(e.into()),
    }
    let keep: HashSet<&str> = files.iter().map(|f| f.name.as_str()).collect();
    let rd = std::fs::read_dir(target)?;
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_ascii_lowercase().ends_with(".sql") || keep.contains(name.as_str()) {
            continue;
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            anyhow::bail!(
                "refusing to stage migrations into {}: {name} is a directory",
                target.display()
            );
        }
        std::fs::remove_file(entry.path())?;
    }
    Ok(())
}

/// The migration file a failed `celld d1 migrations apply` complains about:
/// the last whitespace-separated token shaped like `NNNN_anything.sql`.
/// celld applies in order and stops at the failure, so the failing file is
/// the last one its output names; when the output names none, this is `None`
/// and the caller falls back to the raw output.
pub fn failing_file(output: &str) -> Option<String> {
    output
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`')
        .map(|t| t.trim_matches(|c: char| c == ':' || c == ',' || c == '.' || c == '(' || c == ')'))
        .rfind(|t| {
            t.len() > 4
                && t.to_ascii_lowercase().ends_with(".sql")
                && t.as_bytes().first().is_some_and(|b| b.is_ascii_digit())
        })
        .map(str::to_string)
}

/// The sha256 Outer recorded when it applied `name` to `database_identity`
/// (the effective id: `database_id` when declared, else `database_name`),
/// if any.
pub async fn applied_sha(
    pool: &SqlitePool,
    app_id: &str,
    database_identity: &str,
    name: &str,
) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT sha256 FROM d1_migration WHERE app_id = ?1 AND database_identity = ?2 AND name = ?3",
    )
    .bind(app_id)
    .bind(database_identity)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(sha,)| sha))
}

/// Record every file of a successful apply (upsert: a file that somehow
/// reached celld twice keeps the newest hash, which equals the old one),
/// keyed by the effective identity celld applied to. `database_name` is
/// kept alongside for humans; the conflict key is the identity, so a
/// changed `database_id` under the same name starts clean against the new
/// (empty) database instead of skipping as "already applied".
/// Renaming a file evades the edited-after-apply refusal by construction —
/// a new name is a new migration to both celld and this table — which is
/// inherent to name-keyed bookkeeping, not a bypass: the renamed file still
/// runs through celld in numeric-prefix order under review.
pub async fn record_applied(
    pool: &SqlitePool,
    app_id: &str,
    database_name: &str,
    database_identity: &str,
    files: &[MigrationFile],
    now_ms: i64,
) -> anyhow::Result<()> {
    for file in files {
        sqlx::query(
            "INSERT INTO d1_migration (app_id, database_name, database_identity, name, sha256, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (app_id, database_identity, name)
             DO UPDATE SET sha256 = excluded.sha256, applied_at = excluded.applied_at",
        )
        .bind(app_id)
        .bind(database_name)
        .bind(database_identity)
        .bind(&file.name)
        .bind(&file.sha256)
        .bind(now_ms)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> serde_json::Value {
        json
    }

    #[test]
    fn no_d1_key_means_no_databases() {
        let dbs = d1_databases(&config(serde_json::json!({"name": "x"}))).unwrap();
        assert!(dbs.is_empty());
    }

    #[test]
    fn parses_entries_with_optional_migrations_dir() {
        let dbs = d1_databases(&config(serde_json::json!({
            "d1_databases": [
                {"binding": "DB", "database_name": "app-db"},
                {"binding": "AUX", "database_name": "aux", "migrations_dir": "aux-migrations"},
            ],
        })))
        .unwrap();
        assert_eq!(dbs.len(), 2);
        assert_eq!(migrations_dir_name(&dbs[0]), "migrations");
        assert_eq!(migrations_dir_name(&dbs[1]), "aux-migrations");
    }

    #[test]
    fn database_id_is_optional_but_selects_the_identity() {
        let dbs = d1_databases(&config(serde_json::json!({
            "d1_databases": [
                {"binding": "DB", "database_name": "app-db"},
                {"binding": "AUX", "database_name": "aux", "database_id": "00000000-0000-0000-0000-000000000000"},
            ],
        })))
        .unwrap();
        assert_eq!(dbs[0].database_id, None);
        assert_eq!(dbs[0].effective_identity(), "app-db");
        assert_eq!(
            dbs[1].effective_identity(),
            "00000000-0000-0000-0000-000000000000"
        );
        for bad in ["", &"x".repeat(MAX_DATABASE_ID_LEN + 1)] {
            let err = format!(
                "{:#}",
                d1_databases(&config(serde_json::json!({
                    "d1_databases": [{"binding": "DB", "database_name": "app-db", "database_id": bad}],
                })))
                .unwrap_err()
            );
            assert!(
                err.contains("d1_databases[0]"),
                "names the entry for {bad:?}: {err}"
            );
        }
        let err = format!(
            "{:#}",
            d1_databases(&config(serde_json::json!({
                "d1_databases": [{"binding": "DB", "database_name": "app-db", "database_id": 7}],
            })))
            .unwrap_err()
        );
        assert!(err.contains("d1_databases[0]"), "names the entry: {err}");
    }

    #[test]
    fn entry_without_binding_or_name_fails_naming_the_index() {
        let err = format!(
            "{:#}",
            d1_databases(&config(serde_json::json!({
                "d1_databases": [{"binding": "DB"}],
            })))
            .unwrap_err()
        );
        assert!(err.contains("d1_databases[0]"), "names the entry: {err}");
    }

    #[test]
    fn database_names_reject_flags_and_junk() {
        for bad in [
            "",
            "--bucket",
            "-x",
            "-",
            "a b",
            "a/b",
            "a;rm -rf",
            "db\x00name",
            &"x".repeat(MAX_DATABASE_NAME_LEN + 1),
        ] {
            let err = format!(
                "{:#}",
                d1_databases(&config(serde_json::json!({
                    "d1_databases": [{"binding": "DB", "database_name": bad}],
                })))
                .unwrap_err()
            );
            assert!(
                err.contains("d1_databases[0]"),
                "names the entry for {bad:?}: {err}"
            );
        }
        let ok = "app-db_1";
        let dbs = d1_databases(&config(serde_json::json!({
            "d1_databases": [{"binding": "DB", "database_name": ok}],
        })))
        .unwrap();
        assert_eq!(dbs[0].database_name, ok);
        let max = "x".repeat(MAX_DATABASE_NAME_LEN);
        let dbs = d1_databases(&config(serde_json::json!({
            "d1_databases": [{"binding": "DB", "database_name": max}],
        })))
        .unwrap();
        assert_eq!(dbs[0].database_name.len(), MAX_DATABASE_NAME_LEN);
    }

    #[test]
    fn migrations_dirs_reject_traversal_and_absolute() {
        for bad in [
            "",
            "/migrations",
            "../x",
            "a/../../x",
            "a//b",
            "/",
            ".",
            "..",
            "migrations/",
            "a\\b",
            "a\x00b",
            &"x".repeat(MAX_MIGRATIONS_DIR_LEN + 1),
        ] {
            let err = format!(
                "{:#}",
                d1_databases(&config(serde_json::json!({
                    "d1_databases": [
                        {"binding": "DB", "database_name": "app-db", "migrations_dir": bad},
                    ],
                })))
                .unwrap_err()
            );
            assert!(
                err.contains("d1_databases[0]"),
                "names the entry for {bad:?}: {err}"
            );
            let work = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let err = format!(
                "{:#}",
                stage_migrations(root.path(), work.path(), bad).unwrap_err()
            );
            assert!(!err.is_empty(), "stage refuses {bad:?}");
        }
        for ok in [
            "migrations",
            "db/migrations",
            "aux-migrations",
            "d1/a.b-c_d",
        ] {
            let work = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            assert!(
                stage_migrations(root.path(), work.path(), ok)
                    .unwrap()
                    .is_none(),
                "empty {ok:?} stages to nothing"
            );
        }
    }

    #[test]
    fn lists_sql_files_sorted_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0002_b.sql"), "b").unwrap();
        std::fs::write(dir.path().join("0001_A.SQL"), "a").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let files = list_migration_files(dir.path()).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["0001_A.SQL", "0002_b.sql"]);
        assert_ne!(files[0].sha256, files[1].sha256);
        assert!(list_migration_files(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn undiscoverable_names_fail_instead_of_silently_skipping() {
        use std::os::unix::ffi::OsStringExt as _;
        // A migration file Outer cannot name is a discovery failure, not an
        // empty dir: failing beats deploying without its schema change.
        let dir = tempfile::tempdir().unwrap();
        let mut raw = b"0001_\xff.sql".to_vec();
        let name = std::ffi::OsString::from_vec(std::mem::take(&mut raw));
        std::fs::write(dir.path().join(&name), "CREATE TABLE t (id INTEGER);").unwrap();
        let err = format!("{:#}", list_migration_files(dir.path()).unwrap_err());
        assert!(err.contains("not UTF-8"), "fails loudly: {err}");
    }

    #[test]
    fn repo_files_stage_into_the_root_overwriting_build_output() {
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("db").join("migrations")).unwrap();
        std::fs::write(
            work.path()
                .join("db")
                .join("migrations")
                .join("0001_init.sql"),
            "CREATE TABLE t (id INTEGER);",
        )
        .unwrap();

        // Conventional Void layout stages into the deploy root for celld.
        let staged = stage_migrations(root.path(), work.path(), "migrations")
            .unwrap()
            .expect("files exist");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "0001_init.sql");
        assert!(
            root.path()
                .join("migrations")
                .join("0001_init.sql")
                .is_file(),
            "celld reads them from the deploy root"
        );

        // Build output never wins: the reviewed worktree copy overwrites it.
        std::fs::write(
            root.path().join("migrations").join("0001_init.sql"),
            "STAGED",
        )
        .unwrap();
        let staged = stage_migrations(root.path(), work.path(), "migrations")
            .unwrap()
            .expect("reviewed files win");
        assert_eq!(staged.len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("migrations").join("0001_init.sql")).unwrap(),
            "CREATE TABLE t (id INTEGER);"
        );
    }

    #[test]
    fn stage_ignores_root_only_files_and_prunes_planted_sql() {
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        // Unreviewed build output under the deploy root is not a source.
        std::fs::create_dir_all(root.path().join("migrations")).unwrap();
        std::fs::write(
            root.path().join("migrations").join("9999_evil.sql"),
            "DROP TABLE t;",
        )
        .unwrap();
        let staged = stage_migrations(root.path(), work.path(), "migrations").unwrap();
        assert!(staged.is_none(), "root-only files are ignored, not applied");

        // Once the repo has migrations, the planted file is pruned, not kept.
        std::fs::create_dir_all(work.path().join("db").join("migrations")).unwrap();
        std::fs::write(
            work.path()
                .join("db")
                .join("migrations")
                .join("0001_init.sql"),
            "CREATE TABLE t (id INTEGER);",
        )
        .unwrap();
        let staged = stage_migrations(root.path(), work.path(), "migrations")
            .unwrap()
            .expect("files exist");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "0001_init.sql");
        assert!(
            !root
                .path()
                .join("migrations")
                .join("9999_evil.sql")
                .exists(),
            "planted SQL never reaches celld"
        );
    }

    /// Outer stages as root under a `077` umask while celld applies as the
    /// app uid: staged dirs Outer owns and the copied files end up readable
    /// (and not writable) by others, or celld reports "no migrations
    /// directory" — the live failure of 2026-10-24.
    #[test]
    fn staged_migrations_are_readable_by_the_app_uid() {
        use std::os::unix::fs::PermissionsExt as _;
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let src = work.path().join("db").join("migrations");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("0001_init.sql"), "CREATE TABLE t (id INTEGER);").unwrap();
        // What a 077 umask leaves behind: private dirs and a private file.
        let target = root.path().join("db").join("migrations");
        std::fs::create_dir_all(&target).unwrap();
        for dir in [root.path().join("db"), target.clone()] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(target.join("0001_init.sql"), "").unwrap();
        std::fs::set_permissions(
            target.join("0001_init.sql"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        stage_migrations(root.path(), work.path(), "db/migrations")
            .unwrap()
            .expect("files exist");

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root.path().join("db")), 0o755);
        assert_eq!(mode(&target), 0o755);
        assert_eq!(mode(&target.join("0001_init.sql")), 0o644);
        assert_eq!(
            std::fs::read_to_string(target.join("0001_init.sql")).unwrap(),
            "CREATE TABLE t (id INTEGER);"
        );
    }

    #[test]
    fn traversal_dirs_write_nothing_outside_the_root() {
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        for bad in [
            "../escape",
            "a/../../escape",
            "/definitely-not-here-outer-test",
        ] {
            let err = format!(
                "{:#}",
                stage_migrations(&root, work.path(), bad).unwrap_err()
            );
            assert!(!err.is_empty(), "stage refuses {bad:?}");
        }
        assert!(
            !base.path().join("escape").exists(),
            "no sibling of the root was written"
        );
        assert!(
            !Path::new("/definitely-not-here-outer-test").exists(),
            "no absolute path was written"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_refuses_symlinks_and_skips_symlinked_files() {
        use std::os::unix::fs::symlink;
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("evil.sql"), "DROP TABLE t;").unwrap();

        // A symlinked source dir is refused, and nothing is staged.
        symlink(outside.path(), work.path().join("migrations")).unwrap();
        let err = format!(
            "{:#}",
            stage_migrations(root.path(), work.path(), "migrations").unwrap_err()
        );
        assert!(err.contains("symlink"), "names the cause: {err}");
        assert!(
            !root.path().join("migrations").exists(),
            "refusal stages nothing"
        );
        std::fs::remove_file(work.path().join("migrations")).unwrap();

        // A symlinked file inside a real dir is skipped, never copied.
        std::fs::create_dir_all(work.path().join("migrations")).unwrap();
        std::fs::write(
            work.path().join("migrations").join("0001_init.sql"),
            "CREATE TABLE t (id INTEGER);",
        )
        .unwrap();
        symlink(
            outside.path().join("evil.sql"),
            work.path().join("migrations").join("0002_evil.sql"),
        )
        .unwrap();
        let staged = stage_migrations(root.path(), work.path(), "migrations")
            .unwrap()
            .expect("files exist");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "0001_init.sql");
        assert!(
            !root
                .path()
                .join("migrations")
                .join("0002_evil.sql")
                .exists(),
            "symlinked files never reach the deploy root"
        );

        // A symlinked target is refused, and the link target is untouched.
        let root2 = tempfile::tempdir().unwrap();
        let planted = tempfile::tempdir().unwrap();
        symlink(planted.path(), root2.path().join("migrations")).unwrap();
        let err = format!(
            "{:#}",
            stage_migrations(root2.path(), work.path(), "migrations").unwrap_err()
        );
        assert!(err.contains("symlink"), "names the cause: {err}");
        let mut survivors: Vec<_> = std::fs::read_dir(planted.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        survivors.sort();
        assert!(
            survivors.is_empty(),
            "nothing written outside: {survivors:?}"
        );
    }

    #[test]
    fn migration_caps_fail_before_reading() {
        // One file over the per-file cap.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("0001_big.sql"),
            vec![b'x'; MAX_MIGRATION_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let err = format!("{:#}", list_migration_files(dir.path()).unwrap_err());
        assert!(err.contains("exceeds"), "names the cap: {err}");
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("migrations")).unwrap();
        std::fs::write(
            work.path().join("migrations").join("0001_big.sql"),
            vec![b'x'; MAX_MIGRATION_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            stage_migrations(root.path(), work.path(), "migrations").is_err(),
            "over-cap sources fail the stage, nothing staged"
        );
        assert!(!root.path().join("migrations").exists());

        // Enough files to break the total cap.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..17 {
            std::fs::write(
                dir.path().join(format!("{i:04}_f.sql")),
                vec![b'x'; MAX_MIGRATION_FILE_BYTES as usize],
            )
            .unwrap();
        }
        let err = format!("{:#}", list_migration_files(dir.path()).unwrap_err());
        assert!(err.contains("total"), "names the cap: {err}");

        // Enough tiny files to break the count cap.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=MAX_MIGRATION_FILES {
            std::fs::write(dir.path().join(format!("{i:04}_f.sql")), "x").unwrap();
        }
        let err = format!("{:#}", list_migration_files(dir.path()).unwrap_err());
        assert!(err.contains("too many"), "names the cap: {err}");
    }

    #[test]
    fn no_files_anywhere_means_no_migrations() {
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        assert!(stage_migrations(root.path(), work.path(), "migrations")
            .unwrap()
            .is_none());
    }

    #[test]
    fn failing_file_picks_the_migration_out_of_celld_output() {
        let out =
            "applying 0001_init.sql\nerror applying 0002_add_users.sql: near \"X\": syntax error";
        assert_eq!(
            failing_file(out).as_deref(),
            Some("0002_add_users.sql"),
            "the failing file, not the applied one"
        );
        assert_eq!(failing_file("bucket unreachable"), None);
    }
}
