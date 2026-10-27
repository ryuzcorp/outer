//! The after-live crawl engine (spec §15.2).
//!
//! The pre-live gate reads what a deploy carries; this module reads what the
//! live app renders: pages from D1 rows, fetched APIs, visitor-written text.
//! After every successful deploy, on every abuse report, and periodically for
//! live public/unlisted apps, the engine fetches `/` plus up to
//! [`CrawlConfig::max_pages`] same-origin links through the internal
//! (loopback) path, extracts the visible text, password/card fields and form
//! targets, classifies the text with the same Jev client and gate thresholds,
//! and acts:
//!
//! | Signal | Result |
//! | --- | --- |
//! | Hard phishing: credentials posted off-origin, a brand name with a
//! password/card field, a link to a known-bad host | **suspend** |
//! | Jev `minors > 0.5`, `phishing > 0.9` or `round(severity) == 3` |
//! **suspend** (451, fleet stopped, email, admin queue) |
//! | Jev `profanity >= 0.5` on a rendered page | **hold** (503, email, queue) |
//! | Jev `round(severity) == 2`, or `hate >= 0.5` | queue only |
//! | Jev unavailable | **fail open**: no state change, logged and retried |
//!
//! Fail-open is the deliberate difference from the pre-live gate (which fails
//! closed): a live app keeps serving through a Jev outage, and the next sweep
//! retries it. See the §15 dated note.
//!
//! The engine never touches the network except loopback to the app's own
//! fleet, and never wakes a sleeping fleet: the sweep only crawls awake apps
//! (spec: public apps are re-crawled daily *while awake*). Unchanged pages
//! skip Jev: when the crawl's page hashes match the previous crawl, the
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::apps::{brand_in_text, App, ModerationState};
use crate::config::CrawlConfig;
use crate::moderation::jev::{ChunkScores, JevClient};

/// How often the periodic sweep looks for due apps.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(300);
/// Cap on one crawl's due list per sweep tick.
const SWEEP_BATCH: i64 = 25;
/// Cap on one fetched page body: rendered pages are small; anything bigger
/// is truncated before extraction.
const MAX_PAGE_BYTES: usize = 1024 * 1024;
/// Cap on the text sent to Jev per crawl.
const MAX_CRAWL_TEXT_BYTES: usize = 256 * 1024;
/// Cap on link targets collected per page (the crawl budget caps the fetch).
const MAX_LINKS_PER_PAGE: usize = 64;

/// One fetched page.
#[derive(Debug, Clone)]
pub struct FetchedPage {
    pub path: String,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

pub trait PageFetch: Send + Sync {
    fn fetch(&self, path: &str) -> impl Future<Output = Result<FetchedPage, String>> + Send;
}

/// The production fetcher: plain HTTP to the fleet's loopback port with the
/// spec's per-page timeout. The fleet is already awake (the crawl only runs
/// for awake apps), so this never wakes anything.
pub struct LoopbackFetch {
    client: reqwest::Client,
    base: String,
    max_bytes: usize,
}

impl LoopbackFetch {
    pub fn new(port: u16, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("building the crawl client: {e:#}"))?;
        Ok(Self {
            client,
            base: format!("http://127.0.0.1:{port}"),
            max_bytes: MAX_PAGE_BYTES,
        })
    }
}

impl PageFetch for LoopbackFetch {
    async fn fetch(&self, path: &str) -> Result<FetchedPage, String> {
        let url = format!("{}{path}", self.base);
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = res.bytes().await.map_err(|e| e.to_string())?;
        let mut body = bytes.to_vec();
        body.truncate(self.max_bytes);
        Ok(FetchedPage {
            path: path.to_string(),
            status,
            content_type,
            body,
        })
    }
}

/// What extraction found on one rendered page.
#[derive(Debug, Clone, Default)]
pub struct PageReport {
    pub path: String,
    /// Visible text lines, in document order.
    pub texts: Vec<String>,
    /// A password or card field rendered anywhere on the page.
    pub has_credential_field: bool,
    /// Form targets posting to another origin.
    pub off_origin_targets: Vec<String>,
    /// A credential field inside a form posting to another origin.
    pub credential_posted_off_origin: Option<String>,
    /// A brand name rendered next to the credential field's page.
    pub brand: Option<String>,
    /// A link or form target on a known-bad host.
    pub bad_host_hit: Option<String>,
    /// Same-origin follow-ups (path + query), in document order, deduped.
    pub links: Vec<String>,
}

/// The crawl's verdict over the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlVerdict {
    Pass,
    Queue,
    Hold,
    Suspend,
    /// Jev was unreachable (fail open) or `/` could not be fetched: no state
    /// change, logged, retried by the next sweep.
    Error,
}

impl CrawlVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            CrawlVerdict::Pass => "pass",
            CrawlVerdict::Queue => "queue",
            CrawlVerdict::Hold => "hold",
            CrawlVerdict::Suspend => "suspend",
            CrawlVerdict::Error => "error",
        }
    }
}

/// One crawl finding, shaped like the gate's `{path, category, signal,
/// score}` rows plus a human-readable excerpt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrawlFinding {
    pub page: String,
    pub category: String,
    pub signal: String,
    pub score: f64,
    pub excerpt: String,
}

/// What one crawl decided.
#[derive(Debug, Clone)]
pub struct CrawlResult {
    pub verdict: CrawlVerdict,
    pub findings: Vec<CrawlFinding>,
    /// Pages actually fetched (including `/`).
    pub pages_crawled: usize,
    /// The crawl matched the previous crawl's pages: Jev was skipped and no
    /// new check row was written, only the watermark advanced.
    pub skipped_unchanged: bool,
}

/// Side effects the caller performs after [`crawl_app`] records the verdict:
/// the crawl owns the database rows, the caller owns the fleet, the edge and
/// the mailbox.
#[derive(Debug, Clone, Default)]
pub struct CrawlEffects {
    pub email_to: Option<String>,
    pub email_subject: Option<String>,
    pub email_body: Option<String>,
    pub stop_fleet: bool,
    pub purge_host: bool,
}

