//! Copy-paste patterns served over the public `examples` MCP tool, and
//! applied by `add_example` / `create_app {examples}`.
//!
//! The patterns live in `examples/` in the `outer-templates` repo, vendored
//! into the store as a git bundle at the pinned sha (see [`crate::templates`]).
//! [`list`] and [`get`] extract only `examples/` into a cache dir under
//! Outer's data dir, once per sha, and serve from there. A pinned sha with no
//! `examples/` yields an empty list. A bad example is skipped with a warn and
//! never fails the whole list.
pub mod apply;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::store::Store;
use crate::templates::Template;

/// One file's listing entry: path plus its line count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileRef {
    pub path: String,
    pub lines: usize,
}

/// One row of the `examples` list: metadata plus the file listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub name: String,
    pub title: String,
    pub summary: String,
    pub template: String,
    pub files: Vec<FileRef>,
}

/// One full example: the README plus every file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleFile {
    pub path: String,
    pub text: String,
}

/// One full example: the README plus every file's text, plus the apply
/// metadata `add_example` plans from (`requires`, `nav`, `bindings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavEntry {
    pub href: String,
    pub label: String,
    pub icon: String,
}

/// One full example: the README, every file's text, and the apply metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    pub name: String,
    pub title: String,
    pub summary: String,
    pub template: String,
    pub requires: Vec<String>,
    pub nav: Vec<NavEntry>,
    pub bindings: BTreeMap<String, Vec<BTreeMap<String, String>>>,
    pub readme: String,
    pub files: Vec<ExampleFile>,
}

/// An example dir name: `^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$`.
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 || bytes.len() > 40 {
        return false;
    }
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !edge(bytes[0]) || !edge(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The `example.json` contract: title, summary, template, plus optional
/// `requires`, `nav` and `bindings`. `dependencies` is not supported and
/// rejects the example; unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct NavJson {
    href: String,
    label: String,
    icon: String,
}

#[derive(Debug, Deserialize)]
struct ExampleJson {
    title: String,
    summary: String,
    template: String,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    nav: Vec<NavJson>,
    #[serde(default)]
    bindings: BTreeMap<String, Vec<BTreeMap<String, String>>>,
    #[serde(default)]
    dependencies: Option<serde_json::Value>,
}

/// Largest single file an example may carry (contract: 64 KiB).
const MAX_EXAMPLE_FILE_BYTES: u64 = 64 * 1024;
/// Most files one example may carry (contract: 30).
const MAX_EXAMPLE_FILES: usize = 30;

/// Marker written after a sha's cache dir is fully extracted.
const READY: &str = "READY";
/// How long a racing caller waits for the winner's `READY`.
const EXTRACT_WAIT: Duration = Duration::from_secs(30);
/// A non-ready cache dir older than this is a crashed extraction: take over.
const STALE_AFTER: Duration = Duration::from_secs(600);

/// The cache root: one dir per vendored sha under Outer's data dir.
fn cache_root(data_dir: &Path) -> PathBuf {
    data_dir.join("examples")
}

/// List the valid examples at the current pin, optionally filtered to one
/// template. Sorted by name. No pin, or a pin with no `examples/`, is an
/// empty list.
pub async fn list(
    store: &Store,
    data_dir: &Path,
    template: Option<Template>,
) -> anyhow::Result<Vec<Summary>> {
    let dir = match ensure_extracted(store, data_dir).await? {
        None => return Ok(Vec::new()),
        Some(dir) => dir,
    };
    let id = template.map(|t| t.id());
    Ok(load_examples(&dir)
        .into_iter()
        .filter(|example| id.is_none_or(|id| example.template == id))
        .map(|example| example.summary())
        .collect())
}

/// The full example `name`, or `None` when it names no valid example.
/// An invalid name is the caller's bug: refused before touching the fs.
pub async fn get(store: &Store, data_dir: &Path, name: &str) -> anyhow::Result<Option<Example>> {
    anyhow::ensure!(is_valid_name(name), "bad example name {name:?}");
    let dir = match ensure_extracted(store, data_dir).await? {
        None => return Ok(None),
        Some(dir) => dir,
    };
    Ok(load_example(&dir.join(name), name))
}

