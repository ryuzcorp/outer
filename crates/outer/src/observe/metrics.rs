//! Per-app request metrics (spec §13, §13.1): the proxy counts every
//! request into per-minute rows (`metric_minute`) and per-hour top paths
//! (`path_hour`); the console reads them back through [`summary`].
//!
//! Noite checked, nothing lifted: Noite aggregates minute sums and raw paths
//! through DuckDB (`apps/runner/src/host/metrics/`, `db/metrics.rs`) with no
//! latency histogram, no percentile and no path collapsing, so the buckets,
//! the percentile and the normalization below are designed fresh for Outer.
//!
//! Design (each choice is pinned by a test in this file):
//!
//! - **Latency point:** time-to-response-headers, measured from just before
//!   the upstream send to the upstream headers arriving (or the send
//!   failing). Fleet wake time is excluded: a cold wake would otherwise
//!   dominate the app's percentiles. WebSocket upgrades record at the 101.
//! - **Bytes out:** the upstream `Content-Length` when present and valid,
//!   else 0. Chunked/streamed bodies are not buffered to count them, so
//!   `bytes_out` undercounts SSE-style traffic; the dominant fixed-body
//!   case is exact.
//! - **Visitors:** per (app, UTC day) distinct client IPs, hashed with the
//!   day's process-random salt (a fresh [`RandomState`] per UTC day, kept
//!   in memory, never stored). No PII reaches SQLite, only the count, and
//!   keys from one day never match the next. Each flush upserts the day's
//!   distinct count into `visitor_day` with
//!   `visitors = max(visitors, excluded.visitors)`, so a restart mid-day
//!   (which resets the in-memory set) keeps the count monotonic — at the
//!   price of undercounting while the new process's set is still smaller
//!   than the pre-restart value. `metric_minute.visitors` keeps the old
//!   per-minute distinct count for per-minute charts; the console totals
//!   come from `visitor_day`.
//! - **Histogram:** 13 edges in ms
//!   (`[1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000]`, plus
//!   overflow) stored as a JSON count array in `latency_hist`. Percentiles
//!   return the first bucket edge holding the rank: histogram resolution,
//!   never interpolated.
//! - **Aggregation:** [`MetricsAgg`] owns every in-memory counter (cloned
//!   into [`ProxyState`](crate::http::proxy::ProxyState), never global).
//!   `record` takes one std mutex and never awaits, so request handling
//!   never blocks on SQLite. [`flush`](MetricsAgg::flush) ships deltas in
//!   one transaction every [`FLUSH_EVERY`] (and once more on shutdown);
//!   a failed flush returns `Err` and keeps everything for the next tick.
//! - **Memory bounds:** minute/path entries live only for the current
//!   minute/hour plus what the last flush missed (dropped once fully
//!   flushed); per-minute visitor sets cap at [`MAX_VISITORS_PER_MINUTE`];
//!   per-day visitor sets cap at [`MAX_VISITORS_PER_APP_DAY`] (further
//!   distinct visitors still count as requests, just not as new uniques,
//!   so the flushed daily count saturates at the cap) and drop for days
//!   before yesterday on every successful flush; distinct raw paths per
//!   app-hour cap at [`MAX_PATHS_PER_APP_HOUR`] with overflow folding into
//!   `(other)` (re-checked against SQLite at flush, so the cap survives
//!   restarts).
//! - **Recorded iff forwarded:** only requests that reach the fleet's port
//!   (an upstream response, or the 502 when the connect fails). Outer's own
//!   synthetic answers — unknown-host 404s, 431s, wake-failure 503s, the
//!   unlisted `robots.txt` override — and the celld health gate path are
//!   never counted.
//! - **Retention:** [`prune`] deletes `metric_minute`, `path_hour` and
//!   `visitor_day` rows older than 7 d for Free apps and 30 d for Pro,
//!   joining `app.owner_id` to `account.plan` (`visitor_day` compares the
//!   UTC-day key against the retention cutoff's day).

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// Flush in-memory counters to SQLite this often.
pub const FLUSH_EVERY: Duration = Duration::from_secs(10);
/// Re-run the retention prune at most this often inside the sweep task.
pub const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Log retention by plan (spec §3, owner-set 2026-10-26): 24 h Free, 30 d
/// Pro. The numbers live in [`crate::limits`].
/// Normalized paths longer than this are truncated (characters).
pub const MAX_PATH_LEN: usize = 200;
/// Distinct normalized paths kept per app-hour; the rest fold into `(other)`.
pub const MAX_PATHS_PER_APP_HOUR: usize = 200;
/// Overflow bucket for path cardinality.
pub const OTHER_PATH: &str = "(other)";
/// Distinct visitor keys kept per (app, minute); further visitors still
/// count as requests, just not as distinct.
pub const MAX_VISITORS_PER_MINUTE: usize = 50_000;
/// Distinct visitor keys kept per (app, UTC day); further distinct visitors
/// still count as requests, just not as new uniques, so the flushed daily
/// count saturates at this cap.
pub const MAX_VISITORS_PER_APP_DAY: usize = 100_000;
/// Absolute cap on in-memory path entries (flood guard; normal traffic keeps
/// one entry per distinct normalized path of the current hour).
pub const MAX_PATH_ENTRIES: usize = 50_000;

/// Latency bucket edges in milliseconds; bucket `i` holds
/// `(edge[i-1], edge[i]]`, bucket 0 holds `[0, edge[0]]`, the last bucket
/// everything above.
pub const HISTOGRAM_EDGES_MS: [u64; 13] =
    [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];
/// Buckets per histogram: edges plus the overflow bucket.
pub const HISTOGRAM_BUCKETS: usize = HISTOGRAM_EDGES_MS.len() + 1;

/// One proxied request's measurements, taken by the proxy at the moment the
/// upstream headers arrive (or the upstream send fails).
pub struct RequestRecord<'a> {
    /// Unix milliseconds; keys the minute and hour buckets.
    pub now_ms: i64,
    pub app_id: &'a str,
    /// Raw request path (query already stripped by the caller is fine;
    /// normalization strips it again defensively).
    pub path: &'a str,
    pub status: u16,
    pub latency_ms: u64,
    pub bytes_out: u64,
    /// The attested-or-peer client IP the proxy already resolved, if any.
    pub client_ip: Option<&'a str>,
}