/// Extract one page's signals. Pure: `page_url` is the page's public URL
/// (only its host and path matter), `bad_hosts` the operator's known-bad
/// list. Tags are scanned by hand — no HTML parser on the hot path — with
/// `script`/`style`/`noscript`/`template` contents and comments skipped.
pub fn extract_page(html: &str, page_url: &url::Url, bad_hosts: &[String]) -> PageReport {
    let mut out = PageReport::default();
    let page_host = page_url.host_str().unwrap_or("").to_ascii_lowercase();
    let mut texts: Vec<String> = Vec::new();
    let mut current_text = String::new();
    let mut skip_tag: Option<String> = None;
    let mut in_form_target: Option<url::Url> = None;
    let mut form_has_credential = false;

    let bytes = html.as_bytes();
    let mut i = 0;
    // Flush the running text node, split into non-empty lines.
    let flush = |current: &mut String, texts: &mut Vec<String>| {
        let collapsed = current.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            for line in collapsed.split('\n') {
                let line = line.trim();
                if !line.is_empty() {
                    texts.push(line.to_string());
                }
            }
        }
        current.clear();
    };

    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Comment: skip to `-->`.
            if html[i..].starts_with("<!--") {
                if let Some(end) = html[i..].find("-->") {
                    i += end + 3;
                } else {
                    break;
                }
                continue;
            }
            // Find the tag end, respecting quotes.
            let mut j = i + 1;
            let mut quote = None;
            while j < bytes.len() {
                let c = bytes[j];
                if let Some(q) = quote {
                    if c == q {
                        quote = None;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            // Block boundaries end the running text node; inline tags (`<b>`,
            // `<span>`, `<a>`) only separate with a space, so `Hello
            // <b>world</b>` stays one line.
            let tag = &html[i + 1..j];
            let closing = tag.starts_with('/');
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if is_block(&name) {
                flush(&mut current_text, &mut texts);
            } else {
                current_text.push(' ');
            }
            if skip_tag.is_some() {
                if closing && skip_tag.as_deref() == Some(name.as_str()) {
                    skip_tag = None;
                }
            } else if closing {
                if name == "form" {
                    in_form_target = None;
                    form_has_credential = false;
                }
            } else if matches!(name.as_str(), "script" | "style" | "noscript" | "template") {
                skip_tag = Some(name.clone());
            } else if name == "input" {
                let attrs = parse_attrs(tag);
                let kind = attrs.get("type").map(|s| s.to_ascii_lowercase());
                let name_attr = attrs.get("name").cloned().unwrap_or_default();
                let id_attr = attrs.get("id").cloned().unwrap_or_default();
                let auto = attrs.get("autocomplete").cloned().unwrap_or_default();
                let is_password = kind.as_deref() == Some("password");
                let is_card = is_card_field(kind.as_deref(), &name_attr, &id_attr, &auto);
                if is_password || is_card {
                    out.has_credential_field = true;
                    if let Some(target) = in_form_target.clone() {
                        if is_off_origin(&target, &page_host) {
                            out.credential_posted_off_origin = Some(target.to_string());
                        }
                        form_has_credential = true;
                    }
                }
                let _ = form_has_credential;
            } else if name == "form" {
                let attrs = parse_attrs(tag);
                let target = attrs
                    .get("action")
                    .and_then(|a| page_url.join(a).ok())
                    .unwrap_or_else(|| page_url.clone());
                if is_off_origin(&target, &page_host) {
                    push_unique(&mut out.off_origin_targets, target.to_string());
                }
                if let Some(hit) = host_hit(&target, bad_hosts) {
                    out.bad_host_hit = Some(hit);
                }
                in_form_target = Some(target);
            } else if name == "a" {
                let attrs = parse_attrs(tag);
                if let Some(href) = attrs.get("href") {
                    if let Ok(target) = page_url.join(href) {
                        if matches!(target.scheme(), "http" | "https")
                            && target
                                .host_str()
                                .unwrap_or("")
                                .eq_ignore_ascii_case(&page_host)
                            && !target.path().starts_with("/.outer/")
                            && out.links.len() < MAX_LINKS_PER_PAGE
                        {
                            let mut path = target.path().to_string();
                            if let Some(q) = target.query() {
                                path.push('?');
                                path.push_str(q);
                            }
                            if !(path == "#" || path.is_empty()) && !out.links.contains(&path) {
                                out.links.push(path);
                            }
                        }
                        if let Some(hit) = host_hit(&target, bad_hosts) {
                            out.bad_host_hit = Some(hit);
                        }
                    }
                }
            }
            i = j + 1;
        } else if skip_tag.is_some() {
            i += 1;
        } else {
            // Text node: run to the next `<`, decoding entities on flush.
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            current_text.push_str(&decode_entities(&html[start..i]));
            current_text.push(' ');
        }
    }
    flush(&mut current_text, &mut texts);
    out.texts = texts;
    if out.has_credential_field {
        let joined = out.texts.join("\n");
        if let Some(brand) = brand_in_text(&joined) {
            out.brand = Some(brand.to_string());
        }
    }
    out
}
/// Tags that end the running text node. Everything else is inline and only
/// separates words with a space.
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "br"
            | "hr"
            | "li"
            | "ul"
            | "ol"
            | "tr"
            | "td"
            | "th"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "nav"
            | "aside"
            | "blockquote"
            | "pre"
            | "title"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

/// A card-number, CVC or expiry field: the `autocomplete="cc-*"` family, or
/// a name/id that says card.
fn is_card_field(kind: Option<&str>, name: &str, id: &str, autocomplete: &str) -> bool {
    if autocomplete.to_ascii_lowercase().starts_with("cc-") {
        return true;
    }
    if !matches!(kind, None | Some("text") | Some("tel") | Some("number")) {
        return false;
    }
    let hay = format!("{name} {id}").to_lowercase();
    [
        "card", "cc-num", "ccnum", "cvc", "cvv", "expir", "cc-exp", "ccexp",
    ]
    .iter()
    .any(|needle| hay.contains(needle))
}

fn is_off_origin(target: &url::Url, page_host: &str) -> bool {
    target
        .host_str()
        .is_some_and(|h| !h.eq_ignore_ascii_case(page_host))
}

