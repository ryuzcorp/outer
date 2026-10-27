//! Console R2 object browser backend.
//!
//! Ported from Noite `apps/runner/src/host/storage/r2.rs` @ 58a977a
//! (Apache-2.0): the `r2_encode`/`r2_decode` key form, the key/header
//! validators, and the read/write split are verbatim. Adapted to Outer:
//! reads use Outer's [`crate::store::s3::S3Backend`] with per-request scoped
//! credentials (`fleets/{slug}/`), writes and deletes shell out to `celld r2
//! put` / `celld r2 delete` with the same scoped env the deploy pipeline
//! uses, and the bucket allowlist comes from the app's declared
//! `r2_buckets` instead of the runner's app registry.
//!
//! Reads work on a sleeping fleet (S3 + bucket-direct celld calls need no
//! running node). Every write is audited (`r2_put`, `r2_delete`).

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{celld_env, console_app, run_celld, scoped_creds, scoped_s3};

/// R2's own key bound (1..=1024 bytes).
const R2_MAX_KEY_BYTES: usize = 1024;
/// Folders + files one listing page returns.
const R2_PAGE_LIMIT: usize = 100;
/// One delete call carries at most this many keys (the UI's bulk cap).
const R2_DELETE_LIMIT: usize = 100;
/// Largest object the raw download serves, matching the UI's upload cap.
const R2_RAW_CAP: i64 = 67_108_864;
/// Largest body the text preview fetches; display truncates to
/// [`R2_PREVIEW_CAP`] and anything larger previews as `null`.
const R2_PREVIEW_FETCH_CAP: i64 = 1_048_576;
/// Largest body the text preview displays.
const R2_PREVIEW_CAP: usize = 262_144;
/// HEADs one listing page runs in parallel for content types.
const R2_HEAD_CONCURRENCY: usize = 8;
/// Largest upload the browser may send (spooled, never buffered twice).
const R2_UPLOAD_CAP: usize = 67_108_864;
/// `celld r2 put` gets a generous timeout: a full 64 MiB upload on a slow
/// link must fail on stall, not on a fixed wall.
const CELLD_PUT_TIMEOUT: Duration = Duration::from_secs(300);
const CELLD_TIMEOUT: Duration = Duration::from_secs(60);

/// Content types a browser renders as a document: served inline like the
/// rest, but under a sandbox, so a direct visit to the raw URL cannot run
/// the script an uploaded object might carry.
const SANDBOXED_R2_TYPES: &[&str] = &[
    "application/xhtml+xml",
    "application/xml",
    "image/svg+xml",
    "text/html",
    "text/xml",
];

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/console/apps/{slug}/data/r2/buckets", get(api_buckets))
        .route("/api/console/apps/{slug}/data/r2/list", get(api_list))
        .route("/api/console/apps/{slug}/data/r2/file", get(api_file))
        .route("/api/console/apps/{slug}/data/r2/raw", get(api_raw))
        .route("/api/console/apps/{slug}/data/r2/object", put(api_put))
        .route("/api/console/apps/{slug}/data/r2/delete", post(api_delete))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct R2Bucket {
    pub binding: String,
    pub bucket: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct R2Folder {
    pub name: String,
    pub prefix: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct R2Object {
    pub key: String,
    pub name: String,
    pub size: i64,
    pub last_modified: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct R2Preview {
    bucket: String,
    prefix: String,
    folders: Vec<R2Folder>,
    objects: Vec<R2Object>,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct R2File {
    key: String,
    size: i64,
    truncated: bool,
    content_type: Option<String>,
    text: Option<String>,
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + value - 10) as char,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `true` for a byte celld escapes when it stores a key.
fn r2_escapable(byte: u8) -> bool {
    if byte < 0x20 || byte == 0x7F || byte >= 0x80 {
        return true;
    }
    matches!(
        byte,
        b'%' | b'\\'
            | b'{'
            | b'}'
            | b'^'
            | b'`'
            | b'['
            | b']'
            | b'"'
            | b'<'
            | b'>'
            | b'~'
            | b'#'
            | b'|'
            | b'*'
            | b'?'
    )
}

/// The stored form of an R2 key: every escapable byte as `%XX`, an empty
/// segment as `%` (the docs' `photos/` → `r2/<bucket>/photos/%`).
pub fn r2_encode(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (index, segment) in key.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        if segment.is_empty() {
            out.push('%');
            continue;
        }
        for byte in segment.bytes() {
            if r2_escapable(byte) {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0F));
            } else {
                out.push(byte as char);
            }
        }
    }
    out
}

/// The stored form of a *folder prefix* for a listing. Unlike a key, its
/// trailing `/` is the parent separator, not an empty segment: `dir/` lists
/// `…/dir/`, while the key `dir/` would be stored as `…/dir/%`.
pub fn r2_encode_prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        return String::new();
    }
    let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
    format!("{}/", r2_encode(trimmed))
}

/// One stored segment back to its key bytes. A lone `%` is the empty
/// segment celld wrote; a malformed escape (a key another tool wrote) is
/// kept literally rather than failing the whole page.
fn r2_decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The key a stored key names (the inverse of `r2_encode`).
pub fn r2_decode(stored: &str) -> String {
    let mut out = String::with_capacity(stored.len());
    for (index, segment) in stored.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        if segment == "%" {
            continue;
        }
        out.push_str(&r2_decode_segment(segment));
    }
    out
}

/// A key the browser may write or delete: 1..=1024 bytes, no absolute path
/// and no `..` segment, no control character, and never starting with `-`.
/// A trailing `/` is legal — it is the folder marker. Keys travel
/// positionally to `celld r2 put` / `celld r2 delete`, and celld 0.6.2
/// honors no `--` separator, so the leading-dash rule closes option
/// injection there.
pub fn r2_key_ok(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= R2_MAX_KEY_BYTES
        && !key.starts_with('/')
        && !key.starts_with('-')
        && !key.split('/').any(|segment| segment == "..")
        && !key.chars().any(char::is_control)
}

/// A folder prefix a listing may name: `""` (the bucket root) or a key that
/// passes `r2_key_ok` after its trailing `/` is normalized on.
pub fn r2_prefix_ok(prefix: &str) -> bool {
    prefix.is_empty() || r2_key_ok(prefix)
}

