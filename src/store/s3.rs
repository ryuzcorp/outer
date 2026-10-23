// Derived from noite apps/runner/src/host/s3.rs @ 58a977a (Apache-2.0).
// Outer keeps the presigned-URL request signing, streaming upload/download,
// paginated LIST and retry logic; bucket creation/versioning, copy,
// head-bucket and Noite's Config coupling are dropped.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use super::S3Config;

/// How long a presigned URL stays valid. The signature is checked when the
/// request starts, so one generous value covers slow streaming uploads.
const PRESIGN_SECS: u64 = 3600;

/// How many times an idempotent S3 call is attempted before giving up.
const S3_MAX_ATTEMPTS: u32 = 3;

/// True when an S3 error means "the object does not exist", as opposed
/// to the store being unreachable. Native failures carry `HTTP 404` plus the
/// store's `NoSuchKey` body. Callers that treat "missing" as "start fresh"
/// must not do so on a transport error, or they overwrite good state.
///
/// The matchers are status-anchored on purpose: the target key rides inside
/// the same message, so a bare `"404"` reads a transport failure on a
/// numeric key as "missing". `(404)` is the legacy aws-CLI shape, kept so
/// old snapshots still count as absent.
pub fn is_not_found(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("HTTP 404")
        || text.contains("(404)")
        || text.contains("NoSuchKey")
        || text.contains("NoSuchBucket")
        || text.contains("NotFound")
}

/// S3/R2 backend: rusty-s3 signs presigned URLs, one pooled reqwest client
/// sends them. Region `auto` and [`UrlStyle::Path`] are what R2 expects;
/// a session token (R2 temporary credentials) is attached to every signed
/// request as `x-amz-security-token` via [`Credentials::new_with_token`].
#[derive(Debug, Clone)]
pub struct S3Backend {
    bucket: Bucket,
    creds: Credentials,
    bucket_name: String,
    client: reqwest::Client,
}

impl S3Backend {
    pub fn new(cfg: &S3Config) -> anyhow::Result<Self> {
        let endpoint: reqwest::Url = cfg
            .endpoint
            .parse()
            .with_context(|| format!("S3 endpoint is not a URL: {0}", cfg.endpoint))?;
        let bucket = Bucket::new(
            endpoint,
            UrlStyle::Path,
            cfg.bucket.clone(),
            cfg.region.clone(),
        )
        .context("S3 bucket handle")?;
        let creds = match &cfg.session_token {
            Some(token) => Credentials::new_with_token(
                cfg.access_key_id.clone(),
                cfg.secret_access_key.clone(),
                token.clone(),
            ),
            None => Credentials::new(cfg.access_key_id.clone(), cfg.secret_access_key.clone()),
        };
        let client = reqwest::Client::builder()
            .build()
            .context("reqwest client")?;
        Ok(Self {
            bucket,
            creds,
            bucket_name: cfg.bucket.clone(),
            client,
        })
    }

    fn get_url(&self, key: &str) -> reqwest::Url {
        self.bucket
            .get_object(Some(&self.creds), key)
            .sign(Duration::from_secs(PRESIGN_SECS))
    }

    fn put_url(&self, key: &str) -> reqwest::Url {
        self.bucket
            .put_object(Some(&self.creds), key)
            .sign(Duration::from_secs(PRESIGN_SECS))
    }

    fn delete_url(&self, key: &str) -> reqwest::Url {
        self.bucket
            .delete_object(Some(&self.creds), key)
            .sign(Duration::from_secs(300))
    }