/// The bad-host entry the target sits on (exact or subdomain), if any.
fn host_hit(target: &url::Url, bad_hosts: &[String]) -> Option<String> {
    let host = target.host_str()?.to_ascii_lowercase();
    bad_hosts.iter().find_map(|entry| {
        let entry = entry.to_ascii_lowercase();
        if host == entry || host.ends_with(format!(".{entry}").as_str()) {
            Some(entry)
        } else {
            None
        }
    })
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !out.contains(&value) {
        out.push(value);
    }
}

/// `name="value"`, `name='value'`, `name=value` and bare `name`, lowercased.
/// The tag name (first token) is skipped.
fn parse_attrs(tag: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    let mut rest = tag
        .split_once(|c: char| c.is_whitespace())
        .map(|(_, tail)| tail)
        .unwrap_or("")
        .trim_start_matches('/');
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with('/') {
            break;
        }
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        if name.is_empty() {
            break;
        }
        if !rest.starts_with('=') {
            attrs.insert(name, String::new());
            continue;
        }
        rest = rest[1..].trim_start();
        let (value, tail) =
            if let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let inner = &rest[quote.len_utf8()..];
                match inner.find(quote) {
                    Some(end) => (inner[..end].to_string(), &inner[end + quote.len_utf8()..]),
                    None => (inner.to_string(), ""),
                }
            } else {
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(rest.len());
                (rest[..end].to_string(), &rest[end..])
            };
        attrs.insert(name, value);
        rest = tail;
    }
    attrs
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Decide over the crawled pages. Pure: heuristic phishing signals first
/// (no Jev needed), then Jev thresholds, mirroring the gate's numbers.
pub fn decide(
    pages: &[PageReport],
    scores: Option<&[ChunkScores]>,
) -> (CrawlVerdict, Vec<CrawlFinding>) {
    for page in pages {
        if let Some(target) = &page.credential_posted_off_origin {
            return (
                CrawlVerdict::Suspend,
                vec![CrawlFinding {
                    page: page.path.clone(),
                    category: "phishing".to_string(),
                    signal: "heuristic".to_string(),
                    score: 1.0,
                    excerpt: format!("login or card form posts credentials to {target}"),
                }],
            );
        }
        if page.brand.is_some() && page.has_credential_field {
            return (
                CrawlVerdict::Suspend,
                vec![CrawlFinding {
                    page: page.path.clone(),
                    category: "phishing".to_string(),
                    signal: "heuristic".to_string(),
                    score: 1.0,
                    excerpt: format!(
                        "brand {:?} on a page asking for credentials",
                        page.brand.clone().unwrap_or_default()
                    ),
                }],
            );
        }
        if let Some(host) = &page.bad_host_hit {
            return (
                CrawlVerdict::Suspend,
                vec![CrawlFinding {
                    page: page.path.clone(),
                    category: "phishing".to_string(),
                    signal: "heuristic".to_string(),
                    score: 1.0,
                    excerpt: format!("link or form target on known-bad host {host}"),
                }],
            );
        }
    }
    let Some(scores) = scores else {
        return (CrawlVerdict::Error, Vec::new());
    };
    let mut findings = Vec::new();
    let mut hold = false;
    let mut queued = false;
    for (page, chunk) in pages.iter().zip(scores.iter()) {
        if chunk.minors > 0.5 || chunk.phishing > 0.9 || chunk.severity.round() == 3.0 {
            let category = if chunk.minors > 0.5 {
                "sexual_minors"
            } else if chunk.phishing > 0.9 {
                "phishing"
            } else {
                "severity"
            };
            return (
                CrawlVerdict::Suspend,
                vec![CrawlFinding {
                    page: page.path.clone(),
                    category: category.to_string(),
                    signal: "jev".to_string(),
                    score: (f64::from(chunk.phishing.max(chunk.minors).max(chunk.severity / 3.0))
                        * 100.0)
                        .round()
                        / 100.0,
                    excerpt: excerpt_of(page),
                }],
            );
        }
        if chunk.profanity >= 0.5 {
            hold = true;
            push_jev_finding(&mut findings, page, "profanity", f64::from(chunk.profanity));
        } else if chunk.severity.round() == 2.0 {
            queued = true;
            push_jev_finding(&mut findings, page, "severity", f64::from(chunk.severity));
        } else if chunk.hate >= 0.5 {
            queued = true;
            push_jev_finding(&mut findings, page, "hate", f64::from(chunk.hate));
        }
    }
    if hold {
        (CrawlVerdict::Hold, findings)
    } else if queued {
        (CrawlVerdict::Queue, findings)
    } else {
        (CrawlVerdict::Pass, Vec::new())
    }
}

fn push_jev_finding(out: &mut Vec<CrawlFinding>, page: &PageReport, category: &str, score: f64) {
    if out.len() >= 25 {
        return;
    }
    out.push(CrawlFinding {
        page: page.path.clone(),
        category: category.to_string(),
        signal: "jev".to_string(),
        score: (score * 100.0).round() / 100.0,
        excerpt: excerpt_of(page),
    });
}

fn excerpt_of(page: &PageReport) -> String {
    let joined = page.texts.join(" ");
    joined.chars().take(100).collect()
}

