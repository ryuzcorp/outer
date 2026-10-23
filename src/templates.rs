//! The vendored Ilha + Void template (spec §16).
//!
//! `outer templates sync` pins the template repo at a sha into the store as a
//! git bundle plus a `current.json` pointer; app creation [`materialize`]s
//! that bundle into a fresh directory. No network is needed past the sync.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Where the template lives upstream.
pub const ILHA_VOID_URL: &str = "https://github.com/ryuzcorp/noite-template-ilha";
/// The pinned commit `outer templates sync` vendors.
pub const ILHA_VOID_SHA: &str = "636715ea44d8944701652abda734ab610e75c69d";

/// Pointer to the vendored bundle in the store.
const CURRENT_KEY: &str = "outer/templates/ilha-void/current.json";

/// Per-step git timeout: a full clone of the small template repo takes seconds.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Key of a vendored template bundle in the store.
fn bundle_key(sha: &str) -> String {
    format!("outer/templates/ilha-void/{sha}.bundle")
}

/// What [`sync`] vendors and [`materialize`] reads back. On the wire the
/// bundle key travels as `bundle` (see `current.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateRef {
    pub sha: String,
    #[serde(rename = "bundle")]
    pub bundle_key: String,
    pub source: String,
}

/// Run git with a quiet, non-interactive env and a timeout.
async fn run_git(
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {args:?} timed out"))?
        .context("spawn git")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {args:?} failed\n{err}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Clone `url`, check out exactly `sha` (failing when it is missing), bundle
/// that commit, and upload the bundle plus the `current.json` pointer.
/// Re-running for the same sha overwrites the same keys: idempotent.
pub async fn sync(store: &Store, url: &str, sha: &str) -> anyhow::Result<TemplateRef> {
    // Full lowercase commit sha only: an abbreviated sha or branch name would
    // pass `cat-file` and be stored, but `materialize` checks out and compares
    // `rev-parse HEAD` (always full length), so every later create would fail.
    anyhow::ensure!(
        sha.len() == 40
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "template sha {sha:?} must be a full 40-character lowercase commit sha"
    );
    let tmp = tempfile::tempdir().context("template scratch dir")?;
    let src = tmp.path().join("src");
    let src_str = src.to_string_lossy().into_owned();
    let obj = format!("{sha}^{{commit}}");
    run_git(&["clone", url, src_str.as_str()], None, &[]).await?;
    run_git(
        &["-C", src_str.as_str(), "cat-file", "-e", obj.as_str()],
        None,
        &[],
    )
    .await
    .map_err(|_| anyhow::anyhow!("template sha {sha} not found in {url}"))?;
    run_git(
        &[
            "-c",
            "advice.detachedHead=false",
            "-C",
            src_str.as_str(),
            "checkout",
            "--detach",
            sha,
        ],
        None,
        &[],
    )
    .await?;
    let bundle_path = tmp.path().join("template.bundle");
    let bundle_str = bundle_path.to_string_lossy().into_owned();
    run_git(
        &[
            "-C",
            src_str.as_str(),
            "bundle",
            "create",
            bundle_str.as_str(),
            sha,
            "--all",
        ],
        None,
        &[],
    )
    .await?;
    let key = bundle_key(sha);
    store.put_file(&key, &bundle_path).await?;
    let template = TemplateRef {
        sha: sha.to_string(),
        bundle_key: key,
        source: url.to_string(),
    };
    store
        .put(CURRENT_KEY, serde_json::to_vec(&template)?)
        .await?;
    Ok(template)
}

/// Copy a directory tree to `dest`, skipping a top-level `.git`.
fn copy_tree(src: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        copy_one(&entry.path(), &dest.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_one(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let kind = src.symlink_metadata()?.file_type();
    if kind.is_symlink() {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dest)?;
        #[cfg(not(unix))]
        anyhow::bail!("refusing to copy symlink {}", src.display());
        let _ = target;
        return Ok(());
    }
    if kind.is_dir() {
        return copy_tree_contents(src, dest);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dest)?;
    Ok(())
}

fn copy_tree_contents(src: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        copy_one(&entry.path(), &dest.join(entry.file_name()))?;
    }
    Ok(())
}

/// Read the `current.json` pointer, download its bundle, clone it, and copy
/// the tree (without `.git`) into `dest_dir`, creating it when missing.
pub async fn materialize(store: &Store, dest_dir: &Path) -> anyhow::Result<TemplateRef> {
    let bytes = store.get(CURRENT_KEY).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "no vendored template in the store; run `outer templates sync` \
             to vendor {ILHA_VOID_URL} first"
        )
    })?;
    let template: TemplateRef = serde_json::from_slice(&bytes)?;
    let tmp = tempfile::tempdir().context("template scratch dir")?;
    let bundle_path = tmp.path().join("template.bundle");
    if !store
        .get_to_file(&template.bundle_key, &bundle_path)
        .await?
    {
        anyhow::bail!(
            "template bundle {} is missing from the store; run `outer templates sync` again",
            template.bundle_key
        );
    }
    let bundle_str = bundle_path.to_string_lossy().into_owned();
    let src = tmp.path().join("src");
    let src_str = src.to_string_lossy().into_owned();
    run_git(
        &[
            "clone",
            "--no-checkout",
            bundle_str.as_str(),
            src_str.as_str(),
        ],
        None,
        &[],
    )
    .await?;
    run_git(
        &["-C", src_str.as_str(), "checkout", template.sha.as_str()],
        None,
        &[],
    )
    .await?;
    let head = run_git(&["-C", src_str.as_str(), "rev-parse", "HEAD"], None, &[])
        .await?
        .trim()
        .to_string();
    anyhow::ensure!(
        head == template.sha,
        "vendored bundle checked out {head}, want {}",
        template.sha
    );
    copy_tree(&src, dest_dir)?;
    Ok(template)
}

