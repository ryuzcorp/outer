//! Per-deploy Worker source maps for error symbolication.
//!
//! The deploy passes `--sourcemap <path>` to `celld deploy` (the fork writes
//! the bundled Worker's external map, or the `no_bundle` entry's sibling
//! map, with project-root-relative sources; the deployed bytes are identical
//! with or without it). What celld wrote lands under
//! `maps/<slug>/<sha>/worker.js.map` next to a `manifest.json`, so app
//! delete/rename carry the maps with no extra path, and retention follows
//! the deploy rows: after a successful deploy, sha dirs outside the recent
//! history are pruned.
//!
//! Grouping is untouched: fingerprints still hash the raw frames, so an
//! issue may span deploys whose minified names shifted — each frame maps
//! against the newest successful deploy at or before the frame's timestamp
//! at read time, and anything without a map renders raw. Symbolication is
//! best-effort by contract: a missing manifest, a bad map, or an unknown
//! deploy leaves the raw positions the flush stored — the read never fails
//! for symbolication.
//!
//! Only the Worker bundle maps: workerd reports the entry bare
//! (`worker.js`, the fork's `index.js` module name), so only frames naming
//! exactly those files are attempted. Python `File ...` frames, `node:`
//! internals, and unknown bare names never reach a map: the token lookup is
//! positional, so anything else would map to a wrong position.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// Maps live beside fleet state, one sha dir per deploy.
pub const MAPS_DIR: &str = "maps";
/// `maps/<slug>/<sha>/manifest.json`: which map belongs to the bundle.
const MANIFEST_FILE: &str = "manifest.json";
/// `maps/<slug>/<sha>/worker.js.map`: the Worker bundle's map.
pub const WORKER_MAP_FILE: &str = "worker.js.map";
/// One map file is telemetry-scale, not bundle-scale: refuse anything bigger
/// (a corrupt or hostile map must not blow the read path's memory). Matches
/// the fork's own 64 MiB ceiling from below: celld never writes more.
pub const MAX_MAP_BYTES: u64 = 8 * 1024 * 1024;
/// Deploy sha dirs kept per app besides the just-finished one. Occurrences
/// older than this still render — with their raw, unsymbolicated stacks.
pub const KEEP_DEPLOY_SHAS: usize = 10;

/// The Worker bundle's report names: upstream celld serves the entry as
/// `worker.js`, the fork as its `index.js` module name — workerd prints the
/// entry bare in both cases, never an app path. Only these two names map:
/// the token lookup is purely positional (greatest-lower-bound), so any
/// other bare frame (`node:events:1:1`, a stray `events:1:1`) would map to
/// an unrelated app position instead of missing. Unknown names render raw.
const BUNDLE_FILES: &[&str] = &["worker.js", "index.js"];

fn is_bundle_file(file: &str) -> bool {
    BUNDLE_FILES.contains(&file)
}

/// What `maps/<slug>/<sha>/manifest.json` records: the Worker bundle's map,
/// relative to the sha dir.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MapManifest {
    /// Stored map path for the Worker bundle, relative to the sha dir.
    #[serde(default)]
    pub worker: Option<String>,
}

/// The maps root for one app. Slugs are `[a-z0-9-]`, so the join cannot
/// escape `MAPS_DIR`; a crafted slug never reaches this from outside the
/// deploy path, which takes it from the app row.
pub fn app_maps_dir(data_dir: &Path, slug: &str) -> PathBuf {
    data_dir.join(MAPS_DIR).join(slug)
}

/// Deploy shas are git shas; a sha that is not hex would escape the maps dir.
pub fn is_key_sha(sha: &str) -> bool {
    !sha.is_empty() && sha.len() <= 64 && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// Newest-first deploy shas for one app, for the read-time join of a frame
/// timestamp to its deploy's maps: every successful deploy, plus the live
/// one when it is not successful. An upload-first deploy whose migrate step
/// fails still serves the new version (`last_deploy_sha` moves exactly as
/// on success while the row stays `failed`), so excluding it would map the
/// live version's frames against the previous deploy's map — wrong
/// positions, not just raw ones.
pub async fn deploy_shas(pool: &SqlitePool, app_id: &str) -> Vec<(String, i64)> {
    let mut shas: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "SELECT sha, created_at FROM deploy WHERE app_id = ?1 AND status = 'success' \
         ORDER BY created_at DESC, sha DESC LIMIT ?2",
    )
    .bind(app_id)
    .bind((KEEP_DEPLOY_SHAS + 1) as i64)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let live: Option<(String, i64)> = sqlx::query_as(
        "SELECT d.sha, d.created_at FROM deploy d JOIN app a ON a.id = d.app_id \
         WHERE d.app_id = ?1 AND d.sha = a.last_deploy_sha AND d.status != 'success' \
         ORDER BY d.created_at DESC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some((sha, created_at)) = live {
        if !shas.iter().any(|(s, _)| s == &sha) {
            shas.push((sha, created_at));
            shas.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        }
    }
    shas
}