/// Crawl one app: fetch `/` plus same-origin links, extract, classify, record
/// and apply. `now` is injected (tests run it on a fake clock); the M2
/// "≤ 60 s" bound is met because the deploy hook spawns this immediately and
/// the whole crawl is bounded by `(1 + max_pages) × timeout` plus one Jev
/// call, well under a minute at the default 5 s timeout with buffered
/// follow-ups.
pub async fn crawl_app<F: PageFetch>(
    pool: &SqlitePool,
    app: &App,
    fetch: &F,
    jev: Option<&JevClient>,
    cfg: &CrawlConfig,
    now: i64,
) -> anyhow::Result<(CrawlResult, CrawlEffects)> {
    let app_url = format!("https://{}.outer.now/", app.slug);
    let page_url = url::Url::parse(&app_url)?;
    let root = match fetch.fetch("/").await {
        Ok(page) => page,
        Err(err) => {
            tracing::warn!(slug = %app.slug, error = %err, "after-live crawl: / unreachable, failing open");
            write_check(pool, &app.id, CrawlVerdict::Error, &[], None, now).await?;
            return Ok((
                CrawlResult {
                    verdict: CrawlVerdict::Error,
                    findings: Vec::new(),
                    pages_crawled: 0,
                    skipped_unchanged: false,
                },
                CrawlEffects::default(),
            ));
        }
    };
    let mut pages = vec![root];
    let first = extract_page(
        std::str::from_utf8(&pages[0].body).unwrap_or(""),
        &page_url,
        &cfg.bad_hosts,
    );
    let mut first = first;
    first.path = "/".to_string();
    let mut links: Vec<String> = first.links.clone();
    links.truncate(cfg.max_pages);
    let mut reports = vec![first];
    if !links.is_empty() {
        use futures::stream::{self, StreamExt};
        let fetched: Vec<(String, Result<FetchedPage, String>)> = stream::iter(links)
            .map(|path| async move {
                let got = fetch.fetch(&path).await;
                (path, got)
            })
            .buffered(4)
            .collect()
            .await;
        for (path, got) in fetched {
            let Ok(page) = got else { continue };
            if !page.content_type.is_empty()
                && !page.content_type.contains("text/html")
                && !page.content_type.contains("text/plain")
            {
                continue;
            }
            let Ok(joined) = page_url.join(&path) else {
                continue;
            };
            let mut report = extract_page(
                std::str::from_utf8(&page.body).unwrap_or(""),
                &joined,
                &cfg.bad_hosts,
            );
            report.path = path;
            pages.push(page);
            reports.push(report);
        }
    }
    let pages_crawled = reports.len();

    // Skip unchanged pages: when the body hashes match the previous crawl,
    // only the watermark advances — no Jev call, no new check row.
    let pages_hash = hash_pages(&pages);
    if let Some((last_hash, last_verdict)) = last_crawl_hash(pool, &app.id).await? {
        if last_hash == pages_hash && matches!(last_verdict.as_str(), "pass" | "queue") {
            touch_crawl(pool, &app.id, now).await?;
            return Ok((
                CrawlResult {
                    verdict: if last_verdict == "queue" {
                        CrawlVerdict::Queue
                    } else {
                        CrawlVerdict::Pass
                    },
                    findings: Vec::new(),
                    pages_crawled,
                    skipped_unchanged: true,
                },
                CrawlEffects::default(),
            ));
        }
    }

    let with_text: Vec<usize> = reports
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.texts.join(" ").trim().is_empty())
        .map(|(i, _)| i)
        .collect();
    let scores: Option<Vec<ChunkScores>> = match (&jev, with_text.is_empty()) {
        (_, true) => Some(vec![ChunkScores::default(); reports.len()]),
        (Some(client), false) => {
            // Texts and their report indices, in order, so the scores map
            // back onto the right pages below.
            let mut indexed: Vec<(usize, String)> = Vec::new();
            let mut total = 0;
            for i in with_text {
                let joined = reports[i].texts.join("\n");
                if total >= MAX_CRAWL_TEXT_BYTES {
                    break;
                }
                let mut text = joined;
                text.truncate(MAX_CRAWL_TEXT_BYTES - total);
                total += text.len();
                indexed.push((i, text));
            }
            let texts: Vec<String> = indexed.iter().map(|(_, t)| t.clone()).collect();
            match client.classify(&texts).await {
                Ok(chunk_scores) => {
                    let mut full = vec![ChunkScores::default(); reports.len()];
                    for ((i, _), score) in indexed.iter().zip(chunk_scores.iter()) {
                        full[*i] = *score;
                    }
                    Some(full)
                }
                Err(err) => {
                    tracing::warn!(slug = %app.slug, error = %err, "after-live crawl: Jev unavailable, failing open");
                    None
                }
            }
        }
        (None, false) => None,
    };
    // No Jev client at all (localhost dev without the Cloudflare pair): the
    // heuristic signals above still apply; otherwise fail open like an
    // outage.
    let (verdict, findings) = decide(&reports, scores.as_deref());
    let scores_json = serde_json::json!({
        "pages_hash": pages_hash,
        "pages": reports.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
    })
    .to_string();
    write_check(pool, &app.id, verdict, &findings, Some(&scores_json), now).await?;
    let effects = apply_verdict(pool, app, verdict, &findings, now).await?;
    Ok((
        CrawlResult {
            verdict,
            findings,
            pages_crawled,
            skipped_unchanged: false,
        },
        effects,
    ))
}

