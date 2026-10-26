//! The vendored starter templates (spec §16): the interactive `app` (Ilha +
//! Void) and the content-site `website` (Astro + Ilha islands).
//!
//! Both live in one repo, `ryuzcorp/outer-templates`, as `templates/app/` and
//! `templates/website/`. `outer templates sync` pins that repo at one sha into
//! the store as a git bundle plus a `current.json` pointer; app creation
//! [`materialize`]s one template's folder into a fresh directory. No network
//! is needed past the sync.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Where the templates live upstream: one folder per template under
/// `templates/`.
pub const TEMPLATES_URL: &str = "https://github.com/ryuzcorp/outer-templates";
/// The pinned commit `outer templates sync` vendors. Repinning is a
/// one-line change, then `outer templates sync`.
pub const TEMPLATES_SHA: &str = "1ae125f38bc58c1fbe3cfa8925990a761ed092a6";

/// The store pointer to the vendored bundle.
const CURRENT_KEY: &str = "outer/templates/current.json";

/// Which starter template an app is created from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// Interactive Ilha + Void app (`templates/app`).
    App,
    /// Astro + Ilha-islands content site (`templates/website`).
    Website,
}

impl Template {
    /// Every template the vendored repo must carry.
    pub const ALL: [Template; 2] = [Template::App, Template::Website];

    /// The `create_app`/`--template` id.
    pub fn id(self) -> &'static str {
        match self {
            Template::App => "app",
            Template::Website => "website",
        }
    }

    /// Parse a `create_app`/`--template` id, else `None`.
    pub fn from_id(id: &str) -> Option<Template> {
        match id {
            "app" => Some(Template::App),
            "website" => Some(Template::Website),
            _ => None,
        }
    }

    /// This template's folder in the templates repo.
    pub fn dir(self) -> &'static str {
        match self {
            Template::App => "templates/app",
            Template::Website => "templates/website",
        }
    }

    /// The `Start from …` seed commit message for this template.
    pub fn seed_message(self, sha: &str) -> String {
        let short = &sha[..12.min(sha.len())];
        match self {
            Template::App => format!("Start from the Ilha + Void template ({short})"),
            Template::Website => format!("Start from the Astro website template ({short})"),
        }
    }
}

/// Write (overwriting) the fresh tree's `brand.json` for a new app
/// (BRANDING.md Contract 1): `name` defaults to the slug title-cased
/// (`kalkulator-pizzy` → `Kalkulator Pizzy`), `description` defaults to
/// `name`, and `accent` is deterministic per slug (a purplish-blue band:
/// hue 230 + FNV-1a(slug) mod 60). Creation paths call this
/// right after [`materialize`], before the first commit, so the seed commit
/// carries the brand. It overwrites the template's own `brand.json`.
pub fn write_brand(
    tree: &Path,
    slug: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> anyhow::Result<()> {
    let brand = crate::brand::resolve_brand(slug, name, description)?;
    crate::brand::write_brand_file(tree, &brand)
}

/// Key of the vendored bundle for `sha` in the store.
fn bundle_key(sha: &str) -> String {
    format!("outer/templates/{sha}.bundle")
}

/// Per-step git timeout: a full clone of the small templates repo takes seconds.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

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

/// Clone `url`, check out exactly `sha` (failing when it is missing or lacks
/// any template folder), bundle that commit, and upload the bundle plus the
/// `current.json` pointer. Re-running for the same sha overwrites the same
/// keys: idempotent.
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
    // Every template `create_app` offers must exist at this commit, or a
    // create would fail long after the sync said it was fine.
    for template in Template::ALL {
        let tree = format!("{sha}:{}", template.dir());
        // `-t` and not `-e`: a file or symlink at the path exists too, but
        // only a tree can be copied as the app's project root.
        let kind = run_git(
            &["-C", src_str.as_str(), "cat-file", "-t", tree.as_str()],
            None,
            &[],
        )
        .await
        .unwrap_or_default();
        anyhow::ensure!(
            kind.trim() == "tree",
            "{url} at {sha} has no {}/ folder for the {} template",
            template.dir(),
            template.id()
        );
    }
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
    let bundle_path = tmp.path().join("templates.bundle");
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
    let reference = TemplateRef {
        sha: sha.to_string(),
        bundle_key: key,
        source: url.to_string(),
    };
    store
        .put(CURRENT_KEY, serde_json::to_vec(&reference)?)
        .await?;
    Ok(reference)
}

