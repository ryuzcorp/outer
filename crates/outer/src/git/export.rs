//! Pro code export (spec §7.5): a zip of the tree at a commit, uploaded to
//! `outer/exports/{slug}/`, returned as a presigned R2 URL valid for one
//! hour. Objects older than 24 h die in the daily retention sweep.
//!
//! The zip holds exactly the blobs `list_files` names at the resolved sha
//! (gitlinks already filtered there), under `<slug>-<shortsha>/`. Symlinks
//! ride as regular files carrying their target text: Ilha apps have none,
//! and failing the whole export on one would be worse. One blob loads at a
//! time and the zip streams to a temp file, so memory stays near the
//! largest single blob, not the tree.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::git::local::{self, FileEntry};
use crate::git::manifest::{self, GitConfig, GitSync};
use crate::store::Store;

/// Total tree budget: exports may be larger than file views (blobs over
/// 256 KiB would not even render), but a hostile tree still cannot OOM the
/// platform through one export call.
pub const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;
/// Largest single blob an export carries: one file loads whole, so this
/// bounds export memory, not just the object.
pub const MAX_EXPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Most paths one export zips: `ls-tree` output past this refuses instead
/// of streaming an unbounded listing through the zip writer.
pub const MAX_EXPORT_FILES: usize = 100_000;
/// How long the presigned download URL works (spec §7.5: one hour).
pub const EXPORT_URL_TTL_SECS: u64 = 3600;
/// How long an uploaded export lives (spec §7.5: deleted after 24 h).
pub const EXPORT_RETENTION_SECS: i64 = 24 * 3600;

/// The size budgets [`export_app`] enforces. Tests pass small caps to pin
/// the refusals without building a 256 MiB tree.
#[derive(Debug, Clone)]
pub struct ExportCaps {
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_files: usize,
}

impl Default for ExportCaps {
    fn default() -> Self {
        Self {
            max_total_bytes: MAX_EXPORT_BYTES,
            max_file_bytes: MAX_EXPORT_FILE_BYTES,
            max_files: MAX_EXPORT_FILES,
        }
    }
}

/// One finished export: what the MCP tool and the console endpoint return.
#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub sha: String,
    pub key: String,
    pub bytes: u64,
    pub url: String,
    pub expires_at: String,
}

/// Why an export refused. Callers map these to the spec §8.3 codes; only
/// `Internal` hides its detail (it goes to the server log).
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("app has no commits yet")]
    NoRepo,
    #[error("unknown commit {0:?}")]
    UnknownRev(String),
    #[error("tree has {0} files, over the export cap")]
    TooManyFiles(usize),
    #[error("{path} is {size} bytes, over the export file cap")]
    FileTooLarge { path: String, size: u64 },
    #[error("tree is {bytes} bytes, over the export cap")]
    TooLarge { bytes: u64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// The handles [`export_app`] needs: the pool for the `app_export` row, the
/// store for the zip object, and the git read paths.
#[derive(Clone, Copy)]
pub struct ExportDeps<'a> {
    pub pool: &'a sqlx::SqlitePool,
    pub store: &'a Store,
    pub git: &'a GitConfig,
    pub sync: &'a GitSync,
}

/// What to export: the slug, its app id for the row, the rev (default:
/// `main`), the size budgets, and now for the row and the link expiry.
pub struct ExportParams<'a> {
    pub slug: &'a str,
    pub app_id: &'a str,
    pub rev: Option<&'a str>,
    pub caps: &'a ExportCaps,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// Zip the tree at `rev` (default: `main`) and upload it to