    /// Whole-object download into memory (manifests, meta, owner claims).
    /// Streamed into the buffer chunk by chunk; retried like the file
    /// download, never on 4xx.
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match with_s3_retry("download", key, || self.get_once(key)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_once(&self, key: &str) -> Result<Vec<u8>, S3Fail> {
        use futures::StreamExt;
        let url = self.get_url(key);
        let resp = match self
            .client
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                return Err(S3Fail::Retryable(anyhow::anyhow!(
                    "s3 download {key} transport: {e:#}"
                )));
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = bounded_text_or_empty(resp, Duration::from_secs(30)).await;
            return Err(classify_status("download", key, status, &body));
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| S3Fail::Retryable(anyhow::anyhow!("s3 download {key} body: {e:#}")))?;
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Stream one object to `dest` (bundles, snapshots). The body is
    /// written chunk by chunk, never buffered whole. Returns false on 404.
    ///
    /// Retried on 5xx/SlowDown/transport only, never on 4xx: a missing key
    /// comes back identically every time.
    pub async fn get_to_file(&self, key: &str, dest: &Path) -> anyhow::Result<bool> {
        match with_s3_retry("download", key, || self.get_to_file_once(key, dest)).await {
            Ok(()) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn get_to_file_once(&self, key: &str, dest: &Path) -> Result<(), S3Fail> {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let url = self.get_url(key);
        let resp = match self
            .client
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                return Err(S3Fail::Retryable(anyhow::anyhow!(
                    "s3 download {key} transport: {e:#}"
                )));
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = bounded_text_or_empty(resp, Duration::from_secs(30)).await;
            return Err(classify_status("download", key, status, &body));
        }
        // Created per attempt, so a retry truncates the partial body instead
        // of appending a second copy after it.
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| S3Fail::Final(anyhow::anyhow!("create {0}: {e:#}", dest.display())))?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| S3Fail::Retryable(anyhow::anyhow!("s3 download {key} body: {e:#}")))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| S3Fail::Final(anyhow::anyhow!("write download: {e:#}")))?;
        }
        file.flush()
            .await
            .map_err(|e| S3Fail::Final(anyhow::anyhow!("flush download: {e:#}")))?;
        Ok(())
    }

    /// Small-object upload (manifests, meta, owner claims).
    pub async fn put(&self, key: &str, body: Vec<u8>) -> anyhow::Result<()> {
        // Cloned per attempt: a full-key PUT is an overwrite, so repeating
        // one is idempotent, but the body is consumed by the send.
        with_s3_retry("upload", key, || {
            let body = body.clone();
            self.put_bytes_once(key, body)
        })
        .await
    }

    async fn put_bytes_once(&self, key: &str, body: Vec<u8>) -> Result<(), S3Fail> {
        let url = self.put_url(key);
        let len = body.len();
        let resp = match self
            .client
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                return Err(S3Fail::Retryable(anyhow::anyhow!(
                    "s3 upload {key} transport: {e:#}"
                )));
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = bounded_text_or_empty(resp, Duration::from_secs(20)).await;
            return Err(classify_status("upload", key, status, &body));
        }
        Ok(())
    }

    /// Stream one file to `key` with its length up front (S3 needs it).
    /// The file is read in 32 KiB pieces as the request drains, so bundles
    /// and snapshots never sit whole in memory.
    ///
    /// Retried like the download: a full-key PUT is an overwrite, so
    /// repeating one is idempotent.
    pub async fn put_file(&self, key: &str, src: &Path) -> anyhow::Result<()> {
        with_s3_retry("upload", key, || self.put_file_once(key, src)).await
    }

    async fn put_file_once(&self, key: &str, src: &Path) -> Result<(), S3Fail> {
        // Opened per attempt: the streaming body consumes the file, so a
        // retry must re-read it instead of uploading the first attempt's EOF.
        let file = tokio::fs::File::open(src)
            .await
            .map_err(|e| S3Fail::Final(anyhow::anyhow!("open {0}: {e:#}", src.display())))?;
        let len = file
            .metadata()
            .await
            .map_err(|e| S3Fail::Final(anyhow::anyhow!("stat upload: {e:#}")))?
            .len();
        let url = self.put_url(key);
        let resp = match self
            .client
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(reqwest::Body::wrap_stream(FileChunks { file }))
            .timeout(Duration::from_secs(70))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                return Err(S3Fail::Retryable(anyhow::anyhow!(
                    "s3 upload {key} transport: {e:#}"
                )));
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = bounded_text_or_empty(resp, Duration::from_secs(70)).await;
            return Err(classify_status("upload", key, status, &body));
        }
        Ok(())
    }

    /// Delete one key. A missing key is Ok.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let url = self.delete_url(key);
        let resp = self
            .client
            .delete(url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("s3 DELETE")?;
        // Deleting a missing key is a no-op upstream too (`delete-object` on
        // a missing key returns 204), so 404 joins the success set.
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(s3_err("delete", key, resp, Duration::from_secs(20)).await)
    }

    /// All keys under `prefix`, paginated internally. A retry restarts from
    /// the first page — pages are small and a partial merge could otherwise
    /// duplicate keys.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        with_s3_retry("list", prefix, || self.list_once(prefix)).await
    }

    async fn list_once(&self, prefix: &str) -> Result<Vec<String>, S3Fail> {
        let mut keys = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.creds));
            action.with_prefix(prefix.to_string());
            if let Some(t) = continuation.take() {
                action.with_continuation_token(t);
            }
            let url = action.sign(Duration::from_secs(300));
            let resp = match self
                .client
                .get(url)
                .timeout(Duration::from_secs(20))
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    return Err(S3Fail::Retryable(anyhow::anyhow!(
                        "s3 list {prefix} transport: {e:#}"
                    )));
                }
            };
            if !resp.status().is_success() {
                let status = resp.status();
                let body = bounded_text_or_empty(resp, Duration::from_secs(20)).await;
                return Err(classify_status("list", prefix, status, &body));
            }
            let text = bounded_text(resp, Duration::from_secs(20), "s3 LIST body")
                .await
                .map_err(S3Fail::Final)?;
            let (page_keys, next) = list_page_from_xml(&text).map_err(S3Fail::Final)?;
            keys.extend(page_keys);
            continuation = next;
            if continuation.is_none() {
                break;
            }
        }
        Ok(keys)
    }

    /// Recursive delete of one prefix. Empty prefixes are a no-op, so
    /// retries after a partial delete stay quiet.
    pub async fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
        let keys = self.list(prefix).await?;
        self.delete_keys(&keys).await
    }

    /// Batch delete (one `DeleteObjects` POST per 1000 keys). A 200 with
    /// per-key `Error` entries is a failure, not a success: access problems
    /// surface that way, and ignoring them would pretend a purge worked.
    async fn delete_keys(&self, keys: &[String]) -> anyhow::Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        for chunk in keys.chunks(1000) {
            self.delete_keys_chunk(chunk.to_vec()).await?;
        }
        Ok(())
    }

    /// One `DeleteObjects` POST for a single ≤1000-key chunk.
    async fn delete_keys_chunk(&self, chunk: Vec<String>) -> anyhow::Result<()> {
        use rusty_s3::actions::{DeleteObjectsResponse, ObjectIdentifier};
        let ids: Vec<ObjectIdentifier> = chunk
            .iter()
            .map(|k| ObjectIdentifier::new(k.clone()))
            .collect();
        let action = self.bucket.delete_objects(Some(&self.creds), ids.iter());
        let url = action.sign(Duration::from_secs(300));
        let (body, md5) = action.body_with_md5();
        let resp = self
            .client
            .post(url)
            .header("Content-MD5", md5)
            .body(body)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("s3 DeleteObjects")?;
        if !resp.status().is_success() {
            return Err(s3_err(
                "delete-keys",
                &self.bucket_name,
                resp,
                Duration::from_secs(60),
            )
            .await);
        }
        let text = bounded_text(resp, Duration::from_secs(60), "s3 DeleteObjects body").await?;
        let parsed =
            DeleteObjectsResponse::parse(&text).with_context(|| "s3 DeleteObjects XML parse")?;
        if let Some(first) = parsed.errors.first() {
            anyhow::bail!(
                "s3 delete-keys {0} failed: {1} {2}",
                self.bucket_name,
                first.code,
                first.message
            );
        }
        Ok(())
    }
}