/// The newest successful deploy at or before `ts_secs` (unix seconds, the
/// `app_log` unit; `created_at` is unix millis). `None` means no deploy
/// predates the frame, so it renders raw.
pub fn sha_for_ts(shas: &[(String, i64)], ts_secs: i64) -> Option<&str> {
    shas.iter()
        .filter(|(_, created_at)| created_at.saturating_div(1000) <= ts_secs)
        .max_by_key(|(_, created_at)| *created_at)
        .map(|(sha, _)| sha.as_str())
}

/// What the deploy's temp map path held when the upload finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapRead {
    /// Nothing there: celld had nothing to map (asset-only or Python
    /// projects, and entries without a sibling map — celld warns and writes
    /// nothing), or the file vanished. Not an error: there is simply no map.
    Absent,
    /// A map, but over [`MAX_MAP_BYTES`]: refused before parsing, so a
    /// corrupt or hostile map cannot blow the read path's memory.
    OverCap,
    /// Map bytes to validate and store.
    Found(Vec<u8>),
}

/// Read one temp map the deploy step passed `--sourcemap`: the worktree
/// belongs to the app uid, which can replace the path while celld runs, so
/// the open refuses symlinks (`O_NOFOLLOW`), refuses non-regular files
/// (FIFOs would block the deploy; directories, sockets and devices are not
/// maps), and reads at most [`MAX_MAP_BYTES`] + 1 (over-cap surfaces as
/// [`MapRead::OverCap` instead of truncating into a corrupt map). The
/// validation and the open cannot be pried apart by a path swap: a single
/// `O_NOFOLLOW` open plus an `fstat` file-type check, no name re-lookup.
pub async fn read_map_file(path: &Path) -> MapRead {
    use std::os::unix::fs::OpenOptionsExt;
    let open = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match open {
        Ok(file) => file,
        Err(_) => return MapRead::Absent,
    };
    // `fstat`, not a name re-lookup: what was opened is what is checked, so
    // a path swap between the check and the read cannot smuggle a FIFO or
    // a symlink past. `O_NOFOLLOW` already refused symlinks at open.
    let is_regular = match file.metadata() {
        Ok(meta) => meta.file_type().is_file(),
        Err(_) => return MapRead::Absent,
    };
    if !is_regular {
        return MapRead::Absent;
    }
    let mut buf = Vec::new();
    // `O_NONBLOCK` on a regular file reads normally; on a FIFO it errors
    // instead of blocking the deploy, which lands here as absent. One byte
    // past the cap distinguishes over-cap from exactly-at-cap without ever
    // holding more.
    use tokio::io::AsyncReadExt as _;
    let capped = match tokio::fs::File::from_std(file)
        .take(MAX_MAP_BYTES + 1)
        .read_to_end(&mut buf)
        .await
    {
        Ok(n) => n as u64,
        Err(_) => return MapRead::Absent,
    };
    if buf.is_empty() {
        return MapRead::Absent;
    }
    if capped > MAX_MAP_BYTES {
        return MapRead::OverCap;
    }
    MapRead::Found(buf)
}