/// Collapse check: all-digit and UUID segments become `:id`.
fn is_uuid_segment(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Normalize a request path for `path_hour`: drop the query, collapse
/// numeric and UUID segments to `:id`, truncate to [`MAX_PATH_LEN`]
/// characters. Percent-escapes stay encoded (`%2F` never becomes a
/// separator), and an empty path becomes `/`.
pub fn normalize_path(raw: &str) -> String {
    let path = raw.split('?').next().unwrap_or("");
    let path = if path.is_empty() { "/" } else { path };
    let mut out = String::with_capacity(path.len().min(MAX_PATH_LEN + 8));
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if !seg.is_empty() && (seg.bytes().all(|b| b.is_ascii_digit()) || is_uuid_segment(seg)) {
            out.push_str(":id");
        } else {
            out.push_str(seg);
        }
    }
    if out.len() <= MAX_PATH_LEN {
        return out;
    }
    let mut end = MAX_PATH_LEN;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out
}
/// Bucket index for a latency sample: the first edge `>=` the sample, or the
/// overflow bucket.
fn histogram_bucket(latency_ms: u64) -> usize {
    HISTOGRAM_EDGES_MS
        .iter()
        .position(|edge| latency_ms <= *edge)
        .unwrap_or(HISTOGRAM_BUCKETS - 1)
}

/// Merge `delta` counts into `base` element-wise.
fn merge_histogram(base: &mut [u64; HISTOGRAM_BUCKETS], delta: &[u64; HISTOGRAM_BUCKETS]) {
    for (b, d) in base.iter_mut().zip(delta.iter()) {
        *b = b.saturating_add(*d);
    }
}

/// The `q` quantile (0 < q <= 1) of a merged histogram in milliseconds: the
/// first bucket edge holding rank `ceil(q * n)`. Returns 0 for an empty
/// histogram.
pub fn percentile_ms(hist: &[u64; HISTOGRAM_BUCKETS], q: f64) -> u64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0;
    }
    let rank = (q * total as f64).ceil() as u64;
    let rank = rank.max(1);
    let mut acc = 0;
    for (i, count) in hist.iter().enumerate() {
        acc += count;
        if acc >= rank {
            return HISTOGRAM_EDGES_MS.get(i).copied().unwrap_or(u64::MAX);
        }
    }
    u64::MAX
}

fn parse_histogram(raw: Option<String>) -> [u64; HISTOGRAM_BUCKETS] {
    let mut hist = [0; HISTOGRAM_BUCKETS];
    let Some(text) = raw else {
        return hist;
    };
    let parsed: Vec<u64> = serde_json::from_str(&text).unwrap_or_default();
    if parsed.len() == HISTOGRAM_BUCKETS {
        hist.copy_from_slice(&parsed);
    }
    hist
}

fn render_histogram(hist: &[u64; HISTOGRAM_BUCKETS]) -> String {
    serde_json::to_string(&hist.as_slice()).unwrap_or_else(|_| "[]".to_string())
}

fn minute_of(now_ms: i64) -> i64 {
    now_ms / 60_000 * 60_000
}

fn hour_of(now_ms: i64) -> i64 {
    now_ms / 3_600_000 * 3_600_000
}

fn day_of(now_ms: i64) -> i64 {
    now_ms.div_euclid(86_400_000)
}

/// Per-day visitor salt: fresh process-random hash keys every UTC day, kept
/// in memory only. Rotating the [`RandomState`] (rather than hashing a
/// stored salt) means yesterday's keys can never match today's.
struct VisitorSalt {
    day: i64,
    keys: RandomState,
}

impl VisitorSalt {
    fn new(day: i64) -> Self {
        Self {
            day,
            keys: RandomState::new(),
        }
    }

    fn key(&mut self, now_ms: i64, ip: &str) -> u64 {
        let day = day_of(now_ms);
        if day != self.day {
            *self = Self::new(day);
        }
        self.keys.hash_one((self.day, ip))
    }
}

#[derive(Default)]
struct MinuteAgg {
    requests: u64,
    s4xx: u64,
    s5xx: u64,
    bytes_out: u64,
    hist: [u64; HISTOGRAM_BUCKETS],
    visitors: HashSet<u64>,
}

#[derive(Default)]
struct FlushedMinute {
    requests: u64,
    s4xx: u64,
    s5xx: u64,
    bytes_out: u64,
    hist: [u64; HISTOGRAM_BUCKETS],
    visitors: usize,
}

#[derive(Default, Clone, Copy)]
struct PathAgg {
    requests: u64,
    s5xx: u64,
}

/// One path delta plus where it actually persisted (a raw key can fold into
/// `(other)` while keeping its own memory entry for the watermark).
struct PersistedPath {
    mem_key: (String, i64, String),
    db_path: String,
    requests: u64,
    s5xx: u64,
}

struct Inner {
    salt: VisitorSalt,
    minutes: HashMap<(String, i64), MinuteAgg>,
    flushed_minutes: HashMap<(String, i64), FlushedMinute>,
    paths: HashMap<(String, i64, String), PathAgg>,
    flushed_paths: HashMap<(String, i64, String), PathAgg>,
    /// Distinct path keys admitted per (app, hour): the in-memory half of
    /// the cardinality cap (the flush re-checks SQLite for the restart
    /// half). Pruned with the hour.
    path_card: HashMap<(String, i64), usize>,
    /// Distinct visitor keys per (app, UTC day): the in-memory half of
    /// `visitor_day`. Dropped for days before yesterday once flushed.
    visitor_days: HashMap<(String, i64), HashSet<u64>>,
    /// Last flushed set size per (app, day): flush upserts the absolute
    /// count, so only growth since this watermark ships.
    flushed_days: HashMap<(String, i64), usize>,
}

impl Inner {
    fn new(now_ms: i64) -> Self {
        Self {
            salt: VisitorSalt::new(day_of(now_ms)),
            minutes: HashMap::new(),
            flushed_minutes: HashMap::new(),
            paths: HashMap::new(),
            flushed_paths: HashMap::new(),
            path_card: HashMap::new(),
            visitor_days: HashMap::new(),
            flushed_days: HashMap::new(),
        }
    }
}

/// In-memory metrics accumulator, owned by the proxy state and shared with
/// the flush task. Cloning shares the same counters.
#[derive(Clone)]
pub struct MetricsAgg {
    inner: Arc<Mutex<Inner>>,
}