fn hash_pages(pages: &[FetchedPage]) -> String {
    let mut hasher = Sha256::new();
    for page in pages {
        hasher.update(page.path.as_bytes());
        hasher.update([0]);
        let mut body = Sha256::new();
        body.update(&page.body);
        hasher.update(body.finalize());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

async fn last_crawl_hash(
    pool: &SqlitePool,
    app_id: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let row: Option<(Option<String>, String)> = sqlx::query_as(
        "SELECT scores, verdict FROM moderation_check \
         WHERE app_id = ?1 AND stage = 'crawl' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    let Some((scores, verdict)) = row else {
        return Ok(None);
    };
    let hash: Option<String> = scores
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("pages_hash")?.as_str().map(str::to_string));
    Ok(hash.map(|h| (h, verdict)))
}

async fn touch_crawl(pool: &SqlitePool, app_id: &str, now: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE app SET last_crawl_at = ?1 WHERE id = ?2")
        .bind(now)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Write the crawl's `moderation_check` row (stage `crawl`). Deploy and sha
/// stay NULL: the after-live stages have no deploy row of their own, and the
/// queue reads these rows on their own id.
async fn write_check(
    pool: &SqlitePool,
    app_id: &str,
    verdict: CrawlVerdict,
    findings: &[CrawlFinding],
    scores_json: Option<&str>,
    now: i64,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let findings_json = serde_json::to_string(findings).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "INSERT INTO moderation_check (id, app_id, deploy_id, sha, stage, findings, scores, verdict, created_at)
         VALUES (?1, ?2, NULL, NULL, 'crawl', ?3, ?4, ?5, ?6)",
    )
    .bind(id)
    .bind(app_id)
    .bind(findings_json)
    .bind(scores_json)
    .bind(verdict.as_str())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record the verdict's database effects and build the caller's side effects.
/// State only ever moves *away* from `Ok` here: a suspended app is never
/// downgraded to held by a later crawl, and emails fire only on the
/// transition (repeated crawls while held stay silent).
async fn apply_verdict(
    pool: &SqlitePool,
    app: &App,
    verdict: CrawlVerdict,
    findings: &[CrawlFinding],
    now: i64,
) -> anyhow::Result<CrawlEffects> {
    touch_crawl_unless_error(pool, &app.id, verdict, now).await?;
    let (state, stop) = match verdict {
        CrawlVerdict::Suspend => (ModerationState::Suspended, true),
        CrawlVerdict::Hold => (ModerationState::Held, false),
        _ => return Ok(CrawlEffects::default()),
    };
    let done = sqlx::query(
        "UPDATE app SET moderation_state = ?1 WHERE id = ?2 AND moderation_state = 'ok'",
    )
    .bind(state.as_str())
    .bind(&app.id)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(CrawlEffects::default());
    }
    let owner_email: Option<String> = sqlx::query_scalar("SELECT email FROM user WHERE id = ?1")
        .bind(&app.owner_id)
        .fetch_optional(pool)
        .await?;
    let Some(email_to) = owner_email else {
        return Ok(CrawlEffects {
            stop_fleet: stop,
            purge_host: true,
            ..CrawlEffects::default()
        });
    };
    let first = findings.first();
    let (email_subject, email_body) = match verdict {
        CrawlVerdict::Suspend => (
            format!("Outer suspended {} for review", app.slug),
            format!(
                "Outer suspended https://{slug}.outer.now because a crawl of its rendered pages flagged {what}.\n\
                 \n\
                 The app is stopped and visitors see a suspended page. Fix the cause (often a login or payment form \
                 posting somewhere unexpected, or page text the content check flags), then deploy again: a clean \
                 crawl after the next successful deploy lifts this automatically, or an admin can restore the app.\n\
                 \n\
                 If you believe this is a mistake, reply to this email.\n",
                slug = app.slug,
                what = first.map(|f| format!("{} ({})", f.page, f.excerpt)).unwrap_or_else(|| "harmful content".to_string()),
            ),
        ),
        _ => (
            format!("Outer held {} for review", app.slug),
            format!(
                "Outer held https://{slug}.outer.now because a crawl of its rendered pages flagged {what}.\n\
                 \n\
                 Visitors see a \"being reviewed\" page until the hold lifts. Fix the flagged text (often \
                 visitor-written content stored in the app's database), then deploy again: a clean crawl after \
                 the next successful deploy lifts this automatically, or an admin can restore the app.\n\
                 \n\
                 If you believe this is a mistake, reply to this email.\n",
                slug = app.slug,
                what = first.map(|f| format!("{} ({})", f.page, f.excerpt)).unwrap_or_else(|| "flagged content".to_string()),
            ),
        ),
    };
    Ok(CrawlEffects {
        email_to: Some(email_to),
        email_subject: Some(email_subject),
        email_body: Some(email_body),
        stop_fleet: stop,
        purge_host: true,
    })
}

async fn touch_crawl_unless_error(
    pool: &SqlitePool,
    app_id: &str,
    verdict: CrawlVerdict,
    now: i64,
) -> anyhow::Result<()> {
    // An error leaves the watermark alone so the next sweep retries soon.
    if verdict == CrawlVerdict::Error {
        return Ok(());
    }
    touch_crawl(pool, app_id, now).await
}

/// Apps due for a crawl at `now`: never crawled since the last deploy
/// (`last_crawl_at IS NULL`), past their visibility's re-crawl cadence while
/// awake, or carrying an unreviewed abuse report newer than the last crawl.
/// Only running, awake, live, unpaused, unexpired, `ok` apps — never a held
/// or suspended app, and never anything asleep (the crawl must not wake).
pub async fn due_apps(pool: &SqlitePool, cfg: &CrawlConfig, now: i64) -> anyhow::Result<Vec<App>> {
    let public_due = now.saturating_sub(cfg.public_interval.as_millis() as i64);
    let unlisted_due = now.saturating_sub(cfg.unlisted_interval.as_millis() as i64);
    let ids: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM app WHERE desired_state = 'running' AND asleep_since IS NULL \
         AND moderation_state = 'ok' AND paused_reason IS NULL AND expired_at IS NULL \
         AND last_deploy_sha IS NOT NULL \
         AND (last_crawl_at IS NULL \
              OR (visibility = 'public' AND last_crawl_at <= ?1) \
              OR (visibility = 'unlisted' AND last_crawl_at <= ?2) \
              OR EXISTS (SELECT 1 FROM moderation_check c WHERE c.app_id = app.id \
                         AND c.stage = 'report' AND c.reviewed_by IS NULL \
                         AND c.created_at > COALESCE(app.last_crawl_at, 0))) \
         ORDER BY last_crawl_at ASC LIMIT ?3",
    )
    .bind(public_due)
    .bind(unlisted_due)
    .bind(SWEEP_BATCH)
    .fetch_all(pool)
    .await?;
    let mut apps = Vec::with_capacity(ids.len());
    for (id,) in ids {
        if let Some(app) = crate::apps::get_by_id(pool, &id).await? {
            apps.push(app);
        }
    }
    Ok(apps)
}

/// One sweep tick: crawl every due app, apply each verdict's effects
/// (fleet stop, edge purge, owner email). Returns the apps crawled.
pub async fn sweep_once(deps: &SweepDeps) -> anyhow::Result<Vec<(String, CrawlVerdict)>> {
    let now = crate::apps::now_ms();
    let mut done = Vec::new();
    for app in due_apps(&deps.pool, &deps.cfg, now).await? {
        // The sweep never wakes: skip anything that fell asleep since the
        // selection read.
        if app.asleep_since.is_some() {
            continue;
        }
        let fetch = match LoopbackFetch::new(app.port, deps.cfg.page_timeout) {
            Ok(fetch) => fetch,
            Err(err) => {
                tracing::warn!(slug = %app.slug, error = %err, "crawl sweep: no loopback client");
                continue;
            }
        };
        match crawl_app(&deps.pool, &app, &fetch, deps.jev.as_ref(), &deps.cfg, now).await {
            Ok((result, effects)) => {
                apply_effects(&deps.effects, &app, &effects).await;
                done.push((app.slug.clone(), result.verdict));
            }
            Err(err) => {
                tracing::warn!(slug = %app.slug, error = %format!("{err:#}"), "crawl sweep failed");
            }
        }
    }
    Ok(done)
}