/// Store one deploy's Worker map: validates the sha and the size, writes
/// `worker.js.map` plus the manifest, then prunes sha dirs outside `keep`.
/// Never fails the deploy: every refusal returns a note for the journal,
/// and the deploy already succeeded.
pub async fn store_worker_map(
    data_dir: &Path,
    slug: &str,
    sha: &str,
    read: MapRead,
    keep: &[(String, i64)],
) -> String {
    let bytes = match read {
        MapRead::Absent => {
            return "sourcemaps: no worker map (celld had nothing to map)".to_string();
        }
        MapRead::OverCap => {
            return "sourcemaps: skipped storing the worker map (over the size cap)".to_string();
        }
        MapRead::Found(bytes) => bytes,
    };
    if !is_key_sha(sha) {
        return "sourcemaps: skipped storing the worker map (bad deploy sha)".to_string();
    }
    if bytes.len() as u64 > MAX_MAP_BYTES {
        return "sourcemaps: skipped storing the worker map (over the size cap)".to_string();
    }
    if sourcemap::SourceMap::from_slice(&bytes).is_err() {
        return "sourcemaps: skipped storing the worker map (not a v3 source map)".to_string();
    }
    let dir = app_maps_dir(data_dir, slug).join(sha);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return format!("sourcemaps: skipped storing the worker map ({e})");
    }
    let manifest = MapManifest {
        worker: Some(WORKER_MAP_FILE.to_string()),
    };
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap_or_default();
    if tokio::fs::write(dir.join(WORKER_MAP_FILE), &bytes)
        .await
        .is_err()
        || tokio::fs::write(dir.join(MANIFEST_FILE), &manifest_bytes)
            .await
            .is_err()
    {
        return "sourcemaps: skipped storing the worker map (could not write it)".to_string();
    }
    prune_maps(&app_maps_dir(data_dir, slug), keep).await;
    "sourcemaps: stored the worker map".to_string()
}

/// Drop sha dirs outside `keep` (plus the manifest-less strays a crashed
/// deploy may leave). Best-effort: a leftover dir renders those frames raw.
async fn prune_maps(maps: &Path, keep: &[(String, i64)]) {
    let Ok(mut entries) = tokio::fs::read_dir(maps).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let wanted = is_key_sha(&name) && keep.iter().any(|(sha, _)| sha == &name);
        if !wanted {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}

/// One parsed map per sha dir that names a manifest worker map. `None` for
/// missing manifests, oversize or broken maps: those frames render raw.
pub fn load_worker_map(data_dir: &Path, slug: &str, sha: &str) -> Option<sourcemap::SourceMap> {
    if !is_key_sha(sha) {
        return None;
    }
    let dir = app_maps_dir(data_dir, slug).join(sha);
    let manifest_bytes = std::fs::read(dir.join(MANIFEST_FILE)).ok()?;
    let manifest: MapManifest = serde_json::from_slice(&manifest_bytes).ok()?;
    let rel = manifest.worker?;
    if rel.contains('/') || rel.contains('\\') || rel.is_empty() {
        return None;
    }
    let bytes = std::fs::read(dir.join(&rel)).ok()?;
    if bytes.len() as u64 > MAX_MAP_BYTES {
        return None;
    }
    sourcemap::SourceMap::from_slice(&bytes).ok()
}

/// One frame location split into file + 0-based line/col:
/// `worker.js:21:27` → `("worker.js", 20, 26)`. V8 prints 1-based positions.
/// `None` for anything that is not a mappable Worker frame: only the two
/// bundle names attempt a lookup, so `node:` internals, `file://` URLs,
/// paths with a directory, and unknown bare names never reach the map.
/// The allowlist is load-bearing, not just hygiene: the token lookup is
/// positional, so anything else would map to a wrong app position.
pub fn split_worker_location(location: &str) -> Option<(&str, u32, u32)> {
    let (head, col) = location.rsplit_once(':')?;
    let col: u32 = col.parse().ok()?;
    let (file, line) = head.rsplit_once(':')?;
    let line: u32 = line.parse().ok()?;
    if line == 0 || !is_bundle_file(file) {
        return None;
    }
    Some((file, line - 1, col.saturating_sub(1)))
}

/// The original position for one Worker frame location, or `None` when the
/// frame is not a bundle frame or the map has no token for it. The map
/// lookup is greatest-lower-bound, so a minified column still lands on its
/// token.
pub fn mapped_location(map: &sourcemap::SourceMap, location: &str) -> Option<String> {
    let (_, line, col) = split_worker_location(location)?;
    let token = map.lookup_token(line, col)?;
    let source = token.get_source()?;
    Some(format!(
        "{}:{}:{}",
        source,
        token.get_src_line() + 1,
        token.get_src_col() + 1
    ))
}

/// A V8 stack line with its bundle tail mapped in place:
/// `    at handler (index.js:21:27)` → `    at handler (src/app.ts:5:10)`.
/// `None` when the line carries no mappable bundle frame (plain log text,
/// Python frames, internals), so callers keep the raw line untouched.
pub fn mapped_frame_line(map: &sourcemap::SourceMap, line: &str) -> Option<String> {
    // The parenthesized form first: `at fn (index.js:L:C)`. The tail inside
    // the last parens is the only candidate; anything unparseable stays raw.
    if let Some(open) = line.rfind('(') {
        if let Some(close) = line[open..].rfind(')') {
            let inner = &line[open + 1..open + close];
            if let Some(mapped) = mapped_location(map, inner.trim()) {
                return Some(format!(
                    "{}{}{}",
                    &line[..open + 1],
                    mapped,
                    &line[open + close..]
                ));
            }
        }
    }
    // The bare form: `at index.js:L:C` at the line end. Trim first: the
    // candidate is a whitespace-delimited token, so without the trim a
    // trailing space would fold the whole line into the prefix and append
    // the mapping after it (duplicating the frame), and the trailing
    // whitespace is preserved byte-identically after the mapping.
    let end = line.trim_end().len();
    let candidate = line[..end].split_whitespace().next_back()?;
    let mapped = mapped_location(map, candidate)?;
    Some(format!(
        "{}{}{}",
        &line[..end - candidate.len()],
        mapped,
        &line[end..]
    ))
}

/// One parsed map per sha, loaded on first use for that sha. The cache
/// borrow (`'c`) and the key lifetime (`'s`) stay separate: unifying them
/// would hold the mutable borrow for the whole caller.
fn cached_map<'c, 's>(
    cache: &'c mut HashMap<&'s str, Option<sourcemap::SourceMap>>,
    data_dir: &Path,
    slug: &str,
    sha: &'s str,
) -> Option<&'c sourcemap::SourceMap> {
    cache
        .entry(sha)
        .or_insert_with(|| load_worker_map(data_dir, slug, sha))
        .as_ref()
}

