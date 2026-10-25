// Derived from noite apps/runner/src/host/s3.rs @ 58a977a (Apache-2.0).
// Outer keeps the presigned-URL request signing, streaming upload/download,
// paginated LIST and retry logic; bucket creation/versioning, copy,
// head-bucket and Noite's Config coupling are dropped.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use hmac::{Hmac, KeyInit, Mac};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};

use super::S3Config;

/// How long a presigned URL stays valid. The signature is checked when the
/// request starts, so one generous value covers slow streaming uploads.
const PRESIGN_SECS: u64 = 3600;

/// How many times an idempotent S3 call is attempted before giving up.
const S3_MAX_ATTEMPTS: u32 = 3;

/// Bound on TCP+TLS connect for every request, streamed or not.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// No-progress bound on streamed bodies (and on time-to-first-byte): each
/// body read must yield within this, so a large bundle on a slow link fails
/// only when it stalls — never at a fixed total wall. Small objects keep
/// their own total deadlines at each call site.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// True when an S3 error means "the object does not exist", as opposed
/// to the store being unreachable. Callers that treat "missing" as "start
/// fresh" must not do so on a transport error, or they overwrite good state.
///
/// Classification never scans free text: the target key rides inside every
/// message, so a `"NoSuchKey"`/`"NotFound"` substring match reads a 5xx or a
/// transport failure on a key named `NotFound...` as "missing", and the
/// manifest reader would restart the seq and lose history. Instead the
/// backend attaches an [`S3NotFound`] marker when it saw the object answer
/// 404 / `<Code>NoSuchKey</Code>`, and this matches that marker by downcast.
/// A `<Code>NoSuchBucket</Code>` body is a configuration error (wrong bucket),
/// never "missing": reading it as absent would start fresh and upload an
/// empty database over the good snapshot. Callers that learn "missing"
/// another way (a `Store::get` that returned `None`) build their error with
/// [`not_found_error`], so there is exactly one signal.
pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<S3NotFound>().is_some()
}

/// A "key is absent" error that [`is_not_found`] recognizes.
pub fn not_found_error(key: &str) -> anyhow::Error {
    anyhow::Error::new(S3NotFound).context(format!("{key} not found"))
}

/// Marker for "the store answered 404 / NoSuchKey for the object": the key
/// is absent, not the store unreachable. Attached at construction by
/// [`s3_err_msg`], matched by [`is_not_found`] through the anyhow chain.
/// Never attached for `<Code>NoSuchBucket</Code>`, which is a wrong-bucket
/// configuration error even when its status is 404.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct S3NotFound;

impl std::fmt::Display for S3NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "object not found")
    }
}

impl std::error::Error for S3NotFound {}

/// One object a delimited LIST page names, with the fields a browser shows.
#[derive(Debug, Clone)]
pub struct S3Object {
    pub key: String,
    pub last_modified: String,
    pub size: u64,
    pub etag: String,
}

/// One delimited LIST page: the objects in this folder, the folders below
/// it, and the token that continues the page (`None` when complete).
#[derive(Debug, Clone, Default)]
pub struct S3Page {
    pub objects: Vec<S3Object>,
    pub prefixes: Vec<String>,
    pub next: Option<String>,
}

/// One object's record as a HEAD reports it: the size (a listing carries it
/// too, but a single-key lookup has no listing) and the real `content-type`
/// header the writer stored for the object.
#[derive(Debug, Clone)]
pub struct S3Head {
    pub size: i64,
    pub content_type: Option<String>,
}