/// `outer/exports/{slug}/<shortsha>-<random>.zip`. Returns the presigned
/// download URL. The caller checks the plan and the role first: this runs
/// no gate of its own.
pub async fn export_app(
    deps: &ExportDeps<'_>,
    params: ExportParams<'_>,
) -> Result<ExportOutcome, ExportError> {
    let ExportParams {
        slug,
        app_id,
        rev,
        caps,
        now,
    } = params;
    let ExportDeps {
        pool,
        store,
        git,
        sync,
    } = *deps;
    manifest::validate_slug(slug).map_err(ExportError::Internal)?;
    if let Some(rev) = rev {
        local::validate_sha(rev).map_err(|_| ExportError::UnknownRev(rev.to_string()))?;
    }
    let bare = manifest::ensure_synced(store, git, sync, slug).await?;
    let sha = match local::resolve_rev(&bare, rev).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return Err(ExportError::NoRepo),
        Err(e) if rev.is_some() && local::is_unknown_rev(&e) => {
            return Err(ExportError::UnknownRev(rev.unwrap_or_default().to_string()));
        }
        Err(err) => return Err(ExportError::Internal(err)),
    };
    let (_, entries, truncated) = local::list_files(&bare, Some(&sha), None, caps.max_files + 1)
        .await
        .map_err(ExportError::Internal)?;
    if truncated || entries.len() > caps.max_files {
        return Err(ExportError::TooManyFiles(entries.len()));
    }
    let mut total: u64 = 0;
    for entry in &entries {
        if entry.size > caps.max_file_bytes {
            return Err(ExportError::FileTooLarge {
                path: entry.path.clone(),
                size: entry.size,
            });
        }
        total = total.saturating_add(entry.size);
        if total > caps.max_total_bytes {
            return Err(ExportError::TooLarge { bytes: total });
        }
    }

    let short = sha.chars().take(7).collect::<String>();
    let prefix = format!("{slug}-{short}/");
    let job = ZipJob {
        bare,
        sha: sha.clone(),
        prefix,
        entries,
    };
    let dest = tempfile::NamedTempFile::new()
        .map_err(|err| ExportError::Internal(anyhow::anyhow!("export temp file: {err:#}")))?;
    let dest_path = dest.path().to_path_buf();
    tokio::task::spawn_blocking(move || build_zip(&job, &dest_path))
        .await
        .map_err(|err| ExportError::Internal(anyhow::anyhow!("export zip task: {err}")))??;
    let bytes = dest
        .as_file()
        .metadata()
        .map(|meta| meta.len())
        .map_err(|err| ExportError::Internal(anyhow::anyhow!("export zip stat: {err:#}")))?;

    let suffix: [u8; 8] = rand::random();
    let key = format!("outer/exports/{slug}/{short}-{}.zip", hex::encode(suffix));
    let created_at = now.timestamp_millis();
    sqlx::query(
        "INSERT INTO app_export (key, app_id, sha, bytes, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&key)
    .bind(app_id)
    .bind(&sha)
    .bind(bytes as i64)
    .bind(created_at)
    .execute(pool)
    .await
    .map_err(|err| ExportError::Internal(anyhow::anyhow!("record export: {err:#}")))?;
    if let Err(err) = store.put_file(&key, dest.path()).await {
        let _ = sqlx::query("DELETE FROM app_export WHERE key = ?")
            .bind(&key)
            .execute(pool)
            .await;
        return Err(ExportError::Internal(err.context("upload export")));
    }

    let filename = format!("{slug}-{short}.zip");
    let url = store
        .presign_get(
            &key,
            &filename,
            std::time::Duration::from_secs(EXPORT_URL_TTL_SECS),
        )
        .map_err(ExportError::Internal)?;
    let expires_at = (now + chrono::Duration::seconds(EXPORT_URL_TTL_SECS as i64)).to_rfc3339();
    Ok(ExportOutcome {
        sha,
        key,
        bytes,
        url,
        expires_at,
    })
}

/// One export's inputs, moved into the blocking thread: the sha's tree is
/// immutable, so the mirror moving under us cannot change what we zip.
struct ZipJob {
    bare: PathBuf,
    sha: String,
    prefix: String,
    entries: Vec<FileEntry>,
}

/// Write every entry under `prefix/` into a fresh zip at `dest`. One
/// blocking thread (the `zip` writer is synchronous) running synchronous
/// `git cat-file`, with the same locked-down env as the async git reads.
fn build_zip(job: &ZipJob, dest: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for entry in &job.entries {
        let name = format!("{}{}", job.prefix, entry.path);
        zip.start_file(name, options)?;
        let spec = format!("{}:{}", job.sha, entry.path);
        let out = std::process::Command::new("git")
            .arg(format!("--git-dir={}", job.bare.display()))
            .args(["cat-file", "blob", &spec])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|err| anyhow::anyhow!("git cat-file {}: {err:#}", entry.path))?;
        if !out.status.success() {
            anyhow::bail!(
                "git cat-file {} failed\n{}",
                entry.path,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        zip.write_all(&out.stdout)?;
    }
    zip.finish()?;
    Ok(())
}

/// Delete exports older than [`EXPORT_RETENTION_SECS`]: the store object
/// (best-effort, then logged) and the row. A missing object is fine — the
/// row still goes, so a half-failed prune converges on the next tick.
pub async fn prune_exports(
    pool: &sqlx::SqlitePool,
    store: &Store,
    now_ms: i64,
) -> anyhow::Result<u64> {
    let cutoff = now_ms - EXPORT_RETENTION_SECS * 1000;
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT key FROM app_export WHERE created_at < ?1 ORDER BY key")
            .bind(cutoff)
            .fetch_all(pool)
            .await?;
    let mut pruned = 0;
    for key in &keys {
        if let Err(err) = store.delete(key).await {
            tracing::warn!(key = %key, error = %format!("{err:#}"), "export prune delete failed");
            continue;
        }
        sqlx::query("DELETE FROM app_export WHERE key = ?1")
            .bind(key)
            .execute(pool)
            .await?;
        pruned += 1;
    }
    Ok(pruned)
}