/// A content type (or any header value) that is safe to place in a command
/// argument and a response header.
pub fn r2_header_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.bytes().all(|byte| (0x20..0x7F).contains(&byte))
}

fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// Parse `r2_buckets` from the built wrangler config. Entries need a string
/// `binding` and `bucket_name`; both come from the app's own config, but
/// both are validated here before anything is listed through or passed to
/// celld — a `bucket_name` starting with `-` would otherwise parse as a
/// celld option.
pub fn r2_buckets(value: &serde_json::Value) -> anyhow::Result<Vec<R2Bucket>> {
    let Some(list) = value.get("r2_buckets") else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("r2_buckets must be an array, got {list}"))?;
    list.iter()
        .enumerate()
        .map(|(i, entry)| {
            let obj = entry
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("r2_buckets[{i}] must be an object"))?;
            let binding = obj
                .get("binding")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("r2_buckets[{i}] needs a string binding"))?;
            let bucket = obj
                .get("bucket_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("r2_buckets[{i}] needs a string bucket_name"))?;
            validate_binding(binding).map_err(|e| anyhow::anyhow!("r2_buckets[{i}]: {e:#}"))?;
            validate_bucket_name(bucket).map_err(|e| anyhow::anyhow!("r2_buckets[{i}]: {e:#}"))?;
            Ok(R2Bucket {
                binding: binding.to_string(),
                bucket: bucket.to_string(),
            })
        })
        .collect()
}

/// A binding is passed nowhere, but it names the entry in the UI: JS
/// identifier shape, bounded.
pub(crate) fn validate_binding(binding: &str) -> anyhow::Result<()> {
    if binding.is_empty() || binding.len() > 64 {
        anyhow::bail!("binding must be 1..=64 characters");
    }
    if !binding
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        anyhow::bail!("binding {binding:?} must be letters, digits or `_`");
    }
    Ok(())
}

/// An R2 bucket name is passed positionally to `celld r2`, so it must never
/// parse as an option: lowercase letters, digits, dots and hyphens,
/// 1–63 characters, never starting with `-`.
pub(crate) fn validate_bucket_name(bucket: &str) -> anyhow::Result<()> {
    if bucket.is_empty() || bucket.len() > 63 {
        anyhow::bail!("bucket_name must be 1..=63 characters");
    }
    if bucket.starts_with('-') || bucket.starts_with('.') {
        anyhow::bail!("bucket_name {bucket:?} must not start with `-` or `.`");
    }
    if !bucket
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        anyhow::bail!("bucket_name {bucket:?} must be lowercase letters, digits, `-` or `.`");
    }
    Ok(())
}

/// The S3 prefix one bucket lives under: `fleets/{slug}/r2/{bucket}/`.
/// Every object key is built by appending the encoded key to this, so a key
/// can never escape the bucket.
pub fn data_prefix(slug: &str, bucket: &str) -> String {
    format!("fleets/{slug}/r2/{bucket}/")
}

/// The full S3 key for a browser key: validated, encoded, and confined under
/// [`data_prefix`]. The single choke point for prefix confinement.
pub fn object_key(slug: &str, bucket: &R2Bucket, key: &str) -> anyhow::Result<String> {
    if !r2_key_ok(key) {
        anyhow::bail!("invalid key");
    }
    Ok(format!(
        "{}{}",
        data_prefix(slug, &bucket.bucket),
        r2_encode(key)
    ))
}

/// Look up the declared bucket behind a request bucket name. Anything not
/// declared is refused before a credential or celld ever sees it.
pub fn find_bucket<'a>(buckets: &'a [R2Bucket], name: &str) -> anyhow::Result<&'a R2Bucket> {
    buckets
        .iter()
        .find(|b| b.bucket == name)
        .ok_or_else(|| anyhow::anyhow!("unknown R2 bucket {name:?}"))
}

/// The exact argv for `celld r2 put`, built pure so tests pin it: key and
/// bucket travel positionally (both pre-validated, never starting with
/// `-`), and `--bucket` carries the fleet bucket URL.
pub fn celld_put_argv(
    bucket_url: &str,
    endpoint: &str,
    region: &str,
    bucket: &str,
    key: &str,
    spool: &str,
    content_type: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "r2".to_string(),
        "put".to_string(),
        bucket.to_string(),
        key.to_string(),
        "--path".to_string(),
        spool.to_string(),
    ];
    if let Some(ct) = content_type {
        args.push("--content-type".to_string());
        args.push(ct.to_string());
    }
    args.extend([
        "--bucket".to_string(),
        bucket_url.to_string(),
        "--endpoint".to_string(),
        endpoint.to_string(),
        "--region".to_string(),
        region.to_string(),
    ]);
    args
}

/// The exact argv for `celld r2 delete`, built pure so tests pin it.
pub fn celld_delete_argv(
    bucket_url: &str,
    endpoint: &str,
    region: &str,
    bucket: &str,
    keys: &[String],
) -> Vec<String> {
    let mut args = vec!["r2".to_string(), "delete".to_string(), bucket.to_string()];
    args.extend(keys.iter().cloned());
    args.extend([
        "--bucket".to_string(),
        bucket_url.to_string(),
        "--endpoint".to_string(),
        endpoint.to_string(),
        "--region".to_string(),
        region.to_string(),
    ]);
    args
}

/// Split one S3 page into the rows a browser shows: `CommonPrefixes` are the
/// folders below the listed prefix, `Contents` the files in it. The zero-byte
/// marker that makes an empty folder visible lands in `Contents` as `<folder>/`
/// and is dropped — the folder itself comes back as a common prefix. Pure so
/// the prefix/delimiter rules stay unit-testable without a store.
fn split_page(
    s3_prefix: &str,
    parent: &str,
    stored_prefixes: &[String],
    stored_objects: &[crate::store::s3::S3Object],
) -> (Vec<R2Folder>, Vec<R2Object>) {
    let mut folders = Vec::new();
    for stored in stored_prefixes {
        let Some(relative) = stored.strip_prefix(s3_prefix) else {
            continue;
        };
        let decoded = r2_decode(relative);
        // A prefix always ends in `/`; the name is the segment before it, and
        // an empty one (a key with an empty segment) still browses.
        let trimmed = decoded.strip_suffix('/').unwrap_or(&decoded);
        let name = trimmed.rsplit('/').next().unwrap_or("");
        folders.push(R2Folder {
            name: if name.is_empty() {
                "(empty)".to_string()
            } else {
                name.to_string()
            },
            prefix: format!("{parent}{decoded}"),
        });
    }
    folders.sort_by(|a, b| a.name.cmp(&b.name));

    let mut objects = Vec::new();
    for object in stored_objects {
        let Some(relative) = object.key.strip_prefix(s3_prefix) else {
            continue;
        };
        let key = format!("{parent}{}", r2_decode(relative));
        if key.ends_with('/') {
            continue;
        }
        let name = key.rsplit('/').next().unwrap_or("").to_string();
        objects.push(R2Object {
            key,
            name,
            size: i64::try_from(object.size).unwrap_or(i64::MAX),
            last_modified: object.last_modified.clone(),
            content_type: None,
            etag: Some(object.etag.clone()),
        });
    }
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    (folders, objects)
}