/// S3/R2 backend: rusty-s3 signs presigned URLs, one pooled reqwest client
/// sends them. Region `auto` and [`UrlStyle::Path`] are what R2 expects;
/// a session token (R2 temporary credentials) is signed into every
/// presigned object URL as the `X-Amz-Security-Token` query parameter by
/// [`Credentials::new_with_token`]. LIST is the exception: R2 rejects the
/// token as a presigned query parameter (501 on `ListObjectsV2`), so both
/// list paths sign a SigV4 `Authorization` header instead and send the
/// token as the signed `x-amz-security-token` header (see `list_request`).
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
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STREAM_IDLE_TIMEOUT)
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
    /// No total deadline: a large object on a slow link must fail only when
    /// it stalls. Connect and time-to-first-byte are bounded by the client's
    /// timeouts, and every body read must yield within `STREAM_IDLE_TIMEOUT`.
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
        // No `.timeout()` here: the client's connect/read timeouts bound
        // idle time, and a fixed total would fail a large body that is
        // still making progress.
        let resp = match self.client.get(url).send().await {
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
    /// The total bound scales with the file size (see `upload_timeout`): a
    /// large bundle on a slow link must fail only when it stalls below the
    /// floor throughput, not at a fixed wall. Connect and time-to-first-byte
    /// still come from the client's timeouts.
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
            .timeout(upload_timeout(len))
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

    /// SHA-256 of the empty body, sent as `x-amz-content-sha256` on every
    /// LIST GET and covered by the signature: a list has no payload to hash.
    const EMPTY_BODY_SHA256: &'static str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// One header-signed `ListObjectsV2` GET for `params` (the full
    /// `list-type=2` parameter set: prefix, delimiter, page size, …).
    ///
    /// R2 answers 501 `NotImplemented` when the session token rides the
    /// presigned *query* string on LIST, while GET/HEAD object presigns with
    /// the same parameter work — so LIST never uses the presigned
    /// `ListObjectsV2::sign`. The token travels as the
    /// `x-amz-security-token` *header* instead, covered by the SigV4
    /// `Authorization` header this builds. Object GET/HEAD/PUT/DELETE stay
    /// presigned: they work, and presigns need no clock.
    fn list_request(&self, params: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let now = chrono::Utc::now();
        let (url, authorization, amz_date) = Self::signed_list_request(
            self.bucket.base_url(),
            self.bucket.region(),
            &self.creds,
            params,
            &now.format("%Y%m%dT%H%M%SZ").to_string(),
            &now.format("%Y%m%d").to_string(),
        );
        let mut request = self
            .client
            .get(url)
            .header("authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", Self::EMPTY_BODY_SHA256);
        if let Some(token) = self.creds.token() {
            request = request.header("x-amz-security-token", token);
        }
        request
    }

    /// Pure SigV4 header signing for one LIST GET: the URL (token-free
    /// query), the `Authorization` value, and the `x-amz-date` value. Pure
    /// so tests pin the shape without a clock or a network.
    fn signed_list_request(
        base_url: &url::Url,
        region: &str,
        creds: &Credentials,
        params: &[(&str, &str)],
        amz_date: &str,
        date_stamp: &str,
    ) -> (url::Url, String, String) {
        let mut sorted: Vec<(&str, &str)> = params.to_vec();
        sorted.sort();
        let query = sorted
            .iter()
            .map(|(key, value)| format!("{}={}", Self::amz_encode(key), Self::amz_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut headers: Vec<(&str, String)> = vec![
            ("host", base_url.host_str().unwrap_or_default().to_string()),
            ("x-amz-content-sha256", Self::EMPTY_BODY_SHA256.to_string()),
            ("x-amz-date", amz_date.to_string()),
        ];
        if let Some(token) = creds.token() {
            headers.push(("x-amz-security-token", token.to_string()));
        }
        // Already ascending (`host < x-amz-content-sha256 < x-amz-date <
        // x-amz-security-token`), as the canonical form requires.
        let signed_headers = headers
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>()
            .join(";");
        let mut canonical_headers = String::new();
        for (key, value) in &headers {
            canonical_headers.push_str(key);
            canonical_headers.push(':');
            canonical_headers.push_str(value.trim());
            canonical_headers.push('\n');
        }
        let canonical = format!(
            "GET\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
            base_url.path(),
            Self::EMPTY_BODY_SHA256
        );
        let scope = format!("{date_stamp}/{region}/s3/aws4_request");
        let canonical_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{canonical_hash}");
        let signature = Self::sigv4_signature(creds.secret(), date_stamp, region, &string_to_sign);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.key()
        );
        let mut url = base_url.clone();
        url.set_query(Some(&query));
        (url, authorization, amz_date.to_string())
    }

    /// The `kDate/kRegion/kService/kSigning` HMAC-SHA256 chain.
    fn sigv4_signature(
        secret: &str,
        date_stamp: &str,
        region: &str,
        string_to_sign: &str,
    ) -> String {
        fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
        let region_key = hmac_sha256(&date_key, region.as_bytes());
        let service_key = hmac_sha256(&region_key, b"s3");
        let signing_key = hmac_sha256(&service_key, b"aws4_request");
        hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()))
    }

    /// RFC 3986 percent-encoding for SigV4: unreserved bytes
    /// (`A-Z a-z 0-9 - _ . ~`) pass through, everything else — including `/`
    /// and `%` — becomes uppercase `%XX`. The same rule rusty-s3 signs
    /// presigned URLs with, so a LIST prefix encodes identically both ways.
    fn amz_encode(input: &str) -> String {
        const UNRESERVED: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut out = String::with_capacity(input.len());
        for byte in input.bytes() {
            if UNRESERVED.contains(&byte) {
                out.push(byte as char);
            } else {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
        out
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
            // Header-signed (see `list_request`): R2 rejects the session
            // token as a presigned LIST query parameter.
            let token = continuation.take();
            let mut params = vec![
                ("encoding-type", "url"),
                ("list-type", "2"),
                ("prefix", prefix),
            ];
            if let Some(token) = token.as_deref() {
                params.push(("continuation-token", token));
            }
            let resp = match self
                .list_request(&params)
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

    /// One bounded LIST request with a delimiter: `prefixes` are the folders
    /// below `prefix` and `objects` the files in it. The caller pages with
    /// the returned token; `max_keys` is clamped to 1–1000.
    pub async fn list_delimited_page(
        &self,
        prefix: &str,
        delimiter: &str,
        max_keys: usize,
        continuation: Option<&str>,
    ) -> anyhow::Result<S3Page> {
        with_s3_retry("list", prefix, || {
            self.list_delimited_once(prefix, delimiter, max_keys, continuation)
        })
        .await
    }

    async fn list_delimited_once(
        &self,
        prefix: &str,
        delimiter: &str,
        max_keys: usize,
        continuation: Option<&str>,
    ) -> Result<S3Page, S3Fail> {
        // Header-signed (see `list_request`): R2 rejects the session token
        // as a presigned LIST query parameter.
        let max_keys = max_keys.clamp(1, 1000).to_string();
        let mut params = vec![
            ("delimiter", delimiter),
            ("encoding-type", "url"),
            ("list-type", "2"),
            ("max-keys", max_keys.as_str()),
            ("prefix", prefix),
        ];
        if let Some(token) = continuation {
            params.push(("continuation-token", token));
        }
        let resp = match self
            .list_request(&params)
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
        let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
            .map_err(|e| S3Fail::Final(anyhow::anyhow!("s3 LIST XML parse: {e:#}")))?;
        Ok(S3Page {
            objects: parsed
                .contents
                .into_iter()
                .map(|o| S3Object {
                    key: o.key,
                    last_modified: o.last_modified,
                    size: o.size,
                    etag: o.etag,
                })
                .collect(),
            prefixes: parsed
                .common_prefixes
                .into_iter()
                .map(|p| p.prefix)
                .collect(),
            next: parsed.next_continuation_token,
        })
    }

    /// `HEAD` one object: `None` when it does not exist. Any other failure
    /// is an error, so a refused credential never reads as "missing".
    pub async fn head(&self, key: &str) -> anyhow::Result<Option<S3Head>> {
        let url = self
            .bucket
            .head_object(Some(&self.creds), key)
            .sign(Duration::from_secs(300));
        let resp = self
            .client
            .head(url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .with_context(|| format!("s3 HEAD {key}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(s3_err("head", key, resp, Duration::from_secs(20)).await);
        }
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(Some(S3Head { size, content_type }))
    }

    /// Recursive delete of one prefix, which must be a whole slug scope
    /// (empty or slash-less prefixes are refused, same as [`Store`]).
    /// Empty matches are a no-op, so retries after a partial delete stay
    /// quiet.
    pub async fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
        super::check_delete_prefix(prefix)?;
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
/// A 404 (or a body carrying the store's missing-key `<Code>`) also carries
/// the [`S3NotFound`] marker, so [`is_not_found`] classifies without
/// scanning the message the key rides in.
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

/// Body read with its own explicit bound. Small bodies (error pages, LIST
/// XML) and small-object requests carry their own total deadlines, and the
/// bound stays explicit at each of those call sites instead of relying on
/// client internals — a stalled body after delivered headers must fail like
/// a stalled send, never hang the caller. Large streamed bodies instead rely
/// on the client's idle read timeout (see `STREAM_IDLE_TIMEOUT`), which has
/// no total wall.
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
///
/// Only the object answering 404 — or a body carrying the store's
/// `<Code>NoSuchKey</Code>` — attaches the [`S3NotFound`] marker. A body
/// naming `<Code>NoSuchBucket</Code>` is a wrong-bucket configuration error
/// and stays a hard error even when its status is 404, so boot refuses to
/// start past it. The code match runs on `body`, not on the composed
/// message, so a key that merely contains those words can never count as
/// missing.
fn s3_err_msg(op: &str, target: &str, status: u16, body: &str) -> anyhow::Error {
    let snippet: String = body.trim().chars().take(300).collect();
    let err = anyhow::anyhow!("s3 {op} {target} failed: HTTP {status} {snippet}");
    let missing_bucket = body.contains("<Code>NoSuchBucket</Code>");
    if !missing_bucket && (status == 404 || body.contains("<Code>NoSuchKey</Code>")) {
        err.context(S3NotFound)
    } else {
        err
    }
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

/// Total bound for one streaming-upload attempt: a fixed headroom plus a
/// floor throughput, so a large bundle on a slow link fails only when it
/// stalls below the floor — not at a fixed wall. At 32 KiB/s a 25 MiB
/// snapshot gets ~14 minutes; anything slower is a stuck connection, not a
/// slow one.
fn upload_timeout(len: u64) -> Duration {
    const HEADROOM: Duration = Duration::from_secs(60);
    const FLOOR_BPS: u64 = 32 * 1024;
    HEADROOM + Duration::from_secs(len.div_ceil(FLOOR_BPS))
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
    }

    /// LIST carries the session token as the signed `x-amz-security-token`
    /// header, never as a query parameter: R2 answers 501 when the token
    /// rides the presigned LIST query string. No network: the built request
    /// is inspected before sending.
    #[test]
    fn list_signs_the_token_as_a_header_not_a_query_param() {
        let backend = S3Backend::new(&test_cfg(Some("session-token-abc"))).expect("backend");
        let request = backend
            .list_request(&[
                ("delimiter", "/"),
                ("encoding-type", "url"),
                ("list-type", "2"),
                ("max-keys", "100"),
                ("prefix", "fleets/demo/r2/files/"),
            ])
            .build()
            .expect("request builds");
        let url = request.url().as_str().to_string();
        assert!(
            url.starts_with("https://account.eu.r2.cloudflarestorage.com/outer-state/?"),
            "not path style: {url}"
        );
        for param in [
            "list-type=2",
            "encoding-type=url",
            "prefix=fleets%2Fdemo%2Fr2%2Ffiles%2F",
            "delimiter=%2F",
            "max-keys=100",
        ] {
            assert!(url.contains(param), "param {param} missing: {url}");
        }
        assert!(
            !url.contains("Security-Token"),
            "token must not ride the query: {url}"
        );
        let headers = request.headers();
        assert_eq!(
            headers
                .get("x-amz-security-token")
                .map(|v| v.to_str().expect("ascii")),
            Some("session-token-abc"),
        );
        let auth = headers
            .get("authorization")
            .map(|v| v.to_str().expect("ascii"))
            .expect("authorization header");
        // The key, date, region and service scope ride the credential; the
        // token header is covered by the signature.
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=key/"),
            "credential scope missing: {auth}"
        );
        assert!(
            auth.contains(
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
            ),
            "token header not signed: {auth}"
        );
        assert!(auth.contains("Signature="), "no signature: {auth}");
    }

    /// Without scoped credentials there is no token header and the signed
    /// headers shrink to match — a `None` token must not sign an empty one.
    #[test]
    fn list_without_a_token_signs_no_token_header() {
        let backend = S3Backend::new(&test_cfg(None)).expect("backend");
        let request = backend
            .list_request(&[
                ("encoding-type", "url"),
                ("list-type", "2"),
                ("prefix", "fleets/demo/"),
            ])
            .build()
            .expect("request builds");
        assert!(
            !request.url().as_str().contains("Security-Token"),
            "unexpected token: {}",
            request.url()
        );
        let headers = request.headers();
        assert!(!headers.contains_key("x-amz-security-token"));
        let auth = headers
            .get("authorization")
            .map(|v| v.to_str().expect("ascii"))
            .expect("authorization header");
        assert!(
            auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date,"),
            "signed headers must omit the token: {auth}"
        );
    }

    /// SigV4 query encoding: unreserved bytes pass through, `/` and `%` in
    /// a prefix become `%2F` / `%25` — the same rule the presigned object
    /// URLs use, so LIST asks for the same bytes it stored.
    #[test]
    fn list_query_encoding_matches_presigned_objects() {
        assert_eq!(
            S3Backend::amz_encode("fleets/demo/r2/files/"),
            "fleets%2Fdemo%2Fr2%2Ffiles%2F"
        );
        assert_eq!(S3Backend::amz_encode("caf%C3%A9/"), "caf%25C3%25A9%2F");
        assert_eq!(S3Backend::amz_encode("a-z_A-Z.0~"), "a-z_A-Z.0~");
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
    /// The prefix guard fires before any request is signed or sent: no
    /// network needed.
    #[tokio::test]
    async fn delete_prefix_refuses_unsafe_prefixes() {
        let backend = S3Backend::new(&test_cfg(None)).expect("backend");
        for prefix in ["", "fleets", "fleets/a", "git"] {
            let err = backend.delete_prefix(prefix).await.unwrap_err();
            assert!(
                err.to_string().contains("refusing to delete"),
                "prefix {prefix:?}: {err:#}"
            );
        }
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

    /// Only the object's own 404 / NoSuchKey counts as missing — never the
    /// key text, and never a missing bucket. A 503 on a key named
    /// `NotFound...` is unreachable, and reading a wrong bucket as "start
    /// fresh" would restart the seq over live history and upload an empty
    /// database over the good snapshot.
    #[test]
    fn not_found_ignores_key_names_and_transport_failures() {
        let missing = s3_err_msg("download", "tip.bundle", 404, "<Code>NoSuchKey</Code>");
        assert!(is_not_found(&missing));
        // A missing bucket is a configuration error, never "absent" — even
        // when its status is 404. Restore and claim must refuse to boot past
        // it instead of starting fresh.
        let missing_bucket = s3_err_msg("download", "tip.bundle", 404, "<Code>NoSuchBucket</Code>");
        assert!(!is_not_found(&missing_bucket));
        let missing_bucket_status =
            s3_err_msg("download", "tip.bundle", 403, "<Code>NoSuchBucket</Code>");
        assert!(!is_not_found(&missing_bucket_status));
        // NoSuchKey in the body counts even when the status is not 404.
        let key_code_on_403 = s3_err_msg("download", "tip.bundle", 403, "<Code>NoSuchKey</Code>");
        assert!(is_not_found(&key_code_on_403));
        // A 404 with an empty body (HeadObject-style) still counts: the
        // status alone marks it, as long as the body does not name a bucket.
        let bare_404 = s3_err_msg("download", "tip.bundle", 404, "");
        assert!(is_not_found(&bare_404));
        // A missing key learned another way (a `get` that returned `None`)
        // counts only when built with `not_found_error`; free text never does.
        assert!(is_not_found(&not_found_error("tip.bundle")));
        let text_only = anyhow::anyhow!("s3 download tip.bundle failed: HTTP 404 NoSuchKey");
        assert!(!is_not_found(&text_only));
        // A 5xx on a key containing `NotFound` is unreachable, not missing.
        let slow_on_notfound_key = s3_err_msg(
            "download",
            "backup-NotFound-2026.sqlite.zst",
            503,
            "ServiceUnavailable",
        );
        assert!(!is_not_found(&slow_on_notfound_key));
        // Same for the other code words in key names on non-404 statuses.
        let denied = s3_err_msg("download", "NoSuchKey-manifest.json", 403, "AccessDenied");
        assert!(!is_not_found(&denied));
        let numeric_key_on_500 = s3_err_msg("download", "404.html", 500, "InternalError");
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

    /// The streaming upload bound grows with the file instead of failing a
    /// large bundle at a fixed wall: empty files get only headroom, and a
    /// bigger file always gets more time at the floor throughput.
    #[test]
    fn upload_timeout_scales_with_file_size() {
        assert_eq!(upload_timeout(0), Duration::from_secs(60));
        assert_eq!(upload_timeout(32 * 1024), Duration::from_secs(61));
        let small = upload_timeout(1024 * 1024);
        let large = upload_timeout(100 * 1024 * 1024);
        assert!(small > Duration::from_secs(60));
        assert!(large > small);
        assert_eq!(
            large,
            Duration::from_secs(60 + 100 * 1024 * 1024 / (32 * 1024))
        );
    }

    /// The client bounds streamed bodies by idle time, not by a total wall:
    /// connect and per-read timeouts are configured, so removing the fixed
    /// `.timeout()` on the streamed paths cannot hang the caller on a stall.
    /// (`Client`'s `Debug` prints the read timeout; the builder's prints the
    /// connect timeout — neither prints both.)
    #[test]
    fn client_bounds_streaming_by_idle_time() {
        let backend = S3Backend::new(&test_cfg(None)).expect("backend");
        let debug = format!("{:?}", backend.client);
        assert!(
            debug.contains("read_timeout"),
            "no idle read bound: {debug}"
        );
        let builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STREAM_IDLE_TIMEOUT);
        let debug = format!("{builder:?}");
        assert!(
            debug.contains("connect_timeout"),
            "no connect bound: {debug}"
        );
    }
}