impl Example {
    fn summary(&self) -> Summary {
        Summary {
            name: self.name.clone(),
            title: self.title.clone(),
            summary: self.summary.clone(),
            template: self.template.clone(),
            files: self
                .files
                .iter()
                .map(|file| FileRef {
                    path: file.path.clone(),
                    lines: file.text.lines().count(),
                })
                .collect(),
        }
    }

    /// The apply engine's view of this example.
    pub fn apply_input(&self) -> apply::ExampleInput {
        apply::ExampleInput {
            name: self.name.clone(),
            template: self.template.clone(),
            requires: self.requires.clone(),
            nav: self
                .nav
                .iter()
                .map(|item| apply::NavItem {
                    href: item.href.clone(),
                    label: item.label.clone(),
                    icon: item.icon.clone(),
                })
                .collect(),
            bindings: self.bindings.clone(),
            files: self
                .files
                .iter()
                .map(|file| (file.path.clone(), file.text.clone()))
                .collect(),
        }
    }
}

/// Extract `examples/` for the current [`crate::templates::TemplateRef`]
/// into the cache, once per sha. Concurrent first calls race on one atomic
/// `create_dir`: the winner extracts, the losers wait for its `READY`.
/// `Ok(None)` means no pin (or nothing vendored yet): serve an empty list.
async fn ensure_extracted(store: &Store, data_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Some(reference) = crate::templates::current_reference(store).await? else {
        return Ok(None);
    };
    let root = cache_root(data_dir);
    std::fs::create_dir_all(&root)?;
    let target = root.join(&reference.sha);
    if is_ready(&target) {
        return Ok(Some(target));
    }
    if std::fs::create_dir(&target).is_ok() {
        return extract(store, &reference, &target)
            .await
            .map(|()| Some(target));
    }
    // Someone else is extracting (or crashed doing so).
    if wait_ready(&target, EXTRACT_WAIT).await {
        return Ok(Some(target));
    }
    // A crashed extraction leaves a stale dir with no `READY`: take over.
    if is_stale(&target) {
        let _ = std::fs::remove_dir_all(&target);
        if std::fs::create_dir(&target).is_ok() {
            return extract(store, &reference, &target)
                .await
                .map(|()| Some(target));
        }
        if wait_ready(&target, EXTRACT_WAIT).await {
            return Ok(Some(target));
        }
    }
    anyhow::bail!(
        "timed out waiting for the examples cache at {}",
        target.display()
    )
}

/// Clone the vendored bundle, check out the pin, and copy only `examples/`
/// into `target` (skipping symlinks: the contract forbids them). A pin with
/// no `examples/` leaves an empty `target`. A failure removes `target` so a
/// racing caller can take over.
async fn extract(
    store: &Store,
    reference: &crate::templates::TemplateRef,
    target: &Path,
) -> anyhow::Result<()> {
    let out = extract_inner(store, reference, target).await;
    if out.is_err() {
        let _ = std::fs::remove_dir_all(target);
    }
    out
}

async fn extract_inner(
    store: &Store,
    reference: &crate::templates::TemplateRef,
    target: &Path,
) -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let bundle_path = tmp.path().join("templates.bundle");
    if !store
        .get_to_file(&reference.bundle_key, &bundle_path)
        .await?
    {
        anyhow::bail!(
            "template bundle {} is missing from the store",
            reference.bundle_key
        );
    }
    let bundle_str = bundle_path.to_string_lossy().into_owned();
    let src = tmp.path().join("src");
    let src_str = src.to_string_lossy().into_owned();
    crate::templates::run_git(
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
    crate::templates::run_git(
        &["-C", src_str.as_str(), "checkout", reference.sha.as_str()],
        None,
        &[],
    )
    .await?;
    let head = crate::templates::run_git(&["-C", src_str.as_str(), "rev-parse", "HEAD"], None, &[])
        .await?;
    anyhow::ensure!(
        head.trim() == reference.sha,
        "vendored bundle checked out {}, want {}",
        head.trim(),
        reference.sha
    );
    let examples = src.join("examples");
    if examples.is_dir() {
        copy_examples(&examples, target)?;
    }
    std::fs::write(target.join(READY), reference.sha.as_bytes())?;
    Ok(())
}