/// Vendor the pinned templates repo: the flag-less `outer templates sync`.
pub async fn sync_pinned(store: &Store) -> anyhow::Result<TemplateRef> {
    sync(store, TEMPLATES_URL, TEMPLATES_SHA).await
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

/// Read the `current.json` pointer, download the bundle, clone it, and copy
/// `template`'s folder (without `.git`) into `dest_dir`, creating it when
/// missing.
pub async fn materialize(
    store: &Store,
    template: Template,
    dest_dir: &Path,
) -> anyhow::Result<TemplateRef> {
    let bytes = store.get(CURRENT_KEY).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "no vendored templates in the store (wanted the {} template); run \
             `outer templates sync` to vendor {TEMPLATES_URL} first",
            template.id(),
        )
    })?;
    let reference: TemplateRef = serde_json::from_slice(&bytes)?;
    let tmp = tempfile::tempdir().context("template scratch dir")?;
    let bundle_path = tmp.path().join("templates.bundle");
    if !store
        .get_to_file(&reference.bundle_key, &bundle_path)
        .await?
    {
        anyhow::bail!(
            "template bundle {} is missing from the store; run `outer templates sync` again",
            reference.bundle_key
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
        &["-C", src_str.as_str(), "checkout", reference.sha.as_str()],
        None,
        &[],
    )
    .await?;
    let head = run_git(&["-C", src_str.as_str(), "rev-parse", "HEAD"], None, &[])
        .await?
        .trim()
        .to_string();
    anyhow::ensure!(
        head == reference.sha,
        "vendored bundle checked out {head}, want {}",
        reference.sha
    );
    let folder = src.join(template.dir());
    anyhow::ensure!(
        folder.is_dir(),
        "vendored templates at {} have no {}/ folder; repin and run `outer templates sync`",
        reference.sha,
        template.dir()
    );
    copy_tree(&folder, dest_dir)?;
    Ok(reference)
}