/// A read's symbolication state: the deploy shas plus one parsed map per
/// sha actually referenced, so a page of fifty frames parses each map once.
/// Frames without a deploy or a token map to `None` (the caller keeps raw).
pub struct Symbolicator<'a> {
    data_dir: &'a Path,
    slug: &'a str,
    shas: &'a [(String, i64)],
    cache: HashMap<&'a str, Option<sourcemap::SourceMap>>,
}

impl<'a> Symbolicator<'a> {
    /// Symbolicate against `shas` (newest-first successful deploys, see
    /// [`deploy_shas`]) with maps under `data_dir`. A `None` data dir (the
    /// test fake has no pipeline) maps nothing: every frame stays raw.
    pub fn new(
        data_dir: Option<&'a Path>,
        slug: &'a str,
        shas: &'a [(String, i64)],
    ) -> Option<Self> {
        Some(Self {
            data_dir: data_dir?,
            slug,
            shas,
            cache: HashMap::new(),
        })
    }

    /// The original position for one stored issue frame, or `None` to keep
    /// the raw frame. `ts_secs` is the frame's unix timestamp (the issue's
    /// `last_seen` in seconds).
    pub fn mapped_frame(&mut self, ts_secs: i64, frame: Option<&str>) -> Option<String> {
        // The stored frame is one location (`at ... (index.js:L:C)` or a
        // bare tail); map the location and keep the surrounding frame text.
        let f = frame?;
        let sha = sha_for_ts(self.shas, ts_secs)?;
        let map = cached_map(&mut self.cache, self.data_dir, self.slug, sha)?;
        mapped_frame_line(map, f).or_else(|| mapped_location(map, f.trim()).map(|m| m.to_string()))
    }

    /// The line with its bundle tail mapped in place, or `None` to keep the
    /// raw line. `ts_secs` is the line's unix timestamp (`app_log` seconds).
    pub fn mapped_line(&mut self, ts_secs: i64, line: &str) -> Option<String> {
        let sha = sha_for_ts(self.shas, ts_secs)?;
        let map = cached_map(&mut self.cache, self.data_dir, self.slug, sha)?;
        mapped_frame_line(map, line)
    }
}