impl MetricsAgg {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(crate::apps::now_ms()))),
        }
    }

    /// Record one forwarded request. Takes one brief mutex; never awaits,
    /// never touches SQLite (the flush task does that).
    pub fn record(&self, r: RequestRecord<'_>) {
        let minute = minute_of(r.now_ms);
        let hour = hour_of(r.now_ms);
        let path = normalize_path(r.path);
        let s5xx = (500..600).contains(&r.status);
        let mut inner = self.inner.lock().unwrap();
        let visitor_key = r.client_ip.map(|ip| inner.salt.key(r.now_ms, ip));
        {
            let entry = inner
                .minutes
                .entry((r.app_id.to_string(), minute))
                .or_default();
            entry.requests += 1;
            if (400..500).contains(&r.status) {
                entry.s4xx += 1;
            } else if s5xx {
                entry.s5xx += 1;
            }
            entry.bytes_out = entry.bytes_out.saturating_add(r.bytes_out);
            entry.hist[histogram_bucket(r.latency_ms)] += 1;
            if let Some(key) = visitor_key {
                if entry.visitors.len() < MAX_VISITORS_PER_MINUTE {
                    entry.visitors.insert(key);
                }
            }
        }
        if let Some(key) = visitor_key {
            let day_entry = inner
                .visitor_days
                .entry((r.app_id.to_string(), day_of(r.now_ms)))
                .or_default();
            if day_entry.len() < MAX_VISITORS_PER_APP_DAY {
                day_entry.insert(key);
            }
        }

        let mut key = (r.app_id.to_string(), hour, path);
        if key.2 != OTHER_PATH && !inner.paths.contains_key(&key) {
            let distinct = inner
                .path_card
                .get(&(key.0.clone(), key.1))
                .copied()
                .unwrap_or(0);
            if distinct >= MAX_PATHS_PER_APP_HOUR || inner.paths.len() >= MAX_PATH_ENTRIES {
                key.2 = OTHER_PATH.to_string();
            } else {
                inner.path_card.insert((key.0.clone(), key.1), distinct + 1);
            }
        }
        let entry = inner.paths.entry(key).or_default();
        entry.requests += 1;
        if s5xx {
            entry.s5xx += 1;
        }
    }

    /// Drop per-day visitor sets (and their watermarks) for days before
    /// yesterday. Only call once the surviving counts are durable: the
    /// max() upsert makes re-creating a dropped day safe, but an unflushed
    /// drop would lose uniques.
    fn evict_old_days(inner: &mut Inner, now_ms: i64) {
        let keep_from = day_of(now_ms) - 1;
        inner.visitor_days.retain(|(_, day), _| *day >= keep_from);
        inner.flushed_days.retain(|(_, day), _| *day >= keep_from);
    }

    /// Ship every unflushed delta to SQLite in one transaction. On success
    /// the flushed watermarks advance and fully-flushed past buckets drop
    /// out of memory; on failure everything is kept for the next tick.
    pub async fn flush(&self, pool: &SqlitePool) -> anyhow::Result<FlushStats> {
        struct MinuteDelta {
            app_id: String,
            minute: i64,
            requests: u64,
            s4xx: u64,
            s5xx: u64,
            bytes_out: u64,
            visitors: u64,
            hist: [u64; HISTOGRAM_BUCKETS],
        }
        struct PathDelta {
            app_id: String,
            hour: i64,
            path: String,
            requests: u64,
            s5xx: u64,
        }
        struct DayDelta {
            app_id: String,
            day: i64,
            visitors: u64,
        }
        let (minute_deltas, path_deltas, day_deltas, now_ms) = {
            let inner = self.inner.lock().unwrap();
            let now_ms = crate::apps::now_ms();
            let mut minutes = Vec::new();
            for (key, agg) in inner.minutes.iter() {
                let flushed = inner.flushed_minutes.get(key);
                let visitors =
                    agg.visitors.len() as u64 - flushed.map(|f| f.visitors as u64).unwrap_or(0);
                let requests = agg.requests - flushed.map(|f| f.requests).unwrap_or(0);
                let s4xx = agg.s4xx - flushed.map(|f| f.s4xx).unwrap_or(0);
                let s5xx = agg.s5xx - flushed.map(|f| f.s5xx).unwrap_or(0);
                let bytes_out = agg.bytes_out - flushed.map(|f| f.bytes_out).unwrap_or(0);
                let mut hist = [0; HISTOGRAM_BUCKETS];
                for (i, count) in agg.hist.iter().enumerate() {
                    hist[i] = count - flushed.map(|f| f.hist[i]).unwrap_or(0);
                }
                if requests == 0 && visitors == 0 && hist.iter().all(|c| *c == 0) {
                    continue;
                }
                minutes.push(MinuteDelta {
                    app_id: key.0.clone(),
                    minute: key.1,
                    requests,
                    s4xx,
                    s5xx,
                    bytes_out,
                    visitors,
                    hist,
                });
            }
            let mut paths = Vec::new();
            for (key, agg) in inner.paths.iter() {
                let flushed = inner.flushed_paths.get(key).copied().unwrap_or_default();
                let requests = agg.requests - flushed.requests;
                if requests == 0 {
                    continue;
                }
                paths.push(PathDelta {
                    app_id: key.0.clone(),
                    hour: key.1,
                    path: key.2.clone(),
                    requests,
                    s5xx: agg.s5xx - flushed.s5xx,
                });
            }
            let mut days = Vec::new();
            for (key, set) in inner.visitor_days.iter() {
                let flushed = inner.flushed_days.get(key).copied().unwrap_or(0);
                if set.len() > flushed {
                    days.push(DayDelta {
                        app_id: key.0.clone(),
                        day: key.1,
                        visitors: set.len() as u64,
                    });
                }
            }
            (minutes, paths, days, now_ms)
        };
        if minute_deltas.is_empty() && path_deltas.is_empty() && day_deltas.is_empty() {
            let mut inner = self.inner.lock().unwrap();
            Self::evict_old_days(&mut inner, now_ms);
            return Ok(FlushStats::default());
        }
        let mut tx = pool.begin().await?;
        for delta in &minute_deltas {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT latency_hist FROM metric_minute WHERE app_id = ?1 AND minute = ?2",
            )
            .bind(&delta.app_id)
            .bind(delta.minute)
            .fetch_optional(&mut *tx)
            .await?;
            let mut merged = parse_histogram(existing);
            merge_histogram(&mut merged, &delta.hist);
            sqlx::query(
                "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (app_id, minute) DO UPDATE SET \
                 requests = metric_minute.requests + excluded.requests, \
                 s4xx = metric_minute.s4xx + excluded.s4xx, \
                 s5xx = metric_minute.s5xx + excluded.s5xx, \
                 latency_hist = excluded.latency_hist, \
                 bytes_out = metric_minute.bytes_out + excluded.bytes_out, \
                 visitors = metric_minute.visitors + excluded.visitors",
            )
            .bind(&delta.app_id)
            .bind(delta.minute)
            .bind(delta.requests as i64)
            .bind(delta.s4xx as i64)
            .bind(delta.s5xx as i64)
            .bind(render_histogram(&merged))
            .bind(delta.bytes_out.min(i64::MAX as u64) as i64)
            .bind(delta.visitors as i64)
            .execute(&mut *tx)
            .await?;
        }
        // Absolute per-day distinct counts: max() keeps the row monotonic
        // across restarts (a fresh process starts from an empty set, so its
        // smaller count never rewinds the row).
        for delta in &day_deltas {
            sqlx::query(
                "INSERT INTO visitor_day (app_id, day, visitors) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (app_id, day) DO UPDATE SET \
                 visitors = max(visitor_day.visitors, excluded.visitors)",
            )
            .bind(&delta.app_id)
            .bind(delta.day)
            .bind(delta.visitors as i64)
            .execute(&mut *tx)
            .await?;
        }
        // The cardinality cap must survive restarts: every dirty app-hour
        // loads the paths SQLite already holds, and a path outside a full
        // set folds into `(other)`. One indexed query per dirty app-hour
        // per flush; the in-memory cap in `record` keeps the common case
        // from ever reaching the fold.
        let mut siblings: HashMap<(String, i64), HashSet<String>> = HashMap::new();
        // Grouped rows for SQLite, plus the per-memory-key provenance the
        // watermarks need (several raw keys can fold into one `(other)`).
        let mut folded: HashMap<(String, i64, String), (u64, u64)> = HashMap::new();
        let mut persisted: Vec<PersistedPath> = Vec::new();
        for delta in path_deltas {
            let mut db_path = delta.path.clone();
            if delta.path != OTHER_PATH {
                let hour_key = (delta.app_id.clone(), delta.hour);
                if !siblings.contains_key(&hour_key) {
                    let rows: Vec<String> = sqlx::query_scalar(
                        "SELECT path FROM path_hour WHERE app_id = ?1 AND hour = ?2",
                    )
                    .bind(&delta.app_id)
                    .bind(delta.hour)
                    .fetch_all(&mut *tx)
                    .await?;
                    siblings.insert(hour_key.clone(), rows.into_iter().collect());
                }
                let set = siblings.get_mut(&hour_key).expect("just inserted");
                if set.contains(&delta.path) || set.len() < MAX_PATHS_PER_APP_HOUR {
                    set.insert(delta.path.clone());
                } else {
                    db_path = OTHER_PATH.to_string();
                }
            }
            let mem_key = (delta.app_id.clone(), delta.hour, delta.path.clone());
            persisted.push(PersistedPath {
                mem_key,
                db_path: db_path.clone(),
                requests: delta.requests,
                s5xx: delta.s5xx,
            });
            let entry = folded
                .entry((delta.app_id, delta.hour, db_path))
                .or_insert((0, 0));
            entry.0 += delta.requests;
            entry.1 += delta.s5xx;
        }
        for ((app_id, hour, path), (requests, s5xx)) in &folded {
            sqlx::query(
                "INSERT INTO path_hour (app_id, hour, path, requests, s5xx) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (app_id, hour, path) DO UPDATE SET \
                 requests = path_hour.requests + excluded.requests, \
                 s5xx = path_hour.s5xx + excluded.s5xx",
            )
            .bind(app_id)
            .bind(*hour)
            .bind(path)
            .bind(*requests as i64)
            .bind(*s5xx as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        let stats = FlushStats {
            minutes: minute_deltas.len(),
            paths: folded.len(),
            visitor_days: day_deltas.len(),
        };
        {
            let mut inner = self.inner.lock().unwrap();
            for delta in minute_deltas {
                let key = (delta.app_id, delta.minute);
                let current = inner.minutes.get(&key).map(|agg| {
                    (
                        agg.requests,
                        agg.s4xx,
                        agg.s5xx,
                        agg.bytes_out,
                        agg.hist,
                        agg.visitors.len(),
                    )
                });
                let flushed = inner.flushed_minutes.entry(key.clone()).or_default();
                flushed.requests += delta.requests;
                flushed.s4xx += delta.s4xx;
                flushed.s5xx += delta.s5xx;
                flushed.bytes_out = flushed.bytes_out.saturating_add(delta.bytes_out);
                merge_histogram(&mut flushed.hist, &delta.hist);
                flushed.visitors += delta.visitors as usize;
                let idle =
                    current.is_some_and(|(requests, s4xx, s5xx, bytes_out, hist, visitors)| {
                        requests == flushed.requests
                            && s4xx == flushed.s4xx
                            && s5xx == flushed.s5xx
                            && bytes_out == flushed.bytes_out
                            && hist == flushed.hist
                            && visitors == flushed.visitors
                    });
                if idle && key.1 < minute_of(now_ms) {
                    inner.minutes.remove(&key);
                    inner.flushed_minutes.remove(&key);
                }
            }
            // Advance path watermarks by what actually persisted. Every
            // memory key's watermark consumes its own delta. A delta folded
            // into `(other)` additionally moves the `(other)` row: both its
            // cumulative and its watermark grow by the same amount, so its
            // own delta is unchanged and the folded counts are never
            // re-persisted. The raw key stays in memory and keeps folding
            // on later records.
            for persisted in persisted {
                let PersistedPath {
                    mem_key,
                    db_path,
                    requests,
                    s5xx,
                } = persisted;
                let flushed = inner.flushed_paths.entry(mem_key.clone()).or_default();
                flushed.requests += requests;
                flushed.s5xx += s5xx;
                if db_path != mem_key.2 {
                    debug_assert_eq!(db_path, OTHER_PATH);
                    let other_key = (mem_key.0.clone(), mem_key.1, OTHER_PATH.to_string());
                    let agg = inner.paths.entry(other_key.clone()).or_default();
                    agg.requests += requests;
                    agg.s5xx += s5xx;
                    let flushed = inner.flushed_paths.entry(other_key).or_default();
                    flushed.requests += requests;
                    flushed.s5xx += s5xx;
                }
            }
            for key in inner.paths.keys().cloned().collect::<Vec<_>>() {
                let agg = inner.paths.get(&key).copied().unwrap_or_default();
                let flushed = inner.flushed_paths.get(&key).copied().unwrap_or_default();
                if agg.requests == flushed.requests
                    && agg.s5xx == flushed.s5xx
                    && key.1 < hour_of(now_ms)
                {
                    inner.paths.remove(&key);
                    inner.flushed_paths.remove(&key);
                }
            }
            inner
                .path_card
                .retain(|(_, hour), _| *hour >= hour_of(now_ms));
            // Day watermarks track absolute counts, so they just jump to
            // what this flush upserted.
            for delta in day_deltas {
                inner
                    .flushed_days
                    .insert((delta.app_id, delta.day), delta.visitors as usize);
            }
            Self::evict_old_days(&mut inner, now_ms);
        }
        Ok(stats)
    }
}