/// One LIST page parsed from its XML: the keys on this page plus the token
/// that continues it (`None` when the listing is complete).
fn list_page_from_xml(text: &str) -> anyhow::Result<(Vec<String>, Option<String>)> {
    let parsed = rusty_s3::actions::ListObjectsV2::parse_response(text)
        .with_context(|| "s3 LIST XML parse")?;
    Ok((
        parsed.contents.into_iter().map(|o| o.key).collect(),
        parsed.next_continuation_token,
    ))
}

/// Chunked file body for PUT: 32 KiB pieces read as the request drains, so
/// bundles and snapshots never sit whole in memory.
struct FileChunks {
    file: tokio::fs::File,
}

impl futures::Stream for FileChunks {
    type Item = std::io::Result<bytes::Bytes>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        use tokio::io::AsyncRead;
        let mut buf = [0u8; 32 * 1024];
        let mut read_buf = tokio::io::ReadBuf::new(&mut buf);
        match std::pin::Pin::new(&mut self.file).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                if filled.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(bytes::Bytes::copy_from_slice(filled))))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Rich S3 failure: HTTP status plus the store's XML error body.
/// [`is_not_found`] matches on the status-anchored `HTTP 404`/`NoSuchKey`,
/// so "missing" vs "unreachable" keeps working without knowing the transport.
async fn s3_err(
    op: &str,
    target: &str,
    resp: reqwest::Response,
    timeout: Duration,
) -> anyhow::Error {
    let status = resp.status().as_u16();
    let body = bounded_text_or_empty(resp, timeout).await;
    s3_err_msg(op, target, status, &body)
}