/// Symbolicate one issue frame plus a set of log lines against the newest
/// successful deploy at or before each timestamp. Maps parse once per sha
/// per call. Frames without a deploy or a token keep their raw text: the
/// return carries the mapped frame (or the raw one) and, per line, the
/// mapped line (or `None` to keep the raw line).
pub fn symbolicate_issue(
    data_dir: &Path,
    slug: &str,
    shas: &[(String, i64)],
    frame_ts_secs: i64,
    frame: Option<&str>,
    lines: &[(i64, String)],
) -> (Option<String>, Vec<Option<String>>) {
    let mut sym =
        Symbolicator::new(Some(data_dir), slug, shas).expect("a data dir always symbolicates");
    let mapped_frame = sym.mapped_frame(frame_ts_secs, frame);
    let mapped_lines = lines
        .iter()
        .map(|(ts, line)| sym.mapped_line(*ts, line))
        .collect();
    (mapped_frame, mapped_lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal v3 map: `index.js` line 1 col 1 maps to `src/app.ts` 1:1.
    /// `mappings: "AAAA"` is one segment (gen col 0, source 0, src line 0,
    /// src col 0, no name).
    fn test_map_bytes() -> Vec<u8> {
        br#"{"version":3,"sources":["src/app.ts"],"names":[],"mappings":"AAAA"}"#.to_vec()
    }

    fn test_map() -> sourcemap::SourceMap {
        sourcemap::SourceMap::from_slice(&test_map_bytes()).unwrap()
    }

    #[test]
    fn worker_location_parses_one_based_positions() {
        assert_eq!(
            split_worker_location("index.js:21:27"),
            Some(("index.js", 20, 26))
        );
        // Zero lines and columns saturate instead of underflowing.
        assert_eq!(
            split_worker_location("index.js:1:0"),
            Some(("index.js", 0, 0))
        );
    }

    #[test]
    fn worker_location_accepts_bare_bundle_names() {
        // Real fleets report the entry bare: upstream's `worker.js` and the
        // fork's `index.js` module name alike.
        assert_eq!(
            split_worker_location("worker.js:21:27"),
            Some(("worker.js", 20, 26))
        );
        assert_eq!(
            split_worker_location("index.js:21:27"),
            Some(("index.js", 20, 26))
        );
    }

    #[test]
    fn worker_location_rejects_non_bundle_frames() {
        // `node:` internals, paths, unknown bare names, and Python frames
        // never reach a map lookup: the positional lookup would map them to
        // a wrong app position instead of missing.
        for location in [
            "node:internal/modules:5:10",
            "node:events:1:1",
            "events:1:1",
            "server.ts:1:1",
            "file:///app/index.js:1:2",
            "src/index.js:1:2",
            "index.js:0:1",
            "not-a-location",
            "index.js:1",
        ] {
            assert_eq!(split_worker_location(location), None, "{location}");
        }
    }

    #[test]
    fn mapped_frame_line_preserves_trailing_whitespace() {
        let map = test_map();
        assert_eq!(
            mapped_frame_line(&map, "    at handler (index.js:1:1)  "),
            Some("    at handler (src/app.ts:1:1)  ".to_string())
        );
        assert_eq!(
            mapped_frame_line(&map, "    at index.js:1:1\t"),
            Some("    at src/app.ts:1:1\t".to_string())
        );
    }

    #[test]
    fn mapped_location_resolves_through_the_map() {
        assert_eq!(
            mapped_location(&test_map(), "index.js:1:1"),
            Some("src/app.ts:1:1".to_string())
        );
        // Past the only mapping the greatest-lower-bound token still wins.
        assert_eq!(
            mapped_location(&test_map(), "index.js:9:99"),
            Some("src/app.ts:1:1".to_string())
        );
    }

    #[test]
    fn mapped_frame_line_substitutes_the_bundle_tail() {
        let map = test_map();
        assert_eq!(
            mapped_frame_line(&map, "    at handler (index.js:1:1)"),
            Some("    at handler (src/app.ts:1:1)".to_string())
        );
        assert_eq!(
            mapped_frame_line(&map, "    at index.js:1:1"),
            Some("    at src/app.ts:1:1".to_string())
        );
        // Plain text and foreign frames pass through as `None`.
        assert_eq!(mapped_frame_line(&map, "GET / 200"), None);
        assert_eq!(
            mapped_frame_line(&map, "    at foo (node:internal/x:1:1)"),
            None
        );
    }

    #[tokio::test]
    async fn store_round_trips_and_prunes_old_shas() {
        let data = tempfile::tempdir().unwrap();
        let slug = "shop";
        let sha_new = "aa".repeat(32);
        let sha_old = "bb".repeat(32);
        // A stale dir from a crashed deploy (no manifest) prunes away, and
        // only the kept sha survives.
        let stale = app_maps_dir(data.path(), slug).join("cc".repeat(32));
        tokio::fs::create_dir_all(&stale).await.unwrap();
        let keep = vec![(sha_new.clone(), 2), (sha_old.clone(), 1)];
        let note = store_worker_map(
            data.path(),
            slug,
            &sha_new,
            MapRead::Found(test_map_bytes()),
            &keep,
        )
        .await;
        assert_eq!(note, "sourcemaps: stored the worker map");
        assert!(load_worker_map(data.path(), slug, &sha_new).is_some());
        assert!(!stale.exists());
        // Pruning to the newest sha drops the older dir.
        let keep = vec![(sha_new.clone(), 2)];
        prune_maps(&app_maps_dir(data.path(), slug), &keep).await;
        assert!(!app_maps_dir(data.path(), slug).join(&sha_old).exists());
        assert!(app_maps_dir(data.path(), slug)
            .join(&sha_new)
            .join(WORKER_MAP_FILE)
            .exists());
    }

    #[tokio::test]
    async fn store_refuses_bad_inputs_without_failing() {
        let data = tempfile::tempdir().unwrap();
        let keep: Vec<(String, i64)> = vec![];
        // No bytes: celld had nothing to map (asset-only, Python, missing
        // sibling) — storing nothing is the answer, not an error.
        let note =
            store_worker_map(data.path(), "s", &"aa".repeat(32), MapRead::Absent, &keep).await;
        assert!(note.contains("nothing to map"), "{note}");
        // Over-cap surfaces as its own refusal, not a truncated map.
        let note =
            store_worker_map(data.path(), "s", &"aa".repeat(32), MapRead::OverCap, &keep).await;
        assert!(note.contains("size cap"), "{note}");
        // A hostile sha never becomes a path.
        let note = store_worker_map(
            data.path(),
            "s",
            "../../etc",
            MapRead::Found(test_map_bytes()),
            &keep,
        )
        .await;
        assert!(note.contains("bad deploy sha"), "{note}");
        assert!(!data.path().join(MAPS_DIR).join("etc").exists());
        // Oversize and broken maps refuse before touching disk.
        let note = store_worker_map(
            data.path(),
            "s",
            &"aa".repeat(32),
            MapRead::Found(vec![0u8; MAX_MAP_BYTES as usize + 1]),
            &keep,
        )
        .await;
        assert!(note.contains("size cap"), "{note}");
        let note = store_worker_map(
            data.path(),
            "s",
            &"aa".repeat(32),
            MapRead::Found(b"not a map".to_vec()),
            &keep,
        )
        .await;
        assert!(note.contains("not a v3"), "{note}");
        assert!(!data.path().join(MAPS_DIR).join("s").exists());
    }

    #[tokio::test]
    async fn read_map_file_rejects_the_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        // Missing is absent, not an error.
        assert_eq!(
            read_map_file(&dir.path().join("nope.map")).await,
            MapRead::Absent
        );
        // A symlink to a good map is still refused: the app uid must not
        // redirect the read.
        let target = dir.path().join("real.map");
        std::fs::write(&target, test_map_bytes()).unwrap();
        #[cfg(unix)]
        {
            let link = dir.path().join("link.map");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(read_map_file(&link).await, MapRead::Absent);
        }
        // Directories are not maps.
        assert_eq!(read_map_file(dir.path()).await, MapRead::Absent);
        // Empty files are absent, not corrupt.
        let empty = dir.path().join("empty.map");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(read_map_file(&empty).await, MapRead::Absent);
        // Over the cap reads as over-cap, never truncated.
        let big = dir.path().join("big.map");
        std::fs::write(&big, vec![0u8; MAX_MAP_BYTES as usize + 1]).unwrap();
        assert_eq!(read_map_file(&big).await, MapRead::OverCap);
        // Exactly at the cap still reads whole: the cap bounds the read,
        // and the store's own size check decides what to keep.
        let edge = dir.path().join("edge.map");
        std::fs::write(&edge, vec![0u8; MAX_MAP_BYTES as usize]).unwrap();
        assert!(matches!(read_map_file(&edge).await, MapRead::Found(_)));
        // And the plain case round-trips.
        assert_eq!(
            read_map_file(&target).await,
            MapRead::Found(test_map_bytes())
        );
    }

    #[test]
    fn sha_joins_frames_to_their_deploy() {
        let shas = vec![
            ("new".to_string(), 2_000_000),
            ("old".to_string(), 1_000_000),
        ];
        assert_eq!(sha_for_ts(&shas, 1_500), Some("old"));
        assert_eq!(sha_for_ts(&shas, 2_500), Some("new"));
        assert_eq!(sha_for_ts(&shas, 500), None);
    }

    /// The read-time join: only successful deploys, newest first — a failed
    /// or queued row never maps a frame.
    #[tokio::test]
    async fn deploy_shas_lists_success_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('owner1', 'o@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at) \
             VALUES ('app1', 'shop', 'owner1', 10125, 21001, 22001, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, sha, status, created_at) in [
            ("d-old", "aa".repeat(32), "success", 1_000_000),
            ("d-failed", "bb".repeat(32), "failed", 2_000_000),
            ("d-new", "cc".repeat(32), "success", 3_000_000),
            ("d-queued", "dd".repeat(32), "building", 4_000_000),
        ] {
            sqlx::query(
                "INSERT INTO deploy (id, app_id, sha, kind, status, created_at) \
                 VALUES (?1, 'app1', ?2, 'main', ?3, ?4)",
            )
            .bind(id)
            .bind(&sha)
            .bind(status)
            .bind(created_at)
            .execute(&pool)
            .await
            .unwrap();
        }
        let shas = deploy_shas(&pool, "app1").await;
        assert_eq!(
            shas,
            vec![("cc".repeat(32), 3_000_000), ("aa".repeat(32), 1_000_000),]
        );
        assert!(deploy_shas(&pool, "no-such-app").await.is_empty());
        // A failed deploy still serves its version when the migrate step ran
        // after the upload: with `last_deploy_sha` on it, its map joins, or
        // the live version's frames would map against the previous deploy.
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, kind, status, created_at) \
             VALUES ('d-live-failed', 'app1', 'ee', 'main', 'failed', 4000000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE app SET last_deploy_sha = 'ee' WHERE id = 'app1'")
            .execute(&pool)
            .await
            .unwrap();
        let shas = deploy_shas(&pool, "app1").await;
        assert_eq!(
            shas.into_iter().map(|(s, _)| s).collect::<Vec<_>>(),
            vec!["ee".to_string(), "cc".repeat(32), "aa".repeat(32)]
        );
    }

    #[test]
    fn symbolicate_issue_maps_frame_and_lines_per_timestamp() {
        let data = tempfile::tempdir().unwrap();
        let dir = app_maps_dir(data.path(), "shop").join("aa".repeat(32));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(WORKER_MAP_FILE), test_map_bytes()).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_vec(&MapManifest {
                worker: Some(WORKER_MAP_FILE.to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let shas = vec![("aa".repeat(32), 1_000_000)];
        let (frame, lines) = symbolicate_issue(
            data.path(),
            "shop",
            &shas,
            1_500,
            Some("at handler (index.js:1:1)"),
            &[
                (1_500, "    at handler (index.js:1:1)".to_string()),
                (500, "    at handler (index.js:1:1)".to_string()),
                (1_500, "GET / 200".to_string()),
            ],
        );
        // The frame and the in-range line map; the pre-deploy line and the
        // plain line keep their raw text (`None`).
        assert_eq!(frame.as_deref(), Some("at handler (src/app.ts:1:1)"));
        assert_eq!(
            lines,
            vec![
                Some("    at handler (src/app.ts:1:1)".to_string()),
                None,
                None,
            ]
        );
    }
}