impl Default for MetricsAgg {
    fn default() -> Self {
        Self::new()
    }
}

/// What one successful [`MetricsAgg::flush`] persisted.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushStats {
    /// Minute rows touched.
    pub minutes: usize,
    /// Path rows touched (after `(other)` folding).
    pub paths: usize,
    /// Day rows upserted.
    pub visitor_days: usize,
}

/// Delete per-plan expired rows: older than 7 d for Free apps and 30 d for
/// Pro (spec §3). The plan lives on the account that owns the app. Returns
/// `(metric_minute rows, path_hour rows, visitor_day rows)` deleted.
pub async fn prune(pool: &SqlitePool, now_ms: i64) -> anyhow::Result<(u64, u64, u64)> {
    let free_cutoff = now_ms - crate::limits::FREE_LOG_RETENTION_MS;
    let pro_cutoff = now_ms - crate::limits::PRO_LOG_RETENTION_MS;
    // Literal statements (sqlx 0.9 rejects dynamic SQL): one per
    // table × plan, all sharing the same account-plan join.
    let mut minutes = 0;
    for (cutoff, plan) in [(free_cutoff, "free"), (pro_cutoff, "pro")] {
        minutes += sqlx::query(
            "DELETE FROM metric_minute WHERE minute < ?1 AND app_id IN \
             (SELECT a.id FROM app a JOIN user c ON c.id = a.owner_id WHERE c.plan = ?2)",
        )
        .bind(cutoff)
        .bind(plan)
        .execute(pool)
        .await?
        .rows_affected();
    }
    let mut paths = 0;
    for (cutoff, plan) in [(free_cutoff, "free"), (pro_cutoff, "pro")] {
        paths += sqlx::query(
            "DELETE FROM path_hour WHERE hour < ?1 AND app_id IN \
             (SELECT a.id FROM app a JOIN user c ON c.id = a.owner_id WHERE c.plan = ?2)",
        )
        .bind(cutoff)
        .bind(plan)
        .execute(pool)
        .await?
        .rows_affected();
    }
    let mut days = 0;
    for (cutoff, plan) in [(day_of(free_cutoff), "free"), (day_of(pro_cutoff), "pro")] {
        days += sqlx::query(
            "DELETE FROM visitor_day WHERE day < ?1 AND app_id IN \
             (SELECT a.id FROM app a JOIN user c ON c.id = a.owner_id WHERE c.plan = ?2)",
        )
        .bind(cutoff)
        .bind(plan)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok((minutes, paths, days))
}

/// Flush every [`FLUSH_EVERY`], prune at most every [`PRUNE_EVERY`].
/// Failures log a warn and ride the next tick; the counts are never lost.
/// Returns the task handle: on shutdown the owner aborts it and calls
/// [`MetricsAgg::flush`] once more for the final drain (abort-then-flush is
/// race-free, since deltas make a concurrent flush a no-op).
pub fn spawn_sweep(pool: SqlitePool, agg: MetricsAgg) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(FLUSH_EVERY);
        let mut last_prune: Option<tokio::time::Instant> = None;
        loop {
            interval.tick().await;
            if let Err(err) = agg.flush(&pool).await {
                tracing::warn!(error = %err, "metrics flush failed; keeping counts for the next tick");
            }
            let due = last_prune.is_none_or(|at| at.elapsed() >= PRUNE_EVERY);
            if due {
                match prune(&pool, crate::apps::now_ms()).await {
                    Ok((minutes, paths, days)) => {
                        if minutes + paths + days > 0 {
                            tracing::info!(minutes, paths, days, "pruned expired metrics rows");
                        }
                        last_prune = Some(tokio::time::Instant::now());
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "metrics prune failed; retrying on the next tick");
                    }
                }
                // Tool telemetry (migration 0015): 30-day retention, pruned
                // by this same sweep. Independent of the metrics prune above:
                // one failing never blocks the other.
                match crate::mcp::telemetry::prune(&pool, crate::apps::now_ms()).await {
                    Ok(removed) => {
                        if removed > 0 {
                            tracing::info!(removed, "pruned expired tool telemetry rows");
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "tool telemetry prune failed; retrying on the next tick");
                    }
                }
            }
        }
    })
}