/// The raw-download response: the stored media type, an inline filename, no
/// sniffing, and a `sandbox` CSP on the types a browser renders as a
/// document.
fn raw_response(content_type: Option<&str>, filename: &str, bytes: Vec<u8>) -> Response {
    let media = content_type.unwrap_or("application/octet-stream");
    let media = if r2_header_ok(media) {
        media.to_string()
    } else {
        "application/octet-stream".to_string()
    };
    let base = media.split(';').next().unwrap_or("").trim().to_lowercase();
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", media)
        .header(
            "content-disposition",
            format!("inline; filename=\"{}\"", filename.replace('"', "_")),
        )
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store");
    if SANDBOXED_R2_TYPES.contains(&base.as_str()) {
        builder = builder.header("content-security-policy", "sandbox");
    }
    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| super::unavailable("download failed".to_string()))
}

/// R2 bindings from the deployed manifest's `raw_metadata.bindings`: entries
/// shaped `{"type": "r2_bucket", "name": <binding>, "bucket_name": <bucket>}`
/// (celld's record of the built wrangler config; the `name`/`bucket_name`
/// fallbacks cover the `binding`/`bucket` spellings). Anything else —
/// D1, assets, vars, unknown types — is skipped, never failed: one unknown
/// entry must not hide the buckets that parse. In particular `vars` (which
/// may hold injected secret values) is never read, logged or returned.
pub(crate) fn parse_manifest_r2(manifest: &serde_json::Value) -> Vec<R2Bucket> {
    let bindings = manifest
        .get("raw_metadata")
        .and_then(|meta| meta.get("bindings"))
        .and_then(|bindings| bindings.as_array());
    let Some(bindings) = bindings else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in bindings {
        let is_r2 = entry.get("type").and_then(|v| v.as_str()) == Some("r2_bucket");
        if !is_r2 {
            continue;
        }
        let binding = entry
            .get("name")
            .or_else(|| entry.get("binding"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let bucket = entry
            .get("bucket_name")
            .or_else(|| entry.get("bucket"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if validate_binding(binding).is_err() || validate_bucket_name(bucket).is_err() {
            tracing::warn!(
                binding,
                bucket,
                "skipping manifest R2 binding that fails validation"
            );
            continue;
        }
        out.push(R2Bucket {
            binding: binding.to_string(),
            bucket: bucket.to_string(),
        });
    }
    out
}

async fn declared_buckets(state: &super::AppState, slug: &str) -> anyhow::Result<Vec<R2Bucket>> {
    // The bucket manifest is authoritative whenever present; the local
    // build artifact is only the fallback when it is missing.
    if let Some(manifest) = super::deployed_manifest(&state.mcp.store, slug).await? {
        return Ok(parse_manifest_r2(&manifest));
    }
    let Some(value) = super::built_config_value(state, slug)? else {
        return Ok(Vec::new());
    };
    r2_buckets(&value)
}

/// Backend failures (S3/celld) answer 500 with the failure's detail in
/// `message` for the owner to act on, but `user_message` stays the friendly
/// constant the caller passes: raw store XML must never reach the UI.
fn status_for(msg: &str, user_message: &str) -> Response {
    if msg.contains("not found") || msg.contains("unknown R2 bucket") {
        super::missing(msg.to_string())
    } else if msg.contains("invalid") || msg.contains("bucket_name") || msg.contains("select 1 to")
    {
        super::bad_request(msg.to_string())
    } else if msg.contains("too large") {
        super::too_large(msg.to_string())
    } else {
        super::unavailable_friendly(msg.to_string(), user_message)
    }
}

async fn api_buckets(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
) -> Response {
    let (_account, app, _) = match console_app(&state, &headers, &slug, Some("data_r2")).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    match declared_buckets(&state, &app.slug).await {
        Ok(buckets) => super::ok(serde_json::json!({"buckets": buckets})),
        Err(err) => super::unavailable_friendly(
            format!("{err:#}"),
            "Couldn't load the buckets right now. Try again.",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KeyQuery {
    bucket: String,
    key: String,
}

#[derive(Debug, Deserialize)]
struct DeleteBody {
    bucket: String,
    keys: Vec<String>,
}

async fn api_list(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    Query(query): Query<ListQuery>,
) -> Response {
    let (account, app, _) = match console_app(&state, &headers, &slug, Some("data_r2")).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    let _ = account;
    match list_inner(&state, &app.slug, &query).await {
        Ok(preview) => super::ok(serde_json::to_value(&preview).unwrap_or_default()),
        Err(err) => status_for(
            &format!("{err:#}"),
            "Couldn't list this bucket right now. Try again.",
        ),
    }
}

async fn list_inner(
    state: &super::AppState,
    slug: &str,
    query: &ListQuery,
) -> anyhow::Result<R2Preview> {
    let buckets = declared_buckets(state, slug).await?;
    let bucket = find_bucket(&buckets, &query.bucket)?;
    if !r2_prefix_ok(&query.prefix) {
        anyhow::bail!("invalid prefix");
    }
    let prefix = normalize_prefix(&query.prefix);
    let base = data_prefix(slug, &bucket.bucket);
    let s3_prefix = format!("{base}{}", r2_encode_prefix(&prefix));
    let s3 = scoped_s3(state, slug).await?;
    let page = s3
        .list_delimited_page(&s3_prefix, "/", R2_PAGE_LIMIT, query.cursor.as_deref())
        .await?;
    let (folders, mut objects) = split_page(&s3_prefix, &prefix, &page.prefixes, &page.objects);

    // The listing carries no media type, so ask each visible object for its
    // record — a page is bounded, head requests are cheap and they run in
    // parallel. A failure leaves the type unknown rather than failing the page.
    let full_keys: Vec<String> = objects
        .iter()
        .map(|object| format!("{base}{}", r2_encode(&object.key)))
        .collect();
    let heads: Vec<Option<crate::store::s3::S3Head>> =
        futures::stream::iter(full_keys.into_iter().map(|full| {
            let s3 = &s3;
            async move { s3.head(&full).await }
        }))
        .buffered(R2_HEAD_CONCURRENCY)
        .collect::<Vec<anyhow::Result<Option<crate::store::s3::S3Head>>>>()
        .await
        .into_iter()
        .map(|r| r.unwrap_or(None))
        .collect();
    for (object, head) in objects.iter_mut().zip(heads) {
        if let Some(head) = head {
            object.content_type = head.content_type;
        }
    }
    Ok(R2Preview {
        bucket: bucket.bucket.clone(),
        prefix,
        folders,
        objects,
        next_cursor: page.next,
    })
}

async fn api_file(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    Query(query): Query<KeyQuery>,
) -> Response {
    let (_account, app, _) = match console_app(&state, &headers, &slug, Some("data_r2")).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    match file_inner(&state, &app.slug, &query).await {
        Ok(file) => super::ok(serde_json::to_value(&file).unwrap_or_default()),
        Err(err) => status_for(
            &format!("{err:#}"),
            "Couldn't open this object right now. Try again.",
        ),
    }
}

async fn file_inner(
    state: &super::AppState,
    slug: &str,
    query: &KeyQuery,
) -> anyhow::Result<R2File> {
    let buckets = declared_buckets(state, slug).await?;
    let bucket = find_bucket(&buckets, &query.bucket)?;
    let full = object_key(slug, bucket, &query.key)?;
    let s3 = scoped_s3(state, slug).await?;
    let Some(head) = s3.head(&full).await? else {
        anyhow::bail!("object not found");
    };
    if head.size > R2_PREVIEW_FETCH_CAP {
        return Ok(R2File {
            key: query.key.clone(),
            size: head.size,
            truncated: true,
            content_type: head.content_type,
            text: None,
        });
    }
    let bytes = s3.get(&full).await?.unwrap_or_default();
    let truncated = bytes.len() > R2_PREVIEW_CAP;
    let preview = &bytes[..bytes.len().min(R2_PREVIEW_CAP)];
    Ok(R2File {
        key: query.key.clone(),
        size: head.size,
        truncated,
        content_type: head.content_type,
        text: String::from_utf8(preview.to_vec()).ok(),
    })
}

async fn api_raw(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    Query(query): Query<KeyQuery>,
) -> Response {
    let (_account, app, _) = match console_app(&state, &headers, &slug, Some("data_r2")).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    match raw_inner(&state, &app.slug, &query).await {
        Ok(res) => res,
        Err(err) => status_for(
            &format!("{err:#}"),
            "Couldn't download this object right now. Try again.",
        ),
    }
}

async fn raw_inner(
    state: &super::AppState,
    slug: &str,
    query: &KeyQuery,
) -> anyhow::Result<Response> {
    let buckets = declared_buckets(state, slug).await?;
    let bucket = find_bucket(&buckets, &query.bucket)?;
    let full = object_key(slug, bucket, &query.key)?;
    let s3 = scoped_s3(state, slug).await?;
    let Some(head) = s3.head(&full).await? else {
        anyhow::bail!("object not found");
    };
    if head.size > R2_RAW_CAP {
        anyhow::bail!("object too large to download");
    }
    let bytes = s3.get(&full).await?.unwrap_or_default();
    let filename = query.key.rsplit('/').next().unwrap_or(&query.key);
    Ok(raw_response(head.content_type.as_deref(), filename, bytes))
}

/// Whether an upload of `size` bytes would pass the plan's R2 cap on top of
/// `measured` already-stored bytes. Pure so the boundary stays unit-tested;
/// `u64::MAX` measurements saturate instead of wrapping.
pub(crate) fn r2_upload_over_cap(measured: u64, size: u64, cap: u64) -> bool {
    measured.saturating_add(size) > cap
}

/// Refuse a Console upload up front when the app's last measured R2 bytes
/// plus this upload's size would pass the plan cap. `None` means proceed.
/// The measurement is the sweep's (`app.r2_measured_bytes`), falling back to
/// a live sizing that is stored when nothing is recorded yet. Fail-open on
/// store or database trouble (with a warning): a hiccup must not brick
/// uploads, and the hourly sweep flag stays the hard enforcement.
pub(crate) async fn refuse_r2_over_cap(
    mcp: &crate::mcp::McpState,
    app: &crate::apps::App,
    size: u64,
) -> Option<Response> {
    let plan = crate::mcp::tools::plan_of(&mcp.pool, &app.owner_id)
        .await
        .unwrap_or_else(|_| "free".to_string());
    let cap = crate::limits::r2_cap_bytes(&plan);
    let stored: Option<i64> = match sqlx::query_scalar::<_, Option<i64>>(
        "SELECT r2_measured_bytes FROM app WHERE id = ?1",
    )
    .bind(&app.id)
    .fetch_optional(&mcp.pool)
    .await
    {
        Ok(row) => row.flatten(),
        Err(err) => {
            tracing::warn!(slug = %app.slug, error = %format!("{err:#}"), "r2 upload gate: measurement read failed");
            return None;
        }
    };
    let measured = match stored {
        Some(m) => m.max(0) as u64,
        None => {
            let live = match mcp
                .store
                .prefix_bytes(&format!("fleets/{}/r2/", app.slug))
                .await
            {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(slug = %app.slug, error = %format!("{err:#}"), "r2 upload gate: live sizing failed");
                    return None;
                }
            };
            let _ = sqlx::query(
                "UPDATE app SET r2_measured_bytes = ?1 WHERE id = ?2 AND r2_measured_bytes IS NULL",
            )
            .bind(live.min(i64::MAX as u64) as i64)
            .bind(&app.id)
            .execute(&mcp.pool)
            .await;
            live
        }
    };
    if !r2_upload_over_cap(measured, size, cap) {
        return None;
    }
    let upgrade_url = mcp
        .billing
        .upgrade_url(&app.owner_id, chrono::Utc::now().timestamp());
    Some(super::data_json(
        StatusCode::TOO_MANY_REQUESTS,
        serde_json::json!({
            "error": "quota_exceeded",
            "message": format!(
                "{} storage is read-only: R2 holds {} MB and this upload adds {} MB over the {} MB cap",
                app.slug,
                measured / 1_048_576,
                size / 1_048_576,
                cap / 1_048_576,
            ),
            "user_message": format!(
                "This upload would push {} over its R2 cap on your Outer {plan} plan — nothing is deleted. Free space here or upgrade to Pro for bigger caps.",
                app.slug,
            ),
            "quota": "storage_bytes",
            "upgrade_url": upgrade_url,
        }),
    ))
}

/// Add an upload's bytes to the stored R2 measurement (only when one is
/// stored — otherwise the next gate re-sizes live). Best effort.
pub(crate) async fn adjust_r2_measured(pool: &sqlx::SqlitePool, app_id: &str, delta: i64) {
    if let Err(err) = sqlx::query(
        "UPDATE app SET r2_measured_bytes = r2_measured_bytes + ?1 \
         WHERE id = ?2 AND r2_measured_bytes IS NOT NULL",
    )
    .bind(delta)
    .bind(app_id)
    .execute(pool)
    .await
    {
        tracing::warn!(app = app_id, error = %format!("{err:#}"), "r2 measurement adjust failed");
    }
}

/// Forget the stored R2 measurement after a delete (celld reports no freed
/// bytes): the next upload gate re-sizes live instead of refusing against a
/// stale-high number. Best effort.
pub(crate) async fn clear_r2_measured(pool: &sqlx::SqlitePool, app_id: &str) {
    if let Err(err) = sqlx::query("UPDATE app SET r2_measured_bytes = NULL WHERE id = ?1")
        .bind(app_id)
        .execute(pool)
        .await
    {
        tracing::warn!(app = app_id, error = %format!("{err:#}"), "r2 measurement clear failed");
    }
}

async fn api_put(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    Query(query): Query<KeyQuery>,
    body: Body,
) -> Response {
    let (account, app, admin) = match console_app(&state, &headers, &slug, None).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    // Frozen apps refuse uploads at the storage cap; deletes stay allowed
    // (the escape hatch back under the cap).
    if let Some(res) = super::storage_frozen_refusal(&state.mcp, &app).await {
        return res;
    }
    // Prospective cap check, before anything is staged or uploaded: the last
    // measured R2 bytes plus this upload's size must fit the plan cap. The
    // hourly sweep flag is the hard enforcement (in-runtime writes stay
    // sweep-enforced); this closes the within-the-hour burst window on the
    // Console path without trusting a client-supplied length — the bytes are
    // read first, then gated, then uploaded.
    let bytes = match axum::body::to_bytes(body, R2_UPLOAD_CAP + 1).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return status_for(
                "upload too large: 64 MiB cap",
                "Couldn't upload right now. Try again.",
            );
        }
    };
    if bytes.len() > R2_UPLOAD_CAP {
        return status_for(
            "upload too large: 64 MiB cap",
            "Couldn't upload right now. Try again.",
        );
    }
    if let Some(res) = refuse_r2_over_cap(&state.mcp, &app, bytes.len() as u64).await {
        return res;
    }
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match put_inner(&state, &app.slug, &query, content_type.as_deref(), &bytes).await {
        Ok(size) => {
            // Keep the stored sweep measurement current: the prospective gate
            // above read it, so the upload's bytes join it now. Best effort —
            // a write failure here must not fail an upload that succeeded.
            adjust_r2_measured(&state.mcp.pool, &app.id, size as i64).await;
            let summary = super::super::console::audit_summary(
                &account,
                &format!("r2_put {} {} ({} bytes)", query.bucket, query.key, size),
                admin,
            );
            if let Err(res) = super::super::console::audit_mutation(
                &state.mcp.pool, &app.id, &account, "r2_put", &summary, "ok", None, 0, admin,
            )
            .await
            {
                return *res;
            }
            super::ok(serde_json::json!({"key": query.key, "size": size}))
        }
        Err(err) => {
            let msg = format!("{err:#}");
            let summary = super::super::console::audit_summary(
                &account,
                &format!("r2_put {} {}", query.bucket, query.key),
                admin,
            );
            super::super::console::audit_console_refusal(
                &state.mcp.pool, &app.id, &account, "r2_put", &summary, None, 0, admin,
            )
            .await;
            status_for(&msg, "Couldn't upload right now. Try again.")
        }
    }
}

async fn put_inner(
    state: &super::AppState,
    slug: &str,
    query: &KeyQuery,
    content_type: Option<&str>,
    bytes: &[u8],
) -> anyhow::Result<usize> {
    let buckets = declared_buckets(state, slug).await?;
    let bucket = find_bucket(&buckets, &query.bucket)?;
    // Validated before anything is staged or passed to celld.
    let _full = object_key(slug, bucket, &query.key)?;
    if let Some(ct) = content_type {
        if !r2_header_ok(ct) {
            anyhow::bail!("invalid content type");
        }
    }
    let size = bytes.len();
    // Spool to a file: `celld r2 put` reads `--path`, and the body must not
    // sit buffered twice for a 64 MiB upload.
    let spool = tempfile::NamedTempFile::new()?;
    tokio::fs::write(spool.path(), bytes).await?;
    let spool_path = spool.path().to_string_lossy().into_owned();
    let settings = super::deploy_settings(state)?;
    let storage = &settings.storage;
    let creds = scoped_creds(state, slug).await?;
    let args = celld_put_argv(
        &storage.bucket_url(slug),
        &storage.endpoint,
        &storage.region,
        &bucket.bucket,
        &query.key,
        &spool_path,
        content_type,
    );
    run_celld(state, &args, &celld_env(storage, &creds), CELLD_PUT_TIMEOUT).await?;
    Ok(size)
}

async fn api_delete(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    body: axum::Json<DeleteBody>,
) -> Response {
    let (account, app, admin) = match console_app(&state, &headers, &slug, None).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    match delete_inner(&state, &app.slug, &body).await {
        Ok(count) => {
            // The stored measurement no longer reflects reality (celld does
            // not report freed bytes): clear it so the next upload re-sizes
            // live instead of refusing against a stale-high number. Best
            // effort, like the put-side adjustment.
            clear_r2_measured(&state.mcp.pool, &app.id).await;
            let summary = super::super::console::audit_summary(
                &account,
                &format!("r2_delete {} ({} keys)", body.bucket, count),
                admin,
            );
            if let Err(res) = super::super::console::audit_mutation(
                &state.mcp.pool, &app.id, &account, "r2_delete", &summary, "ok", None, 0, admin,
            )
            .await
            {
                return *res;
            }
            super::ok(serde_json::json!({"deleted": count}))
        }
        Err(err) => {
            let msg = format!("{err:#}");
            let summary = super::super::console::audit_summary(
                &account,
                &format!("r2_delete {}", body.bucket),
                admin,
            );
            super::super::console::audit_console_refusal(
                &state.mcp.pool, &app.id, &account, "r2_delete", &summary, None, 0, admin,
            )
            .await;
            status_for(&msg, "Couldn't delete right now. Try again.")
        }
    }
}

async fn delete_inner(
    state: &super::AppState,
    slug: &str,
    body: &DeleteBody,
) -> anyhow::Result<usize> {
    let buckets = declared_buckets(state, slug).await?;
    let bucket = find_bucket(&buckets, &body.bucket)?;
    if body.keys.is_empty() || body.keys.len() > R2_DELETE_LIMIT {
        anyhow::bail!("select 1 to {R2_DELETE_LIMIT} objects");
    }
    for key in &body.keys {
        if !r2_key_ok(key) {
            anyhow::bail!("invalid key");
        }
    }
    let settings = super::deploy_settings(state)?;
    let storage = &settings.storage;
    let creds = scoped_creds(state, slug).await?;
    let args = celld_delete_argv(
        &storage.bucket_url(slug),
        &storage.endpoint,
        &storage.region,
        &bucket.bucket,
        &body.keys,
    );
    run_celld(state, &args, &celld_env(storage, &creds), CELLD_TIMEOUT).await?;
    Ok(body.keys.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stored forms captured from celld 0.6.1: `celld r2 put` for each key,
    /// then a raw `ListObjectsV2`. The encoder must reproduce them byte for
    /// byte, and the decoder must bring them back — a browser that lists S3
    /// directly depends on both. (Vectors from Noite @ 58a977a.)
    const VECTORS: [(&str, &str); 29] = [
        ("a//b", "a/%/b"),
        ("photos/", "photos/%"),
        ("a//", "a/%/%"),
        ("accent-é", "accent-%C3%A9"),
        ("back\\slash", "back%5Cslash"),
        ("brace{}", "brace%7B%7D"),
        ("brack[et]", "brack%5Bet%5D"),
        ("caret^", "caret%5E"),
        ("grave`x", "grave%60x"),
        ("hash#tag", "hash%23tag"),
        ("lt<gt>", "lt%3Cgt%3E"),
        ("percent%25", "percent%2525"),
        ("pic name+café (1).png", "pic name+caf%C3%A9 (1).png"),
        ("pipe|x", "pipe%7Cx"),
        ("q?mark", "q%3Fmark"),
        ("quote\"z", "quote%22z"),
        ("star*", "star%2A"),
        ("tilde~x", "tilde%7Ex"),
        ("deep/nested/x.txt", "deep/nested/x.txt"),
        // Left literal by celld.
        ("sp ace.txt", "sp ace.txt"),
        ("plus+sign", "plus+sign"),
        ("amp&and", "amp&and"),
        ("at@x", "at@x"),
        ("colon:y", "colon:y"),
        ("comma,x", "comma,x"),
        ("bang!", "bang!"),
        ("semi;x", "semi;x"),
        ("dollar$x", "dollar$x"),
        ("paren(1)", "paren(1)"),
    ];

    #[test]
    fn keys_encode_and_decode_the_way_celld_stores_them() {
        for (key, stored) in VECTORS {
            assert_eq!(r2_encode(key), stored, "encode {key}");
            assert_eq!(r2_decode(stored), key, "decode {stored}");
        }
    }

    #[test]
    fn listing_prefixes_keep_their_trailing_separator() {
        // A key `photos/` is stored as `photos/%`, but a *listing* of the
        // folder `photos/` must ask for `photos/` — the separator, not an
        // empty segment — or every object under it is missed.
        assert_eq!(r2_encode_prefix(""), "");
        assert_eq!(r2_encode_prefix("dir/"), "dir/");
        assert_eq!(r2_encode_prefix("e2e-probe/"), "e2e-probe/");
        assert_eq!(r2_encode_prefix("my docs/"), "my docs/");
        assert_eq!(r2_encode_prefix("café/"), "caf%C3%A9/");
        assert_eq!(r2_encode_prefix("a//"), "a/%/");
        assert_eq!(r2_encode_prefix("a//b/"), "a/%/b/");
        assert_eq!(r2_encode("photos/"), "photos/%");
    }

    #[test]
    fn malformed_escapes_stay_literal() {
        // Another tool wrote an object under a key celld cannot name; the page
        // still renders instead of failing.
        assert_eq!(r2_decode_segment("a%ZZ"), "a%ZZ");
        assert_eq!(r2_decode_segment("50%"), "50%");
    }

    #[test]
    fn write_keys_reject_traversal_absolute_and_control_characters() {
        assert!(r2_key_ok("dir/file.txt"));
        assert!(r2_key_ok("folder/"));
        assert!(r2_key_ok("a//b"), "an empty segment is legal to read");
        assert!(!r2_key_ok(""));
        assert!(!r2_key_ok("/etc/passwd"));
        assert!(!r2_key_ok("../escape"));
        assert!(!r2_key_ok("a/../b"));
        assert!(!r2_key_ok("a\nb"));
        assert!(!r2_key_ok(&"x".repeat(R2_MAX_KEY_BYTES + 1)));
        assert!(r2_key_ok(&"x".repeat(R2_MAX_KEY_BYTES)));
    }
    #[test]
    fn write_keys_reject_leading_dash_option_injection() {
        // Keys travel positionally to `celld r2 put` / `celld r2 delete`
        // (celld 0.6.2 honors no `--` separator): a leading `-` would parse
        // as an option. Dashes elsewhere are inert and stay legal.
        assert!(!r2_key_ok("-evil"));
        assert!(!r2_key_ok("-"));
        assert!(!r2_prefix_ok("-evil/"));
        assert!(r2_key_ok("a-b"));
        assert!(r2_key_ok("dir/-file"));
        let bucket = R2Bucket {
            binding: "F".to_string(),
            bucket: "files".to_string(),
        };
        assert!(object_key("my-app", &bucket, "-evil").is_err());
    }

    #[test]
    fn prefixes_allow_the_bucket_root_only_as_empty() {
        assert!(r2_prefix_ok(""));
        assert!(r2_prefix_ok("dir/"));
        assert!(r2_prefix_ok("a/b/"));
        assert!(!r2_prefix_ok("/abs/"));
        assert!(!r2_prefix_ok("../"));
        assert!(!r2_prefix_ok("a/../"));
    }

    #[test]
    fn content_types_reject_header_injection() {
        assert!(r2_header_ok("image/png"));
        assert!(!r2_header_ok("image/png\r\nx: y"));
        assert!(!r2_header_ok(""));
    }

    #[test]
    fn buckets_come_from_declared_r2_buckets() {
        let value = serde_json::json!({
            "r2_buckets": [
                {"binding": "FILES", "bucket_name": "files"},
                {"binding": "AVATARS", "bucket_name": "avatars"},
            ],
        });
        let buckets = r2_buckets(&value).unwrap();
        assert_eq!(
            buckets,
            vec![
                R2Bucket {
                    binding: "FILES".to_string(),
                    bucket: "files".to_string()
                },
                R2Bucket {
                    binding: "AVATARS".to_string(),
                    bucket: "avatars".to_string()
                },
            ]
        );
        assert!(r2_buckets(&serde_json::json!({})).unwrap().is_empty());
        assert!(find_bucket(&buckets, "files").is_ok());
        assert!(find_bucket(&buckets, "other").is_err());
    }

    #[test]
    fn bucket_entries_reject_option_injection_and_bad_shapes() {
        for bad in [
            serde_json::json!({"r2_buckets": [{"binding": "F", "bucket_name": "-evil"}]}),
            serde_json::json!({"r2_buckets": [{"binding": "F", "bucket_name": "UPPER"}]}),
            serde_json::json!({"r2_buckets": [{"binding": "has space", "bucket_name": "ok"}]}),
            serde_json::json!({"r2_buckets": [{"binding": "F"}]}),
            serde_json::json!({"r2_buckets": "nope"}),
        ] {
            assert!(r2_buckets(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn manifest_r2_bindings_skip_non_r2_and_invalid_entries() {
        let manifest = serde_json::json!({
            "raw_metadata": {
                "bindings": [
                    {"type": "r2_bucket", "name": "FILES", "bucket_name": "files"},
                    {"type": "d1", "name": "DB", "database_name": "app-db"},
                    {"type": "assets"},
                    {"type": "r2_bucket", "name": "BAD", "bucket_name": "-evil"},
                    {"type": "r2_bucket", "name": "NO_BUCKET"},
                    {"type": "kv", "name": "KV"},
                ],
                "vars": {"SECRET": "should-never-leak"},
            },
        });
        let buckets = parse_manifest_r2(&manifest);
        // Only the valid R2 entry survives; D1/assets/KV/invalid entries
        // are skipped, and `vars` are never read.
        assert_eq!(
            buckets,
            vec![R2Bucket {
                binding: "FILES".to_string(),
                bucket: "files".to_string()
            },]
        );
        assert!(parse_manifest_r2(&serde_json::json!({})).is_empty());
        assert!(parse_manifest_r2(&serde_json::json!({"raw_metadata": {}})).is_empty());
    }

    #[test]
    fn keys_never_escape_the_bucket_prefix() {
        let bucket = R2Bucket {
            binding: "F".to_string(),
            bucket: "files".to_string(),
        };
        let full = object_key("my-app", &bucket, "a/../b").unwrap_err();
        let _ = full;
        for key in ["a/b.txt", "photos/", "pic name+café (1).png", "a//b"] {
            let full = object_key("my-app", &bucket, key).unwrap();
            assert!(
                full.starts_with("fleets/my-app/r2/files/"),
                "escaped: {full}"
            );
            assert!(!full.contains("../"), "traversal: {full}");
        }
        assert!(object_key("my-app", &bucket, "../escape").is_err());
        assert!(object_key("my-app", &bucket, "/abs").is_err());
    }

    #[test]
    fn celld_argv_pins_bucket_url_and_positions() {
        let put = celld_put_argv(
            "s3://bucket/fleets/my-app",
            "https://ep",
            "auto",
            "files",
            "a/b.txt",
            "/tmp/spool",
            Some("image/png"),
        );
        assert_eq!(
            put,
            vec![
                "r2",
                "put",
                "files",
                "a/b.txt",
                "--path",
                "/tmp/spool",
                "--content-type",
                "image/png",
                "--bucket",
                "s3://bucket/fleets/my-app",
                "--endpoint",
                "https://ep",
                "--region",
                "auto",
            ]
        );
        let delete = celld_delete_argv(
            "s3://bucket/fleets/my-app",
            "https://ep",
            "auto",
            "files",
            &["a.txt".to_string(), "b.txt".to_string()],
        );
        assert_eq!(
            delete,
            vec![
                "r2",
                "delete",
                "files",
                "a.txt",
                "b.txt",
                "--bucket",
                "s3://bucket/fleets/my-app",
                "--endpoint",
                "https://ep",
                "--region",
                "auto",
            ]
        );
    }

    #[test]
    fn raw_serving_is_nosniff_and_sandboxes_documents() {
        let res = raw_response(Some("image/png"), "a.png", vec![1, 2, 3]);
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert!(!res.headers().contains_key("content-security-policy"));
        for ct in ["text/html", "image/svg+xml", "application/xml", "text/xml"] {
            let res = raw_response(Some(ct), "a", vec![]);
            assert_eq!(
                res.headers()["content-security-policy"],
                "sandbox",
                "missing sandbox for {ct}"
            );
        }
        let res = raw_response(Some("evil\r\nx: y"), "a", vec![]);
        assert_eq!(res.headers()["content-type"], "application/octet-stream");
    }

    fn stored(key: &str) -> crate::store::s3::S3Object {
        crate::store::s3::S3Object {
            key: key.to_string(),
            last_modified: "2026-10-06T11:00:57.528Z".to_string(),
            size: 15,
            etag: "etag".to_string(),
        }
    }

    #[test]
    fn a_page_splits_into_folders_then_files() {
        let base = "fleets/app/r2/bucket/";
        let prefixes = vec![
            format!("{base}dir/"),
            format!("{base}a/"),
            "fleets/other/r2/bucket/dir/".to_string(),
        ];
        let objects = vec![
            stored(&format!("{base}dir/hello.txt")),
            stored(&format!("{base}photos/%")),
            stored(&format!("{base}pic name+caf%C3%A9 (1).png")),
        ];
        let (folders, objects) = split_page(base, "", &prefixes, &objects);
        let names: Vec<String> = folders.iter().map(|folder| folder.name.clone()).collect();
        assert_eq!(names, ["a", "dir"]);
        assert_eq!(folders[0].prefix, "a/");
        let keys: Vec<String> = objects.iter().map(|object| object.key.clone()).collect();
        assert_eq!(keys, ["dir/hello.txt", "pic name+café (1).png"]);
        assert_eq!(objects[0].name, "hello.txt");
        assert_eq!(objects[0].size, 15);
    }

    #[test]
    fn an_empty_segment_folder_browses_as_empty() {
        let base = "fleets/app/r2/bucket/a/";
        let prefixes = vec![format!("{base}%/")];
        let (folders, objects) =
            split_page(base, "a/", &prefixes, &[stored(&format!("{base}%/b"))]);
        assert_eq!(folders[0].name, "(empty)");
        assert_eq!(folders[0].prefix, "a//");
        // A key with an empty segment round-trips through the parent prefix.
        assert_eq!(objects[0].key, "a//b");
        assert_eq!(objects[0].name, "b");
    }
    /// Backend failures (the live R2 501 whose XML names `NotImplemented`)
    /// keep their detail in `message` but show a friendly `user_message`:
    /// raw store XML must never reach the UI. Validation errors still show
    /// their own message.
    #[tokio::test]
    async fn backend_failures_show_a_friendly_user_message() {
        async fn body(res: Response) -> (StatusCode, serde_json::Value) {
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            (status, serde_json::from_str(&text).unwrap())
        }
        let xml = "s3 list fleets/demo/r2/files/ failed: HTTP 501 \
            <Code>NotImplemented</Code>\
            <Message>ListObjectsV2 search parameter X-Amz-Security-Token not implemented</Message>";
        let (status, json) = body(status_for(
            xml,
            "Couldn't list this bucket right now. Try again.",
        ))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(json["message"].as_str().unwrap().contains("NotImplemented"));
        assert_eq!(
            json["user_message"],
            "Couldn't list this bucket right now. Try again."
        );
        assert!(!json["user_message"].as_str().unwrap().contains('<'));

        // Validation errors keep their own message as the user message.
        let (status, json) = body(status_for(
            "invalid prefix",
            "Couldn't list this bucket right now. Try again.",
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["message"], "invalid prefix");
        assert_eq!(json["user_message"], "invalid prefix");
    }

    #[test]
    fn upload_gate_fits_measured_plus_size_against_the_cap() {
        let free = crate::limits::FREE_R2_BYTES;
        assert!(!r2_upload_over_cap(0, 1, free));
        assert!(!r2_upload_over_cap(free - 11, 11, free));
        assert!(r2_upload_over_cap(free - 10, 11, free));
        assert!(r2_upload_over_cap(free, 1, free));
        // Saturating: a corrupted-huge measurement refuses, never wraps.
        assert!(r2_upload_over_cap(u64::MAX, 1, free));
    }

    async fn gate_state(measured: Option<i64>) -> (crate::mcp::McpState, crate::apps::App) {
        let pool = crate::auth::test_support::test_pool().await;
        let (config, _codes) = crate::auth::test_support::test_config();
        sqlx::query(
            "INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) \
             VALUES ('a1', 'ana@example.com', 'free', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
             last_activity_at, created_at, r2_measured_bytes) \
             VALUES ('app1', 'shop', 'a1', 300001, 20001, 20002, 0, 0, ?1)",
        )
        .bind(measured)
        .execute(&pool)
        .await
        .unwrap();
        let auth = crate::auth::AuthState {
            pool: pool.clone(),
            config,
        };
        let state = crate::mcp::McpState::for_tests(pool, auth);
        let app = crate::apps::get_by_slug(&state.pool, "shop")
            .await
            .unwrap()
            .unwrap();
        (state, app)
    }

    async fn refusal_body(res: Response) -> (StatusCode, serde_json::Value) {
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn upload_gate_refuses_up_front_past_the_cap() {
        let free = crate::limits::FREE_R2_BYTES;
        let (state, app) = gate_state(Some((free - 10) as i64)).await;
        // Fits: no refusal.
        assert!(refuse_r2_over_cap(&state, &app, 10).await.is_none());
        // One byte past: 429 with the quota shape the UI's Upgrade button
        // reads.
        let res = refuse_r2_over_cap(&state, &app, 11)
            .await
            .expect("an over-cap upload is refused up front");
        let (status, json) = refusal_body(res).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(json["error"], "quota_exceeded");
        assert_eq!(json["quota"], "storage_bytes");
        assert!(json["upgrade_url"].is_string());
    }

    #[tokio::test]
    async fn upload_gate_sizes_live_when_nothing_is_stored() {
        let free = crate::limits::FREE_R2_BYTES;
        let (state, app) = gate_state(None).await;
        state
            .store
            .put("fleets/shop/r2/assets/logo.png", vec![0u8; 100])
            .await
            .unwrap();
        // The live 100 bytes are measured and stored by the gate itself.
        assert!(refuse_r2_over_cap(&state, &app, 10).await.is_none());
        let stored: Option<i64> =
            sqlx::query_scalar("SELECT r2_measured_bytes FROM app WHERE id = 'app1'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(stored, Some(100));
        // And the fresh measurement gates the next upload.
        let res = refuse_r2_over_cap(&state, &app, free)
            .await
            .expect("stored bytes plus a cap-sized upload refuses");
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn put_and_delete_keep_the_measurement_current() {
        let (state, app) = gate_state(Some(100)).await;
        adjust_r2_measured(&state.pool, &app.id, 50).await;
        let stored: i64 = sqlx::query_scalar("SELECT r2_measured_bytes FROM app WHERE id = 'app1'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(stored, 150, "an upload's bytes join the measurement");
        clear_r2_measured(&state.pool, &app.id).await;
        let stored: Option<i64> =
            sqlx::query_scalar("SELECT r2_measured_bytes FROM app WHERE id = 'app1'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(stored, None, "a delete invalidates the stale number");
    }
}