/// Copy the extracted `examples/` tree, skipping symlinks (forbidden by the
/// contract) rather than reproducing them.
fn copy_examples(src: &Path, dest: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        let to = dest.join(entry.file_name());
        if kind.is_dir() {
            copy_examples_dir(&entry.path(), &to)?;
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn copy_examples_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    copy_examples(src, dest)
}

fn is_ready(dir: &Path) -> bool {
    dir.join(READY).is_file()
}

async fn wait_ready(dir: &Path, wait: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < wait {
        if is_ready(dir) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    is_ready(dir)
}

fn is_stale(dir: &Path) -> bool {
    if is_ready(dir) {
        return false;
    }
    match std::fs::metadata(dir).and_then(|meta| meta.modified()) {
        Ok(modified) => modified.elapsed().unwrap_or(Duration::ZERO) > STALE_AFTER,
        Err(_) => false,
    }
}

/// Every valid example in an extracted cache dir, sorted by name. A bad
/// example is skipped with a warn and never fails the whole list.
fn load_examples(dir: &Path) -> Vec<Example> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == READY || !is_valid_name(&name) {
            continue;
        }
        if let Some(example) = load_example(&path, &name) {
            out.push(example);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One valid example, or `None` (with a warn) when it breaks the contract.
fn load_example(dir: &Path, name: &str) -> Option<Example> {
    let reject = |why: &str| {
        tracing::warn!(example = name, "skipping invalid example: {why}");
        None
    };
    let raw = std::fs::read(dir.join("example.json")).ok()?;
    let meta: ExampleJson = match serde_json::from_slice(&raw) {
        Ok(meta) => meta,
        Err(err) => return reject(&format!("bad example.json: {err:#}")),
    };
    if crate::templates::Template::from_id(&meta.template).is_none() {
        return reject(&format!("bad template {:?}", meta.template));
    }
    if meta.dependencies.is_some() {
        return reject("example.json \"dependencies\" is not supported (examples ship files only; add_example never touches package.json)");
    }
    for dep in &meta.requires {
        if !is_valid_name(dep) {
            return reject(&format!("bad requires entry {dep:?}"));
        }
    }
    let mut nav = Vec::with_capacity(meta.nav.len());
    for item in meta.nav {
        if item.href.is_empty() || !item.href.starts_with('/') {
            return reject(&format!("bad nav href {:?}", item.href));
        }
        if item.label.is_empty() {
            return reject(&format!("bad nav label for {:?}", item.href));
        }
        if !apply::is_valid_icon(&item.icon) {
            return reject(&format!("bad nav icon {:?}", item.icon));
        }
        nav.push(NavEntry {
            href: item.href,
            label: item.label,
            icon: item.icon,
        });
    }
    for key in meta.bindings.keys() {
        if key != "d1_databases" && key != "r2_buckets" {
            return reject(&format!(
                "bad bindings key {key:?}: want d1_databases or r2_buckets"
            ));
        }
    }
    let readme = match std::fs::read_to_string(dir.join("README.md")) {
        Ok(readme) => readme,
        Err(_) => return reject("missing README.md"),
    };
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    if files.len() > MAX_EXAMPLE_FILES {
        return reject(&format!(
            "{} files, at most {MAX_EXAMPLE_FILES}",
            files.len()
        ));
    }
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let full = dir.join(&path);
        let size = std::fs::metadata(&full).ok()?.len();
        if size > MAX_EXAMPLE_FILE_BYTES {
            return reject(&format!(
                "{path} is {size} bytes, at most {MAX_EXAMPLE_FILE_BYTES}"
            ));
        }
        match std::fs::read_to_string(&full) {
            Ok(text) => out.push(ExampleFile { path, text }),
            Err(_) => return reject(&format!("{path} is not UTF-8")),
        }
    }
    Some(Example {
        name: name.to_string(),
        title: meta.title,
        summary: meta.summary,
        template: meta.template,
        requires: meta.requires,
        nav,
        bindings: meta.bindings,
        readme,
        files: out,
    })
}

/// Every regular file under an example dir as an app-relative path, sorted.
/// `example.json` and `README.md` are metadata, not app files. Symlinks are
/// skipped (forbidden by the contract).
fn collect_files(dir: &Path, base: &Path, out: &mut Vec<String>) -> Option<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            tracing::warn!(
                file = %entry.path().display(),
                "skipping symlinked example file"
            );
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, base, out)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(base).ok()?.to_string_lossy().into_owned();
            if rel != "example.json" && rel != "README.md" {
                out.push(rel);
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::examples::{examples as examples_tool, ExamplesInput};
    use crate::mcp::tools::Ctx;
    use crate::mcp::McpState;

    /// A live state for tool calls: memory store, temp dirs, no network.
    async fn tool_state(store: Store) -> McpState {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        let (auth_cfg, _) = crate::auth::test_support::test_config();
        let auth_state = crate::auth::AuthState {
            pool: pool.clone(),
            config: auth_cfg,
        };
        let mut state = McpState::for_tests(pool, auth_state);
        state.store = store;
        state
    }

    /// A templates repo with both starter folders plus `examples/`: each
    /// entry is `(example name, [(app-relative path, body)])`, and each
    /// symlink is `(example, link path, target)`.
    async fn fixture(
        store: &Store,
        examples: &[(&str, &[(&str, &str)])],
        symlinks: &[(&str, &str, &str)],
    ) -> String {
        let src = tempfile::tempdir().unwrap();
        for template in Template::ALL {
            let dir = src.path().join(template.dir());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("package.json"), "{}\n").unwrap();
        }
        for (name, files) in examples {
            let dir = src.path().join("examples").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for (rel, body) in *files {
                let path = dir.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
            }
        }
        for (name, link, target) in symlinks {
            let path = src.path().join("examples").join(name).join(link);
            std::os::unix::fs::symlink(target, &path).unwrap();
        }
        let author = [
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@example.com"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@example.com"),
        ];
        crate::templates::run_git(&["init", "-b", "main"], Some(src.path()), &[])
            .await
            .unwrap();
        crate::templates::run_git(&["add", "-A"], Some(src.path()), &[])
            .await
            .unwrap();
        crate::templates::run_git(&["commit", "-m", "seed"], Some(src.path()), &author)
            .await
            .unwrap();
        let sha = crate::templates::run_git(&["rev-parse", "HEAD"], Some(src.path()), &[])
            .await
            .unwrap()
            .trim()
            .to_string();
        let url = format!("file://{}", src.path().to_string_lossy());
        crate::templates::sync(store, &url, &sha).await.unwrap();
        sha
    }

    fn example_json(template: &str, title: &str, summary: &str) -> String {
        serde_json::json!({
            "name": "ignored-the-dir-name-wins",
            "title": title,
            "summary": summary,
            "template": template,
        })
        .to_string()
    }

    async fn two_example_state() -> McpState {
        let store = Store::memory();
        let json = example_json(
            "app",
            "List form",
            "A D1-backed list with add and complete.",
        );
        let blog_json = example_json("website", "Blog", "Content-collection posts.");
        fixture(
            &store,
            &[
                (
                    "list-form",
                    &[
                        ("example.json", json.as_str()),
                        ("README.md", "# List form\n\nCopy these files.\n"),
                        ("pages/list.tsx", "export default function List() {}\n"),
                        ("pages/list.server.ts", "export function load() {}\n"),
                    ],
                ),
                (
                    "blog",
                    &[
                        ("example.json", blog_json.as_str()),
                        ("README.md", "# Blog\n\nCopy these files.\n"),
                        ("src/content/hello.md", "# Hello\n"),
                    ],
                ),
            ],
            &[],
        )
        .await;
        tool_state(store).await
    }

    #[test]
    fn example_names_follow_the_contract() {
        assert!(is_valid_name("list-form"));
        assert!(is_valid_name("a0"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("a"));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("../x"));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name(&"a".repeat(41)));
        assert!(is_valid_name(&format!("a{}b", "c".repeat(38))));
    }

    #[tokio::test]
    async fn list_returns_valid_examples_filtered_by_template() {
        let state = two_example_state().await;
        let dir = &state.delete_cfg.data_dir;
        let all = list(&state.store, dir, None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "blog");
        assert_eq!(all[1].name, "list-form");
        assert_eq!(all[1].title, "List form");
        assert_eq!(all[1].summary, "A D1-backed list with add and complete.");
        assert_eq!(all[1].template, "app");
        let paths: Vec<_> = all[1].files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["pages/list.server.ts", "pages/list.tsx"]);
        assert!(all[1].files.iter().all(|file| file.lines > 0));
        let app_only = list(&state.store, dir, Some(Template::App)).await.unwrap();
        assert_eq!(
            app_only
                .iter()
                .map(|example| example.name.as_str())
                .collect::<Vec<_>>(),
            ["list-form"]
        );
        let site_only = list(&state.store, dir, Some(Template::Website))
            .await
            .unwrap();
        assert_eq!(
            site_only
                .iter()
                .map(|example| example.name.as_str())
                .collect::<Vec<_>>(),
            ["blog"]
        );
    }

    #[tokio::test]
    async fn an_example_carrying_dependencies_is_skipped() {
        let store = Store::memory();
        let json = serde_json::json!({
            "name": "need",
            "title": "Need",
            "summary": "Wants a package.",
            "template": "app",
            "dependencies": {"some-pkg": "^1.0.0"},
        })
        .to_string();
        fixture(
            &store,
            &[(
                "need",
                &[
                    ("example.json", json.as_str()),
                    ("README.md", "# Need\n"),
                    ("pages/need.ts", "export const need = 1;\n"),
                ],
            )],
            &[],
        )
        .await;
        let state = tool_state(store).await;
        let dir = &state.delete_cfg.data_dir;
        assert!(list(&state.store, dir, None).await.unwrap().is_empty());
        assert!(get(&state.store, dir, "need").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_returns_the_readme_and_files() {
        let state = two_example_state().await;
        let example = get(&state.store, &state.delete_cfg.data_dir, "list-form")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(example.readme, "# List form\n\nCopy these files.\n");
        let paths: Vec<_> = example
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["pages/list.server.ts", "pages/list.tsx"]);
        assert!(example
            .files
            .iter()
            .any(|file| file.path == "pages/list.tsx"
                && file.text == "export default function List() {}\n"));
    }

    #[tokio::test]
    async fn unknown_name_lists_the_valid_names() {
        let state = two_example_state().await;
        let ctx = Ctx {
            state: &state,
            principal: None,
            client_ip: None,
        };
        let err = examples_tool(
            ExamplesInput {
                name: Some("nope".to_string()),
                template: None,
                files: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_argument");
        assert!(
            err.message.contains("nope"),
            "names the unknown: {}",
            err.message
        );
        assert!(
            err.message.contains("list-form") && err.message.contains("blog"),
            "lists the valid: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn invalid_names_are_refused_before_touching_the_store() {
        // The store was never vendored: anything past the name check would
        // fail as `internal`, not `invalid_argument`.
        let state = tool_state(Store::memory()).await;
        let ctx = Ctx {
            state: &state,
            principal: None,
            client_ip: None,
        };
        for bad in ["../x".to_string(), "Upper".to_string(), "a".repeat(41)] {
            let err = examples_tool(
                ExamplesInput {
                    name: Some(bad.clone()),
                    template: None,
                    files: None,
                },
                &ctx,
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, "invalid_argument", "for {bad:?}");
        }
        // A well-formed but unknown name on an empty pin stays `invalid_argument`.
        let err = examples_tool(
            ExamplesInput {
                name: Some("list-form".to_string()),
                template: None,
                files: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_argument");
        assert!(err.message.contains("pick one of:"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_symlinked_file_is_skipped() {
        let store = Store::memory();
        let json = example_json("app", "List form", "A D1-backed list.");
        fixture(
            &store,
            &[(
                "list-form",
                &[
                    ("example.json", json.as_str()),
                    ("README.md", "# List form\n"),
                    ("pages/list.tsx", "export default function List() {}\n"),
                ],
            )],
            &[("list-form", "pages/evil.tsx", "../list.server.ts")],
        )
        .await;
        let state = tool_state(store).await;
        let example = get(&state.store, &state.delete_cfg.data_dir, "list-form")
            .await
            .unwrap()
            .unwrap();
        let paths: Vec<_> = example
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["pages/list.tsx"]);
    }

    #[tokio::test]
    async fn the_output_budget_truncates() {
        let store = Store::memory();
        let big = "x".repeat(60 * 1024);
        let json = example_json("app", "Big", "Four files past the budget.");
        fixture(
            &store,
            &[(
                "big",
                &[
                    ("example.json", json.as_str()),
                    ("README.md", "# Big\n"),
                    ("a.txt", big.as_str()),
                    ("b.txt", big.as_str()),
                    ("c.txt", big.as_str()),
                    ("d.txt", big.as_str()),
                ],
            )],
            &[],
        )
        .await;
        let state = tool_state(store).await;
        let ctx = Ctx {
            state: &state,
            principal: None,
            client_ip: None,
        };
        let value = examples_tool(
            ExamplesInput {
                name: Some("big".to_string()),
                template: None,
                files: Some(true),
            },
            &ctx,
        )
        .await
        .unwrap();
        let structured = &value["structuredContent"];
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["files"].as_array().unwrap().len(), 4);
        assert_eq!(structured["files"][3]["truncated"], true);
        let text = value["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("truncated at the 200 KB output budget"),
            "{text}"
        );
        assert!(text.starts_with("# Big\n"), "{text}");
        // Contents travel in the text block only, never duplicated.
        assert!(
            !text.contains("\"text\""),
            "no JSON envelope in the text block"
        );
        for entry in structured["files"].as_array().unwrap() {
            assert!(entry.get("text").is_none(), "no contents in structured");
        }
    }

    #[tokio::test]
    async fn without_files_lists_without_contents() {
        let state = two_example_state().await;
        let ctx = Ctx {
            state: &state,
            principal: None,
            client_ip: None,
        };
        let value = examples_tool(
            ExamplesInput {
                name: Some("list-form".to_string()),
                template: None,
                files: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        let text = value["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < 6 * 1024, "stays small: {}", text.len());
        assert!(text.contains("pages/list.tsx"), "{text}");
        assert!(
            !text.contains("export default function List"),
            "no file contents: {text}"
        );
        assert!(text.contains("add_example"), "{text}");
        let structured = &value["structuredContent"];
        assert_eq!(structured["name"], "list-form");
        assert_eq!(structured["requires"].as_array().unwrap().len(), 0);
        assert_eq!(structured["files"].as_array().unwrap().len(), 2);
        assert!(structured.get("readme").is_some());
        for entry in structured["files"].as_array().unwrap() {
            assert!(entry.get("text").is_none(), "no contents in structured");
        }
    }

    #[tokio::test]
    async fn a_pin_without_examples_gives_an_empty_list() {
        let store = Store::memory();
        crate::templates::vendor_test_templates(&store, &[("package.json", "{}\n")]).await;
        let state = tool_state(store).await;
        let dir = &state.delete_cfg.data_dir;
        assert!(list(&state.store, dir, None).await.unwrap().is_empty());
        assert!(get(&state.store, dir, "list-form").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_first_calls_extract_once() {
        let store = Store::memory();
        let json = example_json("app", "List form", "A D1-backed list.");
        let sha = fixture(
            &store,
            &[(
                "list-form",
                &[
                    ("example.json", json.as_str()),
                    ("README.md", "# List form\n"),
                    ("pages/list.tsx", "export default function List() {}\n"),
                ],
            )],
            &[],
        )
        .await;
        let state = tool_state(store).await;
        let dir = state.delete_cfg.data_dir.clone();
        let (first, second) = tokio::join!(
            list(&state.store, &dir, None),
            list(&state.store, &dir, None)
        );
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        let mut entries: Vec<_> = std::fs::read_dir(dir.join("examples"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().to_string_lossy(), sha.as_str());
        assert!(entries[0].path().join("READY").is_file());
    }
}