/// Body read bounded by the request's own timeout. reqwest's total timeout
/// already covers streaming, but the bound stays explicit at each call site
/// instead of relying on client internals — a stalled body after delivered
/// headers must fail like a stalled send, never hang the caller.
async fn bounded_text(
    resp: reqwest::Response,
    timeout: Duration,
    what: &str,
) -> anyhow::Result<String> {
    let bytes = tokio::time::timeout(timeout, resp.bytes())
        .await
        .map_err(|_| anyhow::anyhow!("{0} timed out after {1}s", what, timeout.as_secs()))?
        .with_context(|| what.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Same bound for error bodies, which stay best-effort: garbage (or no body
/// at all) must not hide the status the caller already has.
async fn bounded_text_or_empty(resp: reqwest::Response, timeout: Duration) -> String {
    tokio::time::timeout(timeout, resp.bytes())
        .await
        .ok()
        .and_then(|result| result.ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// Same failure without the response: the retryable attempts classify before
/// they would consume the body, so the message is built from parts.
fn s3_err_msg(op: &str, target: &str, status: u16, body: &str) -> anyhow::Error {
    let snippet: String = body.trim().chars().take(300).collect();
    anyhow::anyhow!("s3 {op} {target} failed: HTTP {status} {snippet}")
}

/// Backoff between S3 attempts: 250 ms doubling per failure plus up to one
/// base of jitter, so a store restart does not get every caller retrying in
/// lockstep.
fn s3_retry_delay(failed: u32) -> Duration {
    let base_ms: u64 = 250 << failed.min(3);
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % (base_ms + 1))
        .unwrap_or(0);
    Duration::from_millis(base_ms + jitter_ms)
}

/// One S3 attempt's outcome. Only idempotent calls retry — GET, PUT of a full
/// key (an overwrite, never an append) and LIST — so a repeated attempt can
/// never duplicate or corrupt state.
enum S3Fail {
    /// Worth one more attempt: a 5xx, a SlowDown body, or a request that
    /// never reached the store.
    Retryable(anyhow::Error),
    /// Retrying cannot help: 4xx, auth, local disk, XML parse.
    Final(anyhow::Error),
}

/// Run `run` until it succeeds, fails finally, or exhausts
/// [`S3_MAX_ATTEMPTS`] on retryable errors. 4xx responses are never retried:
/// a missing key or a refused credential comes back identically every time.
async fn with_s3_retry<T, F, Fut>(op: &str, target: &str, mut run: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, S3Fail>>,
{
    let mut attempt: u32 = 0;
    loop {
        match run().await {
            Ok(value) => return Ok(value),
            Err(S3Fail::Final(e)) => return Err(e),
            Err(S3Fail::Retryable(e)) => {
                attempt += 1;

                if attempt >= S3_MAX_ATTEMPTS {
                    return Err(e);
                }
                // Visible at the default level: a retried attempt is a stall
                // (a 20 s list timeout, a 5xx) that a slow caller must be able
                // to name, not only a final failure.
                tracing::warn!(
                    op,
                    target,
                    attempt,
                    error = format!("{e:#}"),
                    "s3 attempt failed; retrying"
                );
                tokio::time::sleep(s3_retry_delay(attempt)).await;
            }
        }
    }
}

/// Classify a non-success status: 5xx and SlowDown may be retried, everything
/// else (including every other 4xx) is final.
fn classify_status(op: &str, target: &str, status: reqwest::StatusCode, body: &str) -> S3Fail {
    let err = s3_err_msg(op, target, status.as_u16(), body);
    if status.is_server_error() || body.contains("SlowDown") {
        S3Fail::Retryable(err)
    } else {
        S3Fail::Final(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(token: Option<&str>) -> S3Config {
        S3Config {
            endpoint: "https://account.eu.r2.cloudflarestorage.com".into(),
            region: "auto".into(),
            bucket: "outer-state".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            session_token: token.map(str::to_string),
        }
    }

    /// R2 needs region `auto`, path-style URLs, and the session token on
    /// every signed request. No network: signing is pure URL construction.
    #[test]
    fn signed_urls_are_path_style_and_carry_the_session_token() {
        let backend = S3Backend::new(&test_cfg(Some("session-token-abc"))).expect("backend");
        for url in [
            backend.get_url("outer/state/outer.sqlite.zst"),
            backend.put_url("outer/state/outer.sqlite.zst"),
            backend.delete_url("outer/state/outer.sqlite.zst"),
        ] {
            let url = url.as_str();
            // Path style: the host stays the endpoint host, the bucket is
            // the first path segment — not a virtual-host subdomain.
            assert!(
                url.starts_with("https://account.eu.r2.cloudflarestorage.com/outer-state/"),
                "not path style: {url}"
            );
            assert!(
                url.contains("X-Amz-Security-Token=session-token-abc"),
                "token missing: {url}"
            );
        }
        let listed = backend.bucket.list_objects_v2(Some(&backend.creds));
        let listed = listed.sign(Duration::from_secs(300));
        assert!(
            listed
                .as_str()
                .contains("X-Amz-Security-Token=session-token-abc"),
            "list must sign the token too: {listed}"
        );
    }

    #[test]
    fn signed_urls_omit_the_token_when_there_is_none() {
        let backend = S3Backend::new(&test_cfg(None)).expect("backend");
        let url = backend.get_url("outer/state/outer.sqlite.zst");
        assert!(
            !url.as_str().contains("Security-Token"),
            "unexpected token: {url}"
        );
        assert!(
            url.as_str()
                .starts_with("https://account.eu.r2.cloudflarestorage.com/outer-state/"),
            "not path style: {url}"
        );
    }

    /// Pagination: a truncated page yields its keys plus the token that
    /// continues it; the final page yields keys and no token.
    #[test]
    fn list_pagination_parses_pages_and_continuation() {
        let first = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>outer-state</Name><Prefix>git/demo/</Prefix><KeyCount>2</KeyCount><MaxKeys>2</MaxKeys>
<IsTruncated>true</IsTruncated>
<NextContinuationToken>tok-page-2</NextContinuationToken>
<Contents><Key>git/demo/MANIFEST.json</Key><LastModified>2026-10-22T00:00:00.000Z</LastModified><ETag>"a"</ETag><Size>12</Size></Contents>
<Contents><Key>git/demo/main/abc.bundle</Key><LastModified>2026-10-22T00:01:00.000Z</LastModified><ETag>"b"</ETag><Size>34</Size></Contents>
</ListBucketResult>"#;
        let (keys, next) = list_page_from_xml(first).expect("page 1 parses");
        assert_eq!(
            keys,
            vec!["git/demo/MANIFEST.json", "git/demo/main/abc.bundle"]
        );
        assert_eq!(next.as_deref(), Some("tok-page-2"));

        let last = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>outer-state</Name><Prefix>git/demo/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>
<IsTruncated>false</IsTruncated>
<Contents><Key>git/demo/main/def.bundle</Key><LastModified>2026-10-22T00:02:00.000Z</LastModified><ETag>"c"</ETag><Size>56</Size></Contents>
</ListBucketResult>"#;
        let (keys, next) = list_page_from_xml(last).expect("page 2 parses");
        assert_eq!(keys, vec!["git/demo/main/def.bundle"]);
        assert_eq!(next, None);

        assert!(list_page_from_xml("<garbage").is_err());
    }

    /// Only 5xx/SlowDown may be retried: a 4xx comes back identically every
    /// time, so retrying one just burns three timeouts before failing.
    #[test]
    fn retry_covers_5xx_and_slowdown_but_never_4xx() {
        use reqwest::StatusCode;
        assert!(matches!(
            classify_status(
                "download",
                "k",
                StatusCode::SERVICE_UNAVAILABLE,
                "ServiceUnavailable"
            ),
            S3Fail::Retryable(_)
        ));
        assert!(matches!(
            classify_status(
                "list",
                "p",
                StatusCode::SERVICE_UNAVAILABLE,
                "<Code>SlowDown</Code>"
            ),
            S3Fail::Retryable(_)
        ));
        assert!(matches!(
            classify_status(
                "download",
                "missing",
                StatusCode::NOT_FOUND,
                "<Code>NoSuchKey</Code>"
            ),
            S3Fail::Final(_)
        ));
        assert!(matches!(
            classify_status("upload", "k", StatusCode::FORBIDDEN, "AccessDenied"),
            S3Fail::Final(_)
        ));
    }

    /// A transport failure on a numeric key is unreachable, not missing: a
    /// bare-`"404"` match would read `s3 download 404.html failed: HTTP 500`
    /// as "start fresh" and let callers upload emptiness over good state.
    #[test]
    fn not_found_ignores_key_names_and_transport_failures() {
        let missing =
            anyhow::anyhow!("s3 download tip.bundle failed: HTTP 404 <Code>NoSuchKey</Code>");
        assert!(is_not_found(&missing));
        let legacy =
            anyhow::anyhow!("aws exit 1\nfatal error: An error occurred (404) when calling the HeadObject operation");
        assert!(is_not_found(&legacy));
        let numeric_key_on_500 =
            anyhow::anyhow!("s3 download 404.html failed: HTTP 500 ServiceUnavailable");
        assert!(!is_not_found(&numeric_key_on_500));
        let down =
            anyhow::anyhow!("s3 download tip.bundle transport: error sending request for url");
        assert!(!is_not_found(&down));
    }

    /// Backoff grows per failure and stays bounded: three attempts cost
    /// seconds, not minutes, on top of the per-attempt timeout.
    #[test]
    fn retry_delay_backs_off_with_jitter() {
        let first = s3_retry_delay(1);
        let second = s3_retry_delay(2);
        assert!(first >= Duration::from_millis(250));
        assert!(second >= Duration::from_millis(500));
        assert!(second <= Duration::from_millis(2500));
    }
}