/// One per-hour point of the console metrics view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HourBucket {
    pub hour: i64,
    pub requests: i64,
    pub s4xx: i64,
    pub s5xx: i64,
}

/// One top path of the console metrics view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathStat {
    pub path: String,
    pub requests: i64,
    pub s5xx: i64,
}

/// The console metrics view (spec §13.1): totals, latency percentiles and
/// per-hour buckets over the window, plus top paths. `visitors` sums the
/// daily unique counts of every UTC day the window touches (a 24 h window
/// ending mid-day covers today and yesterday), so a visitor active on N
/// days counts N times. `p50_ms`/`p95_ms` come from the merged histogram
/// at bucket resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub requests: i64,
    pub visitors: i64,
    pub s4xx: i64,
    pub s5xx: i64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub buckets: Vec<HourBucket>,
    pub top_paths: Vec<PathStat>,
}

/// Rows read by the console branch (no HTTP here): totals, `p50`/`p95`,
/// zero-filled per-hour buckets and top 20 paths over `hours` ∈ {24, 168,
/// 720} ending now.
pub async fn summary(pool: &SqlitePool, app_id: &str, hours: i64) -> anyhow::Result<Summary> {
    anyhow::ensure!(
        matches!(hours, 24 | 168 | 720),
        "metrics window must be 24, 168 or 720 hours"
    );
    let now_ms = crate::apps::now_ms();
    let window_ms = hours * 3_600_000;
    let since_minute = (now_ms - window_ms) / 60_000 * 60_000;
    let since_hour = (now_ms - window_ms) / 3_600_000 * 3_600_000;
    let (requests, s4xx, s5xx): (i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(s4xx), 0), COALESCE(SUM(s5xx), 0) \
         FROM metric_minute WHERE app_id = ?1 AND minute >= ?2",
    )
    .bind(app_id)
    .bind(since_minute)
    .fetch_one(pool)
    .await?;
    // Daily uniques, summed over every UTC day the window touches: a day
    // only partly inside the window still contributes its full count.
    let visitors: (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(visitors), 0) FROM visitor_day \
         WHERE app_id = ?1 AND day >= ?2 AND day <= ?3",
    )
    .bind(app_id)
    .bind(day_of(now_ms - window_ms))
    .bind(day_of(now_ms))
    .fetch_one(pool)
    .await?;

    let hists: Vec<(Option<String>,)> =
        sqlx::query_as("SELECT latency_hist FROM metric_minute WHERE app_id = ?1 AND minute >= ?2")
            .bind(app_id)
            .bind(since_minute)
            .fetch_all(pool)
            .await?;
    let mut merged = [0; HISTOGRAM_BUCKETS];
    for (raw,) in hists {
        merge_histogram(&mut merged, &parse_histogram(raw));
    }

    let minute_rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT minute, SUM(requests), SUM(s4xx), SUM(s5xx) FROM metric_minute \
         WHERE app_id = ?1 AND minute >= ?2 GROUP BY minute ORDER BY minute",
    )
    .bind(app_id)
    .bind(since_minute)
    .fetch_all(pool)
    .await?;
    let mut by_hour: HashMap<i64, (i64, i64, i64)> = HashMap::new();
    for (minute, requests, s4xx, s5xx) in minute_rows {
        let entry = by_hour.entry(hour_of(minute)).or_insert((0, 0, 0));
        entry.0 += requests;
        entry.1 += s4xx;
        entry.2 += s5xx;
    }
    let mut buckets = Vec::new();
    let mut hour = since_hour;
    let last_hour = hour_of(now_ms);
    while hour <= last_hour {
        let (requests, s4xx, s5xx) = by_hour.get(&hour).copied().unwrap_or((0, 0, 0));
        buckets.push(HourBucket {
            hour,
            requests,
            s4xx,
            s5xx,
        });
        hour += 3_600_000;
    }

    let path_rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT path, SUM(requests), SUM(s5xx) FROM path_hour \
         WHERE app_id = ?1 AND hour >= ?2 GROUP BY path ORDER BY SUM(requests) DESC, path ASC LIMIT 20",
    )
    .bind(app_id)
    .bind(since_hour)
    .fetch_all(pool)
    .await?;
    let top_paths = path_rows
        .into_iter()
        .map(|(path, requests, s5xx)| PathStat {
            path,
            requests,
            s5xx,
        })
        .collect();

    Ok(Summary {
        requests,
        visitors: visitors.0,
        s4xx,
        s5xx,
        p50_ms: percentile_ms(&merged, 0.5),
        p95_ms: percentile_ms(&merged, 0.95),
        buckets,
        top_paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        (pool, dir)
    }

    async fn app(pool: &SqlitePool, id: &str, plan: &str) {
        sqlx::query("INSERT INTO user (id, email, plan, lastActiveAt, createdAt, updatedAt) VALUES (?1, ?2, ?3, 0, 0, 0)")
            .bind(format!("acct-{id}"))
            .bind(format!("{id}@example.com"))
            .bind(plan)
            .execute(pool)
            .await
            .unwrap();
        // Distinct loopback ports per app: the schema uniques them.
        static TEST_PORT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(18_080);
        let port = TEST_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        sqlx::query(
            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, visibility, last_activity_at, created_at) \
             VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(uid), 200000) + 1 FROM app), ?4, ?4 + 10000, 'public', 0, 1000)",
        )
        .bind(id)
        .bind(id)
        .bind(format!("acct-{id}"))
        .bind(port)
        .execute(pool)
        .await
        .unwrap();
    }

    fn record(
        agg: &MetricsAgg,
        now_ms: i64,
        app: &str,
        path: &str,
        status: u16,
        latency_ms: u64,
        ip: Option<&str>,
    ) {
        agg.record(RequestRecord {
            now_ms,
            app_id: app,
            path,
            status,
            latency_ms,
            bytes_out: 100,
            client_ip: ip,
        });
    }

    #[test]
    fn path_normalization_table() {
        let cases = [
            ("/", "/"),
            ("", "/"),
            ("/users/123", "/users/:id"),
            ("/users/007/profile", "/users/:id/profile"),
            ("/a/550e8400-e29b-41d4-a716-446655440000/b", "/a/:id/b"),
            ("/a/550E8400-E29B-41D4-A716-446655440000", "/a/:id"),
            ("/a/not-a-uuid-xyz", "/a/not-a-uuid-xyz"),
            ("/a/12345-678", "/a/12345-678"),
            ("/search?q=1&page=2", "/search"),
            ("/users/123/", "/users/:id/"),
            ("/v2/items/42/reviews/7", "/v2/items/:id/reviews/:id"),
            ("/UPPER/Path", "/UPPER/Path"),
            ("/files/%2Fetc", "/files/%2Fetc"),
        ];
        for (raw, want) in cases {
            assert_eq!(normalize_path(raw), want, "raw {raw:?}");
        }
        let long = format!("/{}", "x".repeat(300));
        assert_eq!(normalize_path(&long).len(), MAX_PATH_LEN);
    }

    #[test]
    fn visitor_salt_rotates_daily_and_is_process_random() {
        let day: i64 = 20_000;
        let mut a = VisitorSalt::new(day);
        let mut b = VisitorSalt::new(day);
        assert_eq!(
            a.key(day * 86_400_000, "203.0.113.7"),
            a.key(day * 86_400_000, "203.0.113.7")
        );
        assert_ne!(
            a.key(day * 86_400_000, "203.0.113.7"),
            b.key(day * 86_400_000, "203.0.113.7"),
            "two processes must not produce the same visitor key"
        );
        let before = a.key(day * 86_400_000, "203.0.113.7");
        let after = a.key((day + 1) * 86_400_000, "203.0.113.7");
        assert_ne!(before, after, "a new day rotates the salt");
        assert_eq!(a.day, day + 1);
    }

    #[test]
    fn percentiles_come_from_bucket_edges() {
        let mut hist = [0; HISTOGRAM_BUCKETS];
        hist[0] = 50;
        hist[3] = 50;
        assert_eq!(percentile_ms(&hist, 0.5), 1);
        assert_eq!(percentile_ms(&hist, 0.51), 10);
        assert_eq!(percentile_ms(&hist, 0.95), 10);
        assert_eq!(percentile_ms(&[0; HISTOGRAM_BUCKETS], 0.5), 0);
        let mut overflow = [0; HISTOGRAM_BUCKETS];
        overflow[HISTOGRAM_BUCKETS - 1] = 7;
        assert_eq!(percentile_ms(&overflow, 0.99), u64::MAX);
    }

    #[tokio::test]
    async fn flush_persists_counters_histogram_and_distinct_visitors() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        let now_ms = crate::apps::now_ms();
        record(&agg, now_ms, "shop", "/items/42", 200, 3, Some("10.0.0.1"));
        record(&agg, now_ms, "shop", "/items/43", 200, 30, Some("10.0.0.1"));
        record(&agg, now_ms, "shop", "/boom", 500, 1200, Some("10.0.0.2"));
        record(&agg, now_ms, "shop", "/old", 404, 8, None);
        record(
            &agg,
            now_ms,
            "shop",
            "/search?q=x",
            200,
            600,
            Some("10.0.0.3"),
        );
        let stats = agg.flush(&pool).await.unwrap();
        assert_eq!(stats.minutes, 1);
        let row: (i64, i64, i64, String, i64, i64) = sqlx::query_as(
            "SELECT requests, s4xx, s5xx, latency_hist, bytes_out, visitors FROM metric_minute WHERE app_id = 'shop'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((row.0, row.1, row.2), (5, 1, 1));
        assert_eq!(row.4, 500);
        assert_eq!(
            row.5, 3,
            "two hits from one IP count once; the ipless hit is no visitor"
        );
        let day_row: (i64,) =
            sqlx::query_as("SELECT visitors FROM visitor_day WHERE app_id = 'shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(day_row.0, 3, "same-day uniques land in visitor_day too");
        let hist: Vec<u64> = serde_json::from_str(&row.3).unwrap();
        assert_eq!(hist.len(), HISTOGRAM_BUCKETS);
        assert_eq!(hist[histogram_bucket(3)], 1);
        assert_eq!(hist[histogram_bucket(30)], 1);
        assert_eq!(hist[histogram_bucket(1200)], 1);
        assert_eq!(hist[histogram_bucket(8)], 1);
        assert_eq!(hist[histogram_bucket(600)], 1);
        let paths: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT path, requests, s5xx FROM path_hour WHERE app_id = 'shop' ORDER BY path",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let names: Vec<&str> = paths.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(names, ["/boom", "/items/:id", "/old", "/search"]);
        assert_eq!(
            paths.iter().find(|(p, _, _)| p == "/items/:id").unwrap().1,
            2
        );
        assert_eq!(paths.iter().find(|(p, _, _)| p == "/boom").unwrap().2, 1);
    }

    #[tokio::test]
    async fn two_flushes_in_one_minute_merge_without_doubling() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        let now_ms = minute_of(crate::apps::now_ms());
        record(&agg, now_ms, "shop", "/", 200, 4, Some("10.0.0.1"));
        agg.flush(&pool).await.unwrap();
        record(&agg, now_ms + 999, "shop", "/", 503, 40, Some("10.0.0.1"));
        record(&agg, now_ms + 999, "shop", "/", 200, 4, Some("10.0.0.9"));
        agg.flush(&pool).await.unwrap();
        let row: (i64, i64, String, i64) = sqlx::query_as(
            "SELECT requests, s5xx, latency_hist, visitors FROM metric_minute WHERE app_id = 'shop'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((row.0, row.1, row.3), (3, 1, 2));
        let hist: Vec<u64> = serde_json::from_str(&row.2).unwrap();
        assert_eq!(hist.iter().sum::<u64>(), 3);
        // An idle flush touches nothing.
        let idle = agg.flush(&pool).await.unwrap();
        assert_eq!(
            idle,
            FlushStats {
                minutes: 0,
                paths: 0,
                visitor_days: 0,
            }
        );
        let again: (i64,) =
            sqlx::query_as("SELECT requests FROM metric_minute WHERE app_id = 'shop'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(again.0, 3);
    }

    #[tokio::test]
    async fn path_cardinality_cap_folds_overflow_into_other() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        let now_ms = crate::apps::now_ms();
        for i in 0..MAX_PATHS_PER_APP_HOUR + 5 {
            record(
                &agg,
                now_ms,
                "shop",
                &format!("/page-{i}"),
                200,
                2,
                Some("10.0.0.1"),
            );
        }
        agg.flush(&pool).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM path_hour WHERE app_id = 'shop'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, (MAX_PATHS_PER_APP_HOUR + 1) as i64);
        let other: (i64,) = sqlx::query_as(
            "SELECT requests FROM path_hour WHERE app_id = 'shop' AND path = '(other)'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other.0, 5);
    }

    #[tokio::test]
    async fn path_cap_survives_a_restart_via_the_flush_time_check() {
        let (pool, _dir) = pool().await;
        let now_ms = crate::apps::now_ms();
        // A previous process already filled this hour with 200 paths.
        let hour = hour_of(now_ms);
        for i in 0..MAX_PATHS_PER_APP_HOUR {
            sqlx::query("INSERT INTO path_hour (app_id, hour, path, requests, s5xx) VALUES ('shop', ?1, ?2, 1, 0)")
                .bind(hour)
                .bind(format!("/seed-{i}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        // A fresh aggregator (empty memory, as after a restart) sees a new
        // path and must fold it into `(other)`, while a seeded path keeps
        // its own row.
        let fresh = MetricsAgg::new();
        record(&fresh, now_ms, "shop", "/brand-new", 200, 2, None);
        record(&fresh, now_ms, "shop", "/seed-7", 200, 2, None);
        fresh.flush(&pool).await.unwrap();
        let other: (i64,) = sqlx::query_as(
            "SELECT requests FROM path_hour WHERE app_id = 'shop' AND path = '(other)'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other.0, 1);
        let seeded: (i64,) = sqlx::query_as(
            "SELECT requests FROM path_hour WHERE app_id = 'shop' AND path = '/seed-7'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(seeded.0, 2);
    }

    #[tokio::test]
    async fn summary_serves_totals_percentiles_buckets_and_top_paths() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let now_ms = crate::apps::now_ms();
        let minute = minute_of(now_ms);
        // One row inside the 24 h window, one just outside it.
        for (m, requests, hist) in [
            (minute - 60_000, 4, "[4,0,0,0,0,0,0,0,0,0,0,0,0,0]"),
            (
                minute - 25 * 3_600_000,
                100,
                "[0,0,0,0,0,0,0,0,0,0,0,0,0,100]",
            ),
        ] {
            sqlx::query(
                "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
                 VALUES ('shop', ?1, ?2, 1, 0, ?3, 0, 0)",
            )
            .bind(m)
            .bind(requests)
            .bind(hist)
            .execute(&pool)
            .await
            .unwrap();
        }
        // One daily count inside the 24 h window's days, one on a day the
        // window never touches.
        for (day, visitors) in [(day_of(minute), 2), (day_of(now_ms) - 2, 100)] {
            sqlx::query("INSERT INTO visitor_day (app_id, day, visitors) VALUES ('shop', ?1, ?2)")
                .bind(day)
                .bind(visitors)
                .execute(&pool)
                .await
                .unwrap();
        }
        let hour = hour_of(now_ms);
        for (h, path, requests) in [
            (hour, "/items/:id", 9),
            (hour, "/boom", 3),
            (hour - 25 * 3_600_000, "/stale", 50),
        ] {
            sqlx::query("INSERT INTO path_hour (app_id, hour, path, requests, s5xx) VALUES ('shop', ?1, ?2, ?3, 0)")
                .bind(h)
                .bind(path)
                .bind(requests)
                .execute(&pool)
                .await
                .unwrap();
        }
        let got = summary(&pool, "shop", 24).await.unwrap();
        assert_eq!(
            (got.requests, got.s4xx, got.s5xx, got.visitors),
            (4, 1, 0, 2)
        );
        assert_eq!((got.p50_ms, got.p95_ms), (1, 1));
        assert_eq!(
            got.buckets.len(),
            25,
            "24 h plus the current partial hour, zero-filled"
        );
        assert_eq!(
            got.buckets.iter().map(|b| b.requests).sum::<i64>(),
            4,
            "only the in-window row feeds the buckets"
        );
        assert_eq!(
            got.top_paths
                .iter()
                .map(|p| (p.path.as_str(), p.requests))
                .collect::<Vec<_>>(),
            [("/items/:id", 9), ("/boom", 3)]
        );
        assert!(summary(&pool, "shop", 168).await.unwrap().buckets.len() >= 168);
        assert!(summary(&pool, "shop", 720).await.unwrap().buckets.len() >= 720);
        assert!(
            summary(&pool, "shop", 7).await.is_err(),
            "only 24/168/720 are served"
        );
    }

    #[tokio::test]
    async fn prune_keeps_per_plan_retention() {
        let (pool, _dir) = pool().await;
        app(&pool, "free-app", "free").await;
        app(&pool, "pro-app", "pro").await;
        let now_ms = crate::apps::now_ms();
        for (id, age_ms) in [
            ("free-app", 8 * 86_400_000),
            ("free-app", 1_000),
            ("pro-app", 8 * 86_400_000),
            ("pro-app", 31 * 86_400_000),
            ("pro-app", 1_000),
        ] {
            let at = now_ms - age_ms;
            sqlx::query(
                "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
                 VALUES (?1, ?2, 1, 0, 0, '', 0, 0)",
            )
            .bind(id)
            .bind(minute_of(at))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO path_hour (app_id, hour, path, requests, s5xx) VALUES (?1, ?2, '/', 1, 0)")
                .bind(id)
                .bind(hour_of(at))
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO visitor_day (app_id, day, visitors) VALUES (?1, ?2, 1)")
                .bind(id)
                .bind(day_of(at))
                .execute(&pool)
                .await
                .unwrap();
        }
        let (minutes, paths, days) = prune(&pool, now_ms).await.unwrap();
        assert_eq!(
            (minutes, paths, days),
            (2, 2, 2),
            "the 8 d free row and the 31 d pro row, in every table"
        );
        let left: Vec<(String, i64)> =
            sqlx::query_as("SELECT app_id, minute FROM metric_minute ORDER BY app_id, minute")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            left.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(),
            ["free-app", "pro-app", "pro-app"],
            "free keeps only the fresh row, pro keeps 8 d and fresh"
        );
        let days_left: Vec<(String, i64)> =
            sqlx::query_as("SELECT app_id, day FROM visitor_day ORDER BY app_id, day")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            days_left
                .iter()
                .map(|(a, _)| a.as_str())
                .collect::<Vec<_>>(),
            ["free-app", "pro-app", "pro-app"],
            "visitor_day keeps the same rows per plan"
        );
    }

    #[tokio::test]
    async fn visitors_count_once_per_day_across_minutes() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        // Noon yesterday: safely in the past, safely inside one UTC day.
        let noon = (day_of(crate::apps::now_ms()) - 1) * 86_400_000 + 12 * 3_600_000;
        for m in 0..30 {
            record(
                &agg,
                noon + m * 60_000,
                "shop",
                "/",
                200,
                3,
                Some("10.0.0.1"),
            );
        }
        record(
            &agg,
            noon + 5 * 60_000,
            "shop",
            "/",
            200,
            3,
            Some("10.0.0.2"),
        );
        agg.flush(&pool).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT visitors FROM visitor_day WHERE app_id = 'shop'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 2, "thirty minutes of one IP is one daily unique");
        let got = summary(&pool, "shop", 168).await.unwrap();
        assert_eq!(got.visitors, 2);
    }

    #[tokio::test]
    async fn visitors_sum_across_days_in_window() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        let today = day_of(crate::apps::now_ms());
        let noon = |days_ago: i64| (today - days_ago) * 86_400_000 + 12 * 3_600_000;
        // One IP on two days counts on both; a second IP on one day adds one.
        record(&agg, noon(2), "shop", "/", 200, 3, Some("10.0.0.1"));
        record(&agg, noon(1), "shop", "/", 200, 3, Some("10.0.0.1"));
        record(
            &agg,
            noon(1) + 60_000,
            "shop",
            "/",
            200,
            3,
            Some("10.0.0.2"),
        );
        agg.flush(&pool).await.unwrap();
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT day, visitors FROM visitor_day WHERE app_id = 'shop' ORDER BY day",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows, vec![(today - 2, 1), (today - 1, 2)]);
        let got = summary(&pool, "shop", 168).await.unwrap();
        assert_eq!(
            got.visitors, 3,
            "daily uniques, summed over the window's days"
        );
    }

    #[tokio::test]
    async fn visitor_day_restart_keeps_the_max() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let noon = (day_of(crate::apps::now_ms()) - 1) * 86_400_000 + 12 * 3_600_000;
        let agg = MetricsAgg::new();
        record(&agg, noon, "shop", "/", 200, 3, Some("10.0.0.1"));
        record(&agg, noon, "shop", "/", 200, 3, Some("10.0.0.2"));
        agg.flush(&pool).await.unwrap();
        // A restart resets the in-memory set: the smaller post-restart
        // count must not rewind the row.
        drop(agg);
        let fresh = MetricsAgg::new();
        record(&fresh, noon + 60_000, "shop", "/", 200, 3, Some("10.0.0.1"));
        fresh.flush(&pool).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT visitors FROM visitor_day WHERE app_id = 'shop'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 2, "max() keeps the pre-restart count");
        // Growth past the pre-restart value resumes counting.
        record(
            &fresh,
            noon + 120_000,
            "shop",
            "/",
            200,
            3,
            Some("10.0.0.3"),
        );
        record(
            &fresh,
            noon + 180_000,
            "shop",
            "/",
            200,
            3,
            Some("10.0.0.4"),
        );
        fresh.flush(&pool).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT visitors FROM visitor_day WHERE app_id = 'shop'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 3);
    }

    #[tokio::test]
    async fn visitor_day_cap_bounds_memory() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        let noon = (day_of(crate::apps::now_ms()) - 1) * 86_400_000 + 12 * 3_600_000;
        for i in 0..MAX_VISITORS_PER_APP_DAY + 100 {
            let ip = format!("10.{}.{}.{}", (i >> 16) & 255, (i >> 8) & 255, i & 255);
            record(&agg, noon, "shop", "/", 200, 3, Some(&ip));
        }
        agg.flush(&pool).await.unwrap();
        let row: (i64,) = sqlx::query_as("SELECT visitors FROM visitor_day WHERE app_id = 'shop'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.0, MAX_VISITORS_PER_APP_DAY as i64,
            "the flushed daily count saturates at the cap"
        );
        let kept: usize = agg
            .inner
            .lock()
            .unwrap()
            .visitor_days
            .values()
            .map(HashSet::len)
            .sum();
        assert_eq!(kept, MAX_VISITORS_PER_APP_DAY);
    }

    #[tokio::test]
    async fn visitor_day_drops_days_before_yesterday() {
        let (pool, _dir) = pool().await;
        app(&pool, "shop", "free").await;
        let agg = MetricsAgg::new();
        let today = day_of(crate::apps::now_ms());
        let old_noon = (today - 5) * 86_400_000 + 12 * 3_600_000;
        record(&agg, old_noon, "shop", "/", 200, 3, Some("10.0.0.1"));
        agg.flush(&pool).await.unwrap();
        // The row persists (prune owns expiry); memory drops the old day.
        let row: (i64,) = sqlx::query_as("SELECT visitors FROM visitor_day WHERE app_id = 'shop'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 1);
        let inner = agg.inner.lock().unwrap();
        assert!(inner.visitor_days.keys().all(|(_, day)| *day >= today - 1));
        assert!(inner.flushed_days.keys().all(|(_, day)| *day >= today - 1));
    }
}