/// Vendor a tiny templates repo into `store` for tests: `files` are written
/// into every template folder, committed once, bundled, and pointed at by
/// `current.json`, so [`materialize`] works without a network `sync`.
#[cfg(test)]
pub(crate) async fn vendor_test_templates(store: &Store, files: &[(&str, &str)]) -> TemplateRef {
    let src = tempfile::tempdir().unwrap();
    for template in Template::ALL {
        for (rel, body) in files {
            let path = src.path().join(template.dir()).join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }
    let author = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.com"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.com"),
    ];
    run_git(&["init", "-b", "main"], Some(src.path()), &[])
        .await
        .unwrap();
    run_git(&["add", "-A"], Some(src.path()), &[])
        .await
        .unwrap();
    run_git(&["commit", "-m", "seed"], Some(src.path()), &author)
        .await
        .unwrap();
    let sha = run_git(&["rev-parse", "HEAD"], Some(src.path()), &[])
        .await
        .unwrap()
        .trim()
        .to_string();
    let bundle = src.path().join("templates.bundle");
    let bundle_str = bundle.to_string_lossy().into_owned();
    run_git(
        &["bundle", "create", bundle_str.as_str(), "main"],
        Some(src.path()),
        &[],
    )
    .await
    .unwrap();
    let key = bundle_key(&sha);
    store.put_file(&key, &bundle).await.unwrap();
    let reference = TemplateRef {
        sha,
        bundle_key: key,
        source: "test".to_string(),
    };
    store
        .put(CURRENT_KEY, serde_json::to_vec(&reference).unwrap())
        .await
        .unwrap();
    reference
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

    async fn commit_all(dir: &Path, message: &str) -> String {
        run_git(&["add", "-A"], Some(dir), &[]).await.unwrap();
        run_git(&["commit", "-m", message], Some(dir), &AUTHOR)
            .await
            .unwrap();
        run_git(&["rev-parse", "HEAD"], Some(dir), &[])
            .await
            .unwrap()
            .trim()
            .to_string()
    }

    /// A local templates repo: an `app` and a `website` folder plus a
    /// root README that must never reach an app. Returns its url and sha.
    async fn make_source_repo() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        run_git(&["init"], Some(dir.path()), &[]).await.unwrap();
        std::fs::write(dir.path().join("README.md"), "# templates\n").unwrap();
        let app = dir.path().join("templates/app/src");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(dir.path().join("templates/app/README.md"), "# app\n").unwrap();
        std::fs::write(app.join("app.ts"), "export {};\n").unwrap();
        let site = dir.path().join("templates/website/src/pages");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(
            dir.path().join("templates/website/README.md"),
            "# website\n",
        )
        .unwrap();
        std::fs::write(site.join("index.astro"), "<h1>site</h1>\n").unwrap();
        let sha = commit_all(dir.path(), "seed").await;
        let url = format!("file://{}", dir.path().to_string_lossy());
        (dir, url, sha)
    }

    #[tokio::test]
    async fn each_template_materializes_only_its_own_folder() {
        let (_repo, url, sha) = make_source_repo().await;
        let store = Store::memory();
        let synced = sync(&store, &url, &sha).await.unwrap();
        assert_eq!(synced.sha, sha);
        assert_eq!(synced.bundle_key, format!("outer/templates/{sha}.bundle"));
        // Idempotent: a second sync vendors the same pointer.
        assert_eq!(sync(&store, &url, &sha).await.unwrap(), synced);

        let app = tempfile::tempdir().unwrap();
        let back = materialize(&store, Template::App, app.path())
            .await
            .unwrap();
        assert_eq!(back, synced);
        assert_eq!(
            std::fs::read_to_string(app.path().join("README.md")).unwrap(),
            "# app\n"
        );
        assert!(app.path().join("src/app.ts").exists());
        assert!(
            !app.path().join("templates").exists(),
            "no repo layout leaks into the app"
        );
        assert!(
            !app.path().join("src/pages").exists(),
            "no website files in an app"
        );
        assert!(!app.path().join(".git").exists());

        let site = tempfile::tempdir().unwrap();
        materialize(&store, Template::Website, site.path())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(site.path().join("README.md")).unwrap(),
            "# website\n"
        );
        assert!(site.path().join("src/pages/index.astro").exists());
        assert!(
            !site.path().join("src/app.ts").exists(),
            "no app files in a website"
        );
    }

    #[tokio::test]
    async fn sync_refuses_a_commit_missing_a_template_folder() {
        let (repo, url, _) = make_source_repo().await;
        std::fs::remove_dir_all(repo.path().join("templates/website")).unwrap();
        let sha = commit_all(repo.path(), "drop website").await;
        let store = Store::memory();
        let err = sync(&store, &url, &sha).await.unwrap_err();
        assert!(
            err.to_string().contains("templates/website"),
            "the error names the missing folder: {err:#}"
        );
        assert!(
            store.get(CURRENT_KEY).await.unwrap().is_none(),
            "nothing vendored"
        );
    }

    #[tokio::test]
    async fn sync_refuses_a_file_where_a_template_folder_belongs() {
        let (repo, url, _) = make_source_repo().await;
        std::fs::remove_dir_all(repo.path().join("templates/website")).unwrap();
        std::fs::write(repo.path().join("templates/website"), "not a folder\n").unwrap();
        let sha = commit_all(repo.path(), "website is a file").await;
        let store = Store::memory();
        let err = sync(&store, &url, &sha).await.unwrap_err();
        assert!(
            err.to_string().contains("templates/website"),
            "the error names the folder: {err:#}"
        );
        assert!(
            store.get(CURRENT_KEY).await.unwrap().is_none(),
            "nothing vendored"
        );
    }

    #[tokio::test]
    async fn template_ids_round_trip() {
        assert_eq!(Template::from_id("app"), Some(Template::App));
        assert_eq!(Template::from_id("website"), Some(Template::Website));
        assert_eq!(Template::from_id("blog"), None);
        assert_eq!(Template::from_id(""), None);
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
        let err = materialize(&store, Template::Website, dest.path())
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("outer templates sync"),
            "names the fix: {message}"
        );
        assert!(message.contains("website"), "names the template: {message}");
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