pub async fn apply_effects(deps: &EffectDeps, app: &App, effects: &CrawlEffects) {
    if effects.stop_fleet {
        deps.fleets.stop_app(&app.slug).await;
    }
    if effects.purge_host {
        let host = format!("{}.{}", app.slug, deps.apps_domain);
        crate::cloudflare::purge_host_cache(deps.purge.as_ref(), &host).await;
    }
    if let (Some(to), Some(subject), Some(body)) = (
        &effects.email_to,
        &effects.email_subject,
        &effects.email_body,
    ) {
        if let Err(err) = deps.sender.send_text(to, subject, body).await {
            tracing::warn!(slug = %app.slug, error = %err.to_string(), "crawl email failed");
        }
    }
}

/// What performing a crawl's side effects needs: the mailbox, the fleet
/// handle and the edge. Shared by the sweep and the proxy's report path.
#[derive(Clone)]
pub struct EffectDeps {
    pub sender: crate::auth::EmailSender,
    pub apps_domain: String,
    pub fleets: crate::fleet::manager::FleetManager,
    pub purge: Option<crate::cloudflare::CachePurge>,
}

/// Everything one sweep tick needs: explicit dependencies, no globals.
#[derive(Clone)]
pub struct SweepDeps {
    pub pool: SqlitePool,
    pub cfg: CrawlConfig,
    pub jev: Option<JevClient>,
    pub effects: EffectDeps,
}

/// The periodic re-crawl loop (spec §15.2: public apps daily while awake).
/// A missed tick only delays a crawl; the next tick catches up.
pub fn spawn_sweep(deps: SweepDeps) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(SWEEP_INTERVAL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(err) = sweep_once(&deps).await {
                tracing::warn!(error = %format!("{err:#}"), "crawl sweep tick failed");
            }
        }
    })
}

/// Validate and normalize an abuse report from `/.outer/report`. Pure, so
/// the proxy and tests share the rules: a fixed reason set, bounded text.
pub fn normalize_report(reason: &str, details: &str) -> Result<(String, String), &'static str> {
    let reason = reason.trim().to_ascii_lowercase();
    if !matches!(
        reason.as_str(),
        "phishing" | "profanity" | "spam" | "abuse" | "other"
    ) {
        return Err("choose a reason: phishing, profanity, spam, abuse or other");
    }
    let details = details.trim().replace('\r', "");
    if details.is_empty() {
        return Err("describe what you saw");
    }
    if details.chars().count() > 4000 {
        return Err("keep the description under 4000 characters");
    }
    Ok((reason, details))
}

/// Record an abuse report as a `moderation_check` row with stage `report`
/// (verdict `flag`): it surfaces in the admin queue immediately, and the
/// next crawl sweep picks the app up for a re-crawl. Returns the check id.
pub async fn record_report(
    pool: &SqlitePool,
    app_id: &str,
    sha: Option<&str>,
    reason: &str,
    details: &str,
    now: i64,
) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let findings = serde_json::json!([{ "reason": reason, "details": details }]).to_string();
    sqlx::query(
        "INSERT INTO moderation_check (id, app_id, deploy_id, sha, stage, findings, scores, verdict, created_at)
         VALUES (?1, ?2, NULL, ?3, 'report', ?4, NULL, 'flag', ?5)",
    )
    .bind(&id)
    .bind(app_id)
    .bind(sha)
    .bind(findings)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Percent-decode one `application/x-www-form-urlencoded` field: `+` becomes