/// `dest_dir` joined with `rel`, for tests.
#[cfg(test)]
fn dest_path(dest_dir: &Path, rel: &str) -> std::path::PathBuf {
    dest_dir.join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHOR: [(&str, &str); 4] = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.com"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.com"),
    ];

    /// A local repo with two commits acting as the template "source".
    async fn make_source_repo() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        run_git(&["init"], Some(dir.path()), &[]).await.unwrap();
        std::fs::write(dir.path().join("README.md"), "# ilha-void\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("app.ts"), "export {};\n").unwrap();
        run_git(&["add", "-A"], Some(dir.path()), &[])
            .await
            .unwrap();
        run_git(&["commit", "-m", "seed"], Some(dir.path()), &AUTHOR)
            .await
            .unwrap();
        std::fs::write(dir.path().join("src").join("extra.ts"), "export {};\n").unwrap();
        run_git(&["add", "-A"], Some(dir.path()), &[])
            .await
            .unwrap();
        run_git(&["commit", "-m", "more"], Some(dir.path()), &AUTHOR)
            .await
            .unwrap();
        let sha = run_git(&["rev-parse", "HEAD"], Some(dir.path()), &[])
            .await
            .unwrap()
            .trim()
            .to_string();
        let url = format!("file://{root}");
        (dir, url, sha)
    }

    #[tokio::test]
    async fn sync_then_materialize_round_trips() {
        let (_repo, url, sha) = make_source_repo().await;
        let store = Store::memory();
        let first = sync(&store, &url, &sha).await.unwrap();
        assert_eq!(first.sha, sha);
        assert_eq!(
            first.bundle_key,
            format!("outer/templates/ilha-void/{sha}.bundle")
        );
        assert_eq!(first.source, url);
        assert!(store.get(&first.bundle_key).await.unwrap().is_some());
        let pointer: serde_json::Value =
            serde_json::from_slice(&store.get(CURRENT_KEY).await.unwrap().unwrap()).unwrap();
        assert_eq!(pointer["sha"], serde_json::Value::String(sha.clone()));
        // Idempotent: a second sync vendors the same pointer.
        let second = sync(&store, &url, &sha).await.unwrap();
        assert_eq!(first, second);
        let dest = tempfile::tempdir().unwrap();
        let out = dest.path().join("app");
        let back = materialize(&store, &out).await.unwrap();
        assert_eq!(back, first);
        assert_eq!(
            std::fs::read_to_string(dest_path(&out, "README.md")).unwrap(),
            "# ilha-void\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest_path(&out, "src/extra.ts")).unwrap(),
            "export {};\n"
        );
        assert!(!out.join(".git").exists(), "materialized tree has no .git");
    }

    #[tokio::test]
    async fn sync_refuses_a_missing_sha() {
        let (_repo, url, _sha) = make_source_repo().await;
        let store = Store::memory();
        let err = sync(&store, &url, &"0".repeat(40)).await.unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn materialize_without_a_sync_names_the_command() {
        let store = Store::memory();
        let dest = tempfile::tempdir().unwrap();
        let err = materialize(&store, dest.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("outer templates sync"),
            "operator-facing error must name the fix, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn sync_refuses_abbreviated_shas_and_branch_names() {
        let (_repo, url, sha) = make_source_repo().await;
        let store = Store::memory();
        // The default branch exists and the short sha resolves, but neither
        // may be stored: `materialize` compares against full-length HEAD.
        for bad in [
            sha[..12].to_string(),
            "main".to_string(),
            "HEAD".to_string(),
        ] {
            let err = sync(&store, &url, &bad).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("full 40-character lowercase commit sha"),
                "unexpected error for {bad:?}: {err:#}"
            );
        }
        assert!(
            store.get(CURRENT_KEY).await.unwrap().is_none(),
            "a refused sync stores no pointer"
        );
    }
}