/// a space, `%XX` the byte. Malformed sequences stay literal.
pub fn url_decode(raw: &str) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = raw.get(i + 1..(i + 3).min(raw.len())).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) if hex.len() == 2 => {
                        out.push(byte);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an `application/x-www-form-urlencoded` body into fields.
pub fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    for pair in std::str::from_utf8(body).unwrap_or("").split('&') {
        let (name, value) = match pair.split_once('=') {
            Some((n, v)) => (n, v),
            None => (pair, ""),
        };
        fields.insert(url_decode(name), url_decode(value));
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct MapFetch {
        pages: HashMap<String, FetchedPage>,
    }

    impl MapFetch {
        fn page(path: &str, body: &str) -> FetchedPage {
            FetchedPage {
                path: path.to_string(),
                status: 200,
                content_type: "text/html".to_string(),
                body: body.as_bytes().to_vec(),
            }
        }
    }

    impl PageFetch for MapFetch {
        async fn fetch(&self, path: &str) -> Result<FetchedPage, String> {
            self.pages
                .get(path)
                .cloned()
                .ok_or_else(|| format!("no such page {path}"))
        }
    }

    fn test_cfg() -> CrawlConfig {
        CrawlConfig::default()
    }

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) VALUES ('o', 'owner@example.com', 0, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        (dir, pool)
    }

    static APP_NO: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    async fn insert_app(pool: &SqlitePool, id: &str, slug: &str) {
        let n = APP_NO.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, last_activity_at, created_at, last_deploy_sha) \
             VALUES (?1, ?2, 'o', ?3, ?4, ?5, 0, 0, 'abc')",
        )
        .bind(id)
        .bind(slug)
        .bind(30_000 + n)
        .bind(19_000 + n * 2)
        .bind(19_001 + n * 2)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn load_app(pool: &SqlitePool, id: &str) -> App {
        crate::apps::get_by_id(pool, id).await.unwrap().unwrap()
    }

    fn page_url() -> url::Url {
        url::Url::parse("https://shop.outer.now/").unwrap()
    }

    #[test]
    fn visible_text_skips_scripts_and_tags() {
        let html = "<html><head><style>.x{color:red}</style><script>evil()</script></head>\
            <body><h1>Hello <b>world</b></h1><!-- hidden --><p>Fish &amp; chips</p></body></html>";
        let report = extract_page(html, &page_url(), &[]);
        assert_eq!(report.texts, vec!["Hello world", "Fish & chips"]);
        assert!(!report.has_credential_field);
    }

    #[test]
    fn credential_fields_and_off_origin_targets_are_flagged() {
        let html = r#"<form action="https://evil.example.com/collect">
            <input type="password" name="pass"></form>"#;
        let report = extract_page(html, &page_url(), &[]);
        assert!(report.has_credential_field);
        assert_eq!(
            report.credential_posted_off_origin.as_deref(),
            Some("https://evil.example.com/collect")
        );
        assert_eq!(
            report.off_origin_targets,
            vec!["https://evil.example.com/collect"]
        );
    }

    #[test]
    fn same_origin_links_are_followed_and_outer_pages_skipped() {
        let html = r#"<a href="/items">items</a><a href="/.outer/report">report</a>
            <a href="https://other.example.com/x">off</a><a href="mailto:a@b.c">mail</a>"#;
        let report = extract_page(html, &page_url(), &[]);
        assert_eq!(report.links, vec!["/items"]);
    }

    #[test]
    fn brand_names_match_on_word_boundaries() {
        assert!(brand_in_text("Sign in with PayPal").is_some());
        assert!(brand_in_text("a pineapple recipe").is_none());
    }

    #[test]
    fn bad_hosts_match_exactly_and_on_subdomains() {
        let bad = vec!["evil.example.com".to_string()];
        let html = r#"<a href="https://login.evil.example.com/">x</a>"#;
        let report = extract_page(html, &page_url(), &bad);
        assert_eq!(report.bad_host_hit.as_deref(), Some("evil.example.com"));
        let clean = extract_page(
            r#"<a href="https://good.example.com/">x</a>"#,
            &page_url(),
            &bad,
        );
        assert!(clean.bad_host_hit.is_none());
    }

    #[tokio::test]
    async fn seeded_phishing_page_is_suspended() {
        let (_dir, pool) = test_pool().await;
        insert_app(&pool, "a", "shop").await;
        // A seeded phishing page: a brand name with a password field posting
        // off-origin. No Jev needed — the heuristic signals suspend alone.
        let mut pages = HashMap::new();
        pages.insert(
            "/".to_string(),
            MapFetch::page(
                "/",
                "<h1>PayPal secure login</h1>\
                 <form action=\"https://evil.example.com/collect\">\
                 <input type=\"password\" name=\"pass\"></form>",
            ),
        );
        let app = load_app(&pool, "a").await;
        let (result, effects) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages },
            None,
            &test_cfg(),
            1_000_000,
        )
        .await
        .unwrap();
        assert_eq!(result.verdict, CrawlVerdict::Suspend);
        assert_eq!(result.findings[0].category, "phishing");
        assert_eq!(
            load_app(&pool, "a").await.moderation_state,
            ModerationState::Suspended
        );
        assert!(effects.stop_fleet, "suspend stops the fleet");
        assert!(effects.purge_host, "the edge cache is purged");
        assert_eq!(effects.email_to.as_deref(), Some("owner@example.com"));
        let (verdict,): (String,) =
            sqlx::query_as("SELECT verdict FROM moderation_check WHERE app_id = 'a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(verdict, "suspend");
    }

    #[tokio::test]
    async fn profane_d1_row_rendered_on_a_page_is_held() {
        let (_dir, pool) = test_pool().await;
        insert_app(&pool, "a", "shop").await;
        // A profane D1 row rendered on the page: Jev flags the visible text,
        // and the app is held — never suspended — on profanity alone.
        let server = crate::moderation::jev::fake::FakeJev::start(std::sync::Arc::new(
            |body: &serde_json::Value| {
                let mut answers = serde_json::Map::new();
                if let Some(questions) = body.pointer("/input/questions").and_then(|q| q.as_object()) {
                    for (qid, question) in questions {
                        let kind = question.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let is_profanity = question
                            .get("instructions")
                            .and_then(|q| q.as_str())
                            .unwrap_or("")
                            .contains("profanity");
                        answers.insert(
                            qid.clone(),
                            if kind == "score" {
                                serde_json::json!({"type": "score", "score": 0.0, "confidence": 0.9})
                            } else if is_profanity {
                                serde_json::json!({"type": "noul", "noul": 0.95})
                            } else {
                                serde_json::json!({"type": "noul", "noul": 0.01})
                            },
                        );
                    }
                }
                (
                    200,
                    crate::moderation::jev::fake::envelope(serde_json::json!({
                        "answers": answers, "usage": {},
                    })),
                )
            },
        ))
        .await;
        let mut config = crate::moderation::jev::JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = server.base_url.clone();
        config.timeout = Duration::from_millis(500);
        let jev = JevClient::new(config).unwrap();
        let mut pages = HashMap::new();
        pages.insert(
            "/".to_string(),
            MapFetch::page("/", "<h1>Guestbook</h1><p>this list is fucking awesome</p>"),
        );
        let app = load_app(&pool, "a").await;
        let (result, effects) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages },
            Some(&jev),
            &test_cfg(),
            1_000_000,
        )
        .await
        .unwrap();
        assert_eq!(result.verdict, CrawlVerdict::Hold, "{:?}", result.findings);
        assert!(!effects.stop_fleet, "a hold keeps the fleet");
        assert!(effects.purge_host);
        assert_eq!(
            load_app(&pool, "a").await.moderation_state,
            ModerationState::Held
        );
    }

    #[tokio::test]
    async fn clean_app_is_untouched() {
        let (_dir, pool) = test_pool().await;
        insert_app(&pool, "a", "shop").await;
        let server = crate::moderation::jev::fake::FakeJev::start(std::sync::Arc::new(
            |body: &serde_json::Value| {
                (
                    200,
                    crate::moderation::jev::fake::envelope(
                        crate::moderation::jev::fake::clear_answers(body),
                    ),
                )
            },
        ))
        .await;
        let mut config = crate::moderation::jev::JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = server.base_url.clone();
        config.timeout = Duration::from_millis(500);
        let jev = JevClient::new(config).unwrap();
        let mut pages = HashMap::new();
        pages.insert(
            "/".to_string(),
            MapFetch::page("/", "<h1>Grocery list</h1><a href=\"/items\">items</a>"),
        );
        pages.insert(
            "/items".to_string(),
            MapFetch::page("/items", "<h1>Items</h1><p>milk, eggs</p>"),
        );
        let app = load_app(&pool, "a").await;
        let (result, effects) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages },
            Some(&jev),
            &test_cfg(),
            1_000_000,
        )
        .await
        .unwrap();
        assert_eq!(result.verdict, CrawlVerdict::Pass);
        assert_eq!(result.pages_crawled, 2, "same-origin links are followed");
        assert!(effects.email_to.is_none());
        assert!(!effects.stop_fleet);
        assert_eq!(
            load_app(&pool, "a").await.moderation_state,
            ModerationState::Ok
        );
    }

    #[tokio::test]
    async fn jev_outage_fails_open_and_leaves_the_watermark() {
        let (_dir, pool) = test_pool().await;
        insert_app(&pool, "a", "shop").await;
        let mut pages = HashMap::new();
        pages.insert("/".to_string(), MapFetch::page("/", "<h1>Still here</h1>"));
        let app = load_app(&pool, "a").await;
        // No Jev client at all (localhost dev without the Cloudflare pair):
        // clean pages fail open, the state is untouched, and the watermark
        // does NOT advance, so the next sweep retries.
        let (result, effects) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages },
            None,
            &test_cfg(),
            1_000_000,
        )
        .await
        .unwrap();
        assert_eq!(result.verdict, CrawlVerdict::Error);
        assert!(effects.email_to.is_none());
        assert_eq!(
            load_app(&pool, "a").await.moderation_state,
            ModerationState::Ok
        );
        let (watermark,): (Option<i64>,) =
            sqlx::query_as("SELECT last_crawl_at FROM app WHERE id = 'a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(watermark, None, "an error never advances the watermark");
        let (verdict,): (String,) =
            sqlx::query_as("SELECT verdict FROM moderation_check WHERE app_id = 'a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(verdict, "error");
        // ...but the same outage does not save a phishing page: heuristic
        // signals suspend without Jev.
        let mut phishing = HashMap::new();
        phishing.insert(
            "/".to_string(),
            MapFetch::page(
                "/",
                "<h1>PayPal login</h1><form action=\"https://evil.example.com/x\">\
                 <input type=\"password\" name=\"p\"></form>",
            ),
        );
        let (result, _) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages: phishing },
            None,
            &test_cfg(),
            2_000_000,
        )
        .await
        .unwrap();
        assert_eq!(result.verdict, CrawlVerdict::Suspend);
    }

    #[tokio::test]
    async fn unchanged_pages_skip_jev_and_write_no_row() {
        let (_dir, pool) = test_pool().await;
        insert_app(&pool, "a", "shop").await;
        let server = crate::moderation::jev::fake::FakeJev::start(std::sync::Arc::new(
            |body: &serde_json::Value| {
                (
                    200,
                    crate::moderation::jev::fake::envelope(
                        crate::moderation::jev::fake::clear_answers(body),
                    ),
                )
            },
        ))
        .await;
        let mut config = crate::moderation::jev::JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = server.base_url.clone();
        config.timeout = Duration::from_millis(500);
        let jev = JevClient::new(config).unwrap();
        let pages = || {
            let mut map = HashMap::new();
            map.insert("/".to_string(), MapFetch::page("/", "<h1>Same</h1>"));
            map
        };
        let app = load_app(&pool, "a").await;
        let (first, _) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages: pages() },
            Some(&jev),
            &test_cfg(),
            1_000_000,
        )
        .await
        .unwrap();
        assert!(!first.skipped_unchanged);
        assert_eq!(server.calls(), 1);
        let (second, _) = crawl_app(
            &pool,
            &app,
            &MapFetch { pages: pages() },
            Some(&jev),
            &test_cfg(),
            2_000_000,
        )
        .await
        .unwrap();
        assert!(second.skipped_unchanged, "identical pages skip Jev");
        assert_eq!(server.calls(), 1, "no second Jev call for unchanged pages");
        let (rows,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM moderation_check WHERE app_id = 'a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1, "the skipped crawl writes no new check row");
    }

    #[tokio::test]
    async fn due_selection_covers_fresh_deploys_reports_and_cadence() {
        let (_dir, pool) = test_pool().await;
        insert_app(&pool, "fresh", "fresh").await;
        insert_app(&pool, "recent", "recent").await;
        insert_app(&pool, "stale", "stale").await;
        // `recent` crawled a minute ago, `stale` a week ago (public app).
        // Wall-clock-scale values: the public cadence is a day in ms.
        let now = 20_000_000_000;
        let recent_at = now - 60_000;
        let stale_at = now - 8 * 86_400_000;
        sqlx::query("UPDATE app SET visibility = 'public', last_crawl_at = ?1 WHERE id = 'recent'")
            .bind(recent_at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE app SET visibility = 'public', last_crawl_at = ?1 WHERE id = 'stale'")
            .bind(stale_at)
            .execute(&pool)
            .await
            .unwrap();
        let due = due_apps(&pool, &test_cfg(), now).await.unwrap();
        let slugs: HashSet<String> = due.into_iter().map(|a| a.slug).collect();
        // `fresh` never crawled (the post-deploy case: due immediately, well
        // under the M2 60 s bound); `stale` is past the daily public cadence.
        assert!(slugs.contains("fresh"), "{slugs:?}");
        assert!(slugs.contains("stale"), "{slugs:?}");
        assert!(!slugs.contains("recent"), "{slugs:?}");
        // An unreviewed report makes even a freshly-crawled app due again.
        record_report(&pool, "recent", None, "spam", "bad links", now)
            .await
            .unwrap();
        let due = due_apps(&pool, &test_cfg(), now).await.unwrap();
        let slugs: HashSet<String> = due.into_iter().map(|a| a.slug).collect();
        assert!(
            slugs.contains("recent"),
            "a report re-crawls the app: {slugs:?}"
        );
    }

    #[test]
    fn report_reasons_are_fixed_and_bounded() {
        assert!(normalize_report("phishing", "fake login page").is_ok());
        assert!(normalize_report("Phishing ", "x").is_ok());
        assert!(normalize_report("hate", "x").is_err());
        assert!(normalize_report("spam", "   ").is_err());
        assert!(normalize_report("other", &"x".repeat(4001)).is_err());
    }

    #[test]
    fn form_parsing_decodes_fields() {
        let fields = parse_form(b"reason=phishing&details=fake+login+%3Cpage%3E");
        assert_eq!(fields.get("reason").map(String::as_str), Some("phishing"));
        assert_eq!(
            fields.get("details").map(String::as_str),
            Some("fake login <page>")
        );
        assert_eq!(url_decode("a%2Fb+c"), "a/b c");
    }
}
