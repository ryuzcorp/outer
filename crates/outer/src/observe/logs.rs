//! Fleet log persistence and error grouping (spec §13).
//!
//! celld prints Worker `console.*` output and uncaught exceptions on its
//! stdout/stderr (Outer sets `RUST_LOG=warn,cell_console=info` and pipes
//! both into the per-fleet [`LogRing`](crate::fleet::supervisor::LogRing)),
//! and celld's telemetry table confirms `console.error` carries ERROR
//! severity. The ring alone does not survive sleep or restart, so the sleep
//! sweep flushes every tracked fleet's new lines into `app_log` (batched,
//! once a minute) and prunes to the last 2,000 lines per app.
//!
//! The same flush groups `ERROR` lines and uncaught-exception stacks into
//! `error_issue` rows keyed by fingerprint, and `app_errors` also reports
//! the proxy 5xx count from `metric_minute` (reused, never duplicated).
//!
//! Sequence numbers are global per app: a fresh ring starts past the
//! persisted `MAX(seq)`, so persisted lines and unflushed ring lines merge
//! on one `since` cursor. A ring that overflowed before its first flush
//! leaves a seq gap; the cursor skips gaps without duplicating lines.

use std::collections::HashSet;

use serde_json::{json, Value};

use sqlx::SqlitePool;

use crate::fleet::manager::FleetManager;
use crate::fleet::supervisor::{LogEntry, SharedLogRing};

/// Persisted lines kept per app (spec §13: last 2,000 lines).
pub const MAX_PERSISTED_LINES: i64 = 2000;
/// Grouped-error retention is per plan (spec §3, owner-set 2026-10-26) and
/// lives in [`crate::limits`]; see [`prune_errors`].
/// Most issues one `app_errors` answer carries.
pub const MAX_ISSUES: usize = 50;
/// Most grouped issues kept per app (spec §13 flood bound): once an app
/// reaches this many, existing fingerprints keep updating but new ones
/// route into the single [`OVERFLOW_FINGERPRINT`] issue, so one noisy app
/// can't bloat the shared SQLite and slow `flush_all` for every app.
pub const MAX_ERROR_ISSUES: usize = 500;
/// Fingerprint of the per-app overflow issue (never collides: real
/// fingerprints are sha256 hex).
pub const OVERFLOW_FINGERPRINT: &str = "overflow";
/// Message of the per-app overflow issue.
pub const OVERFLOW_MESSAGE: &str = "More distinct errors than Outer keeps; showing the first 500";
/// Most stack lines kept in one grouped event (flood bound).
const MAX_EVENT_LINES: usize = 40;
/// Traceback header opening a Python exception block.
const TRACEBACK_HEADER: &str = "Traceback (most recent call last):";

/// One grouped error: an `ERROR` line plus the stack frames that followed
/// it, or a multi-line uncaught-exception stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEvent {
    /// The error line itself (or the trailing exception line of a
    /// `Traceback` block, which carries the actual error).
    pub message: String,
    /// The first stack frame, if any.
    pub frame: Option<String>,
    /// Timestamps (unix seconds, as the ring stores them) of the first and
    /// last line of the event.
    pub first_ts: i64,
    pub last_ts: i64,
}

/// The grouped issues of one app, newest first, for `app_errors` and the
/// console errors endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub fingerprint: String,
    pub message: String,
    pub frame: Option<String>,
    pub count: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub status: String,
    /// Ms of the last resolve/ignore; `None` when never triaged. A
    /// resolved issue that recurs flips back to `open` but keeps this,
    /// so readers can badge it regressed (see [`upsert_issue`]).
    pub resolved_at: Option<i64>,
}

/// Split a leading `2026-10-24T09:41:44.123Z`-style timestamp plus the
/// tracing level/target prefix (`ERROR celld::console: `) off `line`.
/// Returns the tracing level (`TRACE`..`ERROR`) when one is present, plus
/// the rest of the line. The rest is unchanged when no such prefix exists.
fn split_log_prefix(line: &str) -> (Option<&'static str>, &str) {
    let mut rest = line.trim_start();
    // Date: exactly `NNNN-NN-NNTNN:NN:NN`, then anything to whitespace.
    let bytes = rest.as_bytes();
    if bytes.len() > 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
    {
        if let Some(end) = rest.find(char::is_whitespace) {
            rest = rest[end..].trim_start();
        }
    }
    // Level plus optional target: `ERROR `, `INFO celld::x: `.
    for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
        if let Some(after) = rest.strip_prefix(level) {
            if after.starts_with(char::is_whitespace) {
                rest = after.trim_start();
                // The target ends at the first `: ` (`celld::console: ...`,
                // not the first `:`): a target never holds whitespace.
                if let Some(idx) = rest.find(": ") {
                    let (target, msg) = rest.split_at(idx);
                    if !target.contains(char::is_whitespace) {
                        rest = msg[2..].trim_start();
                        // celld logs a Worker's `console.*` at tracing INFO
                        // under `cell_console`, with the console method's
                        // level as the message's first word:
                        // `INFO cell_console: ERROR SyntaxError: ...`. That
                        // console level is the one that matters.
                        if target == "cell_console" {
                            for console in ["ERROR", "WARN", "INFO", "LOG", "DEBUG"] {
                                if let Some(msg) = rest.strip_prefix(console) {
                                    if msg.starts_with(char::is_whitespace) {
                                        return (Some(console), msg.trim_start());
                                    }
                                }
                            }
                        }
                    }
                }
                return (Some(level), rest);
            }
        }
    }
    (None, rest)
}

/// Strip a leading `2026-10-24T09:41:44.123Z`-style timestamp plus the
/// tracing level/target prefix (`ERROR celld::console: `), so fingerprints
/// don't move with the clock. Returns the rest of the line unchanged when
/// no such prefix is present.
fn strip_log_prefix(line: &str) -> &str {
    split_log_prefix(line).1
}

fn is_error_start(line: &str) -> bool {
    let (level, text) = split_log_prefix(line);
    // A tracing `ERROR` line is an error even when the stripped body
    // carries no marker. Any other level leaves the decision to the
    // markers below, so an INFO line mentioning ERROR in passing (or
    // visitor text echoed into the logs) never mints an issue.
    if level == Some("ERROR") {
        return true;
    }
    if text.contains("console.error") || text.contains("severity_text\":\"ERROR\"") {
        return true;
    }
    let trimmed = text.trim_start();
    if trimmed.starts_with("Uncaught ") {
        return true;
    }
    if trimmed == TRACEBACK_HEADER {
        return true;
    }
    // `TypeError: ...`, `Error: ...`, `AssertionError: ...`.
    if let Some((head, _)) = trimmed.split_once(':') {
        let head = head.trim_end();
        if !head.contains(char::is_whitespace)
            && (head == "Error"
                || head == "Exception"
                || head.ends_with("Error")
                || head.ends_with("Exception"))
        {
            return true;
        }
        if head.eq_ignore_ascii_case("error") {
            return true;
        }
    }
    false
}

/// True for a line that continues an open error event: a V8 `at ...` frame,
/// a Python `File "...", line N` frame, a `Caused by:` link, or an elided
/// `... N more` tail.
fn is_stack_frame(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("at ") {
        return true;
    }
    if trimmed.starts_with("Caused by:") {
        return true;
    }
    if trimmed.starts_with("...") {
        return true;
    }
    if trimmed.starts_with("File \"") && trimmed.contains(", line ") {
        return true;
    }
    // Anything else (including Python source-context lines) never extends
    // an event on its own: only error and frame lines do.
    false
}

/// One grouped error event plus the line span that produced it: `start`
/// is the index of the opening line, `end` one past the last line of
/// the event. Spans let readers map events back to exact lines (error
/// links in the console, the recent-lines slice of an issue detail).
#[derive(Debug, Clone)]
pub struct EventSpan {
    pub start: usize,
    pub end: usize,
    pub event: ErrorEvent,
}

/// Group an ordered line stream into error events with their line spans.
/// Same grouping as [`scan_errors`]: a non-frame line — or a second
/// error line — closes the open event, except a trailing exception line
/// after frames in a `Traceback` block (Python's `ValueError: ...`),
/// which joins the open event as its message.
pub fn scan_spans(lines: &[(i64, &str)]) -> Vec<EventSpan> {
    let mut spans = Vec::new();
    let mut current: Vec<(usize, i64, String)> = Vec::new();
    // Whether `current` already holds a frame. A second error line then
    // either belongs to the open event (Python's trailing exception line)
    // or starts a new one (back-to-back bare error lines stay separate).
    let mut has_frame = false;
    for (idx, (ts, line)) in lines.iter().enumerate() {
        let ts = *ts;
        if is_error_start(line) {
            let traceback_open = current
                .first()
                .is_some_and(|(_, _, first)| first.trim_start() == TRACEBACK_HEADER);
            if has_frame && traceback_open {
                if current.len() < MAX_EVENT_LINES {
                    current.push((idx, ts, line.to_string()));
                }
                close_span(&mut current, &mut spans);
                has_frame = false;
            } else {
                close_span(&mut current, &mut spans);
                current.push((idx, ts, line.to_string()));
                has_frame = false;
            }
        } else if !current.is_empty() && is_stack_frame(line) {
            if current.len() < MAX_EVENT_LINES {
                current.push((idx, ts, line.to_string()));
            }
            has_frame = true;
        } else {
            // Python source-context lines (`    return 1 // 0`) sit between
            // the `File` frames and the trailing exception: while a
            // `Traceback` block is open, an indented line keeps it open
            // rather than closing it. Anything else closes the event.
            let traceback_context = current
                .first()
                .is_some_and(|(_, _, first)| first.trim_start() == TRACEBACK_HEADER)
                && line.starts_with(char::is_whitespace);
            if !traceback_context {
                close_span(&mut current, &mut spans);
                has_frame = false;
            }
        }
    }
    close_span(&mut current, &mut spans);
    spans
}

fn close_span(current: &mut Vec<(usize, i64, String)>, spans: &mut Vec<EventSpan>) {
    if current.is_empty() {
        return;
    }
    let start = current.first().map(|(idx, _, _)| *idx).unwrap_or(0);
    let end = current.last().map(|(idx, _, _)| *idx + 1).unwrap_or(start);
    let first_ts = current.first().map(|(_, ts, _)| *ts).unwrap_or(0);
    let last_ts = current.last().map(|(_, ts, _)| *ts).unwrap_or(first_ts);
    let message = {
        let first = current
            .first()
            .map(|(_, _, l)| l.clone())
            .unwrap_or_default();
        let last = current
            .last()
            .map(|(_, _, l)| l.clone())
            .unwrap_or_default();
        // A trailing exception line after frames (Python's `ValueError: ...`
        // after `Traceback ...`) carries the actual error; otherwise the
        // opening line is the message.
        if current.len() > 1 && is_error_start(&last) {
            last
        } else {
            first
        }
    };
    // Store the error as the agent should read it: without the timestamp
    // and log level/target prefix (fingerprints strip it anyway).
    let message = strip_log_prefix(&message).to_string();
    let frame = current
        .iter()
        .skip(1)
        .map(|(_, _, l)| l.trim().to_string())
        .find(|l| is_stack_frame(l));
    spans.push(EventSpan {
        start,
        end,
        event: ErrorEvent {
            message,
            frame,
            first_ts,
            last_ts,
        },
    });
    current.clear();
}

/// Group an ordered line stream into error events. A non-frame line — or a
/// second error line — closes the open event. The one exception is a
/// trailing exception line after frames in a `Traceback` block (Python's
/// `ValueError: ...`): it joins the open event as its message, since the
/// header alone carries no information.
pub fn scan_errors<'a>(lines: impl Iterator<Item = (i64, &'a str)>) -> Vec<ErrorEvent> {
    let owned: Vec<(i64, &'a str)> = lines.collect();
    scan_spans(&owned)
        .into_iter()
        .map(|span| span.event)
        .collect()
}

/// Normalize an error message for fingerprinting: strip the log prefix,
/// then replace UUIDs, `0x` hex, quoted spans, long hex runs, and numbers
/// with `#`, and collapse whitespace. Two failures that differ only in
/// ids, hashes, ports, or counts fingerprint alike.
pub fn normalize_message(raw: &str) -> String {
    let text = strip_log_prefix(raw);
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Quoted span: an id, path, or module name in quotes normalizes
        // away (`Cannot find module "groceries"` groups across apps).
        if c == '"' || c == '\'' || c == '`' {
            out.push('#');
            i += 1;
            while i < bytes.len() && bytes[i] as char != c {
                i += 1;
            }
            i += 1; // the closing quote, if any
            continue;
        }
        // UUID `8-4-4-4-12` hex.
        if is_hex_run(bytes, i) >= 8 && is_uuid_at(bytes, i) {
            out.push('#');
            i += 36;
            continue;
        }
        // `0x...` hex.
        if c == '0'
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'x'
            && bytes[i + 2].is_ascii_hexdigit()
        {
            out.push('#');
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            continue;
        }
        // A number, or a long hex/digit run (shas, nonces, ports in
        // `file.ts:123:45` frame tails).
        if c.is_ascii_digit() {
            out.push('#');
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_alphanumeric()
                    || bytes[i] == b'.'
                    || bytes[i] == b'_')
            {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_hexdigit() && is_hex_run(bytes, i) >= 7 {
            out.push('#');
            i += is_hex_run(bytes, i);
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_hex_run(bytes: &[u8], start: usize) -> usize {
    let mut n = 0;
    while start + n < bytes.len() && bytes[start + n].is_ascii_hexdigit() {
        n += 1;
    }
    n
}

fn is_uuid_at(bytes: &[u8], start: usize) -> bool {
    if start + 36 > bytes.len() {
        return false;
    }
    let window = &bytes[start..start + 36];
    if window[8] != b'-' || window[13] != b'-' || window[18] != b'-' || window[23] != b'-' {
        return false;
    }
    if start + 36 < bytes.len() && bytes[start + 36].is_ascii_hexdigit() {
        return false;
    }
    window
        .iter()
        .enumerate()
        .filter(|(i, _)| ![8, 13, 18, 23].contains(i))
        .all(|(_, b)| b.is_ascii_hexdigit())
}

/// The grouping key: sha256 of the normalized message plus the normalized
/// top frame. Same failure with different ids lands in one issue.
pub fn fingerprint(message: &str, frame: Option<&str>) -> String {
    let mut key = normalize_message(message);
    key.push('\n');
    key.push_str(&frame.map(normalize_message).unwrap_or_default());
    crate::auth::tokens::sha256_hex(&key)
}

/// The highest persisted `seq` for one app, or `None` when nothing was
/// ever flushed. A fresh ring starts past it, so seqs stay global per app
/// across sleep and restart.
pub async fn max_seq(pool: &SqlitePool, app_id: &str) -> anyhow::Result<Option<i64>> {
    let max: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM app_log WHERE app_id = ?1")
        .bind(app_id)
        .fetch_one(pool)
        .await?;
    Ok(max)
}

/// One grouped event to record in `error_issue`.
struct NewIssue<'a> {
    fingerprint: &'a str,
    message: &'a str,
    frame: Option<&'a str>,
    count: i64,
    first_seen_ms: i64,
    last_seen_ms: i64,
}

/// Record one grouped event: insert a new `open` issue, or — on a known
/// fingerprint — bump `count` and refresh `last_seen`/`message`/`frame`
/// while keeping `first_seen`. A resolved issue that recurs (the new event
/// lands after `resolved_at`) flips back to `open` automatically but keeps
/// `resolved_at`, so the console badges it regressed. An ignored issue
/// keeps counting and stays ignored: ignoring is the owner's explicit
/// choice to stop hearing about it.
async fn upsert_issue(
    tx: &mut sqlx::SqliteConnection,
    app_id: &str,
    issue: &NewIssue<'_>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO error_issue (app_id, fingerprint, message, frame, count, first_seen, last_seen, status, resolved_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', NULL) \
         ON CONFLICT (app_id, fingerprint) DO UPDATE SET \
           count = error_issue.count + excluded.count, \
           last_seen = excluded.last_seen, \
           message = excluded.message, \
           frame = excluded.frame, \
           status = CASE \
             WHEN error_issue.status = 'resolved' \
               AND excluded.last_seen > COALESCE(error_issue.resolved_at, 0) THEN 'open' \
             ELSE error_issue.status END",
    )
    .bind(app_id)
    .bind(issue.fingerprint)
    .bind(issue.message)
    .bind(issue.frame)
    .bind(issue.count)
    .bind(issue.first_seen_ms)
    .bind(issue.last_seen_ms)
    .execute(tx)
    .await?;
    Ok(())
}

/// Flush one fleet's ring entries for `app_id`: persist every line past the
/// persisted watermark (idempotent: re-flushes insert nothing), group the
/// new lines into `error_issue` rows, and prune to the last
/// [`MAX_PERSISTED_LINES`] lines. Returns the lines persisted.
///
/// Grouped issues are capped at [`MAX_ERROR_ISSUES`] per app: existing
/// fingerprints keep updating, but fingerprints past the cap route into
/// the single [`OVERFLOW_FINGERPRINT`] issue instead of new rows.
pub async fn flush_fleet(
    pool: &SqlitePool,
    app_id: &str,
    entries: &[LogEntry],
) -> anyhow::Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }
    let watermark = max_seq(pool, app_id).await?.unwrap_or(-1);
    let fresh: Vec<&LogEntry> = entries
        .iter()
        .filter(|e| e.seq as i64 > watermark)
        .collect();
    if fresh.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    for entry in &fresh {
        sqlx::query(
            "INSERT OR IGNORE INTO app_log (app_id, seq, ts, stream, line) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(app_id)
        .bind(entry.seq as i64)
        .bind(entry.ts)
        .bind(format!("{:?}", entry.stream).to_ascii_lowercase())
        .bind(&entry.line)
        .execute(&mut *tx)
        .await?;
    }
    let events = scan_errors(fresh.iter().map(|e| (e.ts, e.line.as_str())));
    // Fingerprint once; distinct fingerprints in first-seen order decide
    // the cap: one COUNT per flush in the common case, plus one bounded
    // fingerprint SELECT only when this flush could push past the cap.
    let keyed: Vec<(String, &ErrorEvent)> = events
        .iter()
        .map(|event| (fingerprint(&event.message, event.frame.as_deref()), event))
        .collect();
    let mut distinct: Vec<&str> = Vec::new();
    {
        let mut seen = HashSet::new();
        for (fp, _) in &keyed {
            if seen.insert(fp.as_str()) {
                distinct.push(fp.as_str());
            }
        }
    }
    let issue_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM error_issue WHERE app_id = ?1 AND fingerprint != ?2",
    )
    .bind(app_id)
    .bind(OVERFLOW_FINGERPRINT)
    .fetch_one(&mut *tx)
    .await?;
    if issue_count as usize + distinct.len() <= MAX_ERROR_ISSUES {
        for (fp, event) in &keyed {
            upsert_issue(
                &mut tx,
                app_id,
                &NewIssue {
                    fingerprint: fp,
                    message: &event.message,
                    frame: event.frame.as_deref(),
                    count: 1,
                    first_seen_ms: event.first_ts * 1000,
                    last_seen_ms: event.last_ts * 1000,
                },
            )
            .await?;
        }
    } else {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT fingerprint FROM error_issue WHERE app_id = ?1")
                .bind(app_id)
                .fetch_all(&mut *tx)
                .await?;
        let mut known: HashSet<String> = rows.into_iter().map(|(fp,)| fp).collect();
        // The overflow row itself must never consume the new-issue budget.
        known.remove(OVERFLOW_FINGERPRINT);
        let mut budget = (MAX_ERROR_ISSUES as i64 - issue_count).max(0) as usize;
        let mut overflow_count: i64 = 0;
        let mut overflow_first: i64 = 0;
        let mut overflow_last: i64 = 0;
        for (fp, event) in &keyed {
            if !known.contains(fp.as_str()) {
                if budget == 0 {
                    let first = event.first_ts * 1000;
                    let last = event.last_ts * 1000;
                    if overflow_count == 0 {
                        overflow_first = first;
                        overflow_last = last;
                    } else {
                        overflow_first = overflow_first.min(first);
                        overflow_last = overflow_last.max(last);
                    }
                    overflow_count += 1;
                    continue;
                }
                budget -= 1;
                known.insert((*fp).to_string());
            }
            upsert_issue(
                &mut tx,
                app_id,
                &NewIssue {
                    fingerprint: fp,
                    message: &event.message,
                    frame: event.frame.as_deref(),
                    count: 1,
                    first_seen_ms: event.first_ts * 1000,
                    last_seen_ms: event.last_ts * 1000,
                },
            )
            .await?;
        }
        if overflow_count > 0 {
            upsert_issue(
                &mut tx,
                app_id,
                &NewIssue {
                    fingerprint: OVERFLOW_FINGERPRINT,
                    message: OVERFLOW_MESSAGE,
                    frame: None,
                    count: overflow_count,
                    first_seen_ms: overflow_first,
                    last_seen_ms: overflow_last,
                },
            )
            .await?;
        }
    }
    sqlx::query(
        "DELETE FROM app_log WHERE app_id = ?1 AND seq NOT IN \
         (SELECT seq FROM app_log WHERE app_id = ?1 ORDER BY seq DESC LIMIT ?2)",
    )
    .bind(app_id)
    .bind(MAX_PERSISTED_LINES)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(fresh.len())
}

/// A ring snapshot: every entry, oldest first.
pub async fn ring_snapshot(ring: &SharedLogRing) -> Vec<LogEntry> {
    let ring = ring.lock().await;
    ring.tail(ring.len())
}

/// Flush every tracked fleet: the sleep sweep calls this each tick, and
/// shutdown calls it once before stopping the fleets. Fleets that vanished
/// mid-tick are skipped; one app's failure warns and the rest still flush.
pub async fn flush_all(pool: &SqlitePool, fleets: &FleetManager) -> anyhow::Result<()> {
    let apps: Vec<(String, String)> = sqlx::query_as("SELECT id, slug FROM app ORDER BY slug")
        .fetch_all(pool)
        .await?;
    for (app_id, slug) in &apps {
        let Some(ring) = fleets.logs(slug) else {
            continue;
        };
        let entries = ring_snapshot(&ring).await;
        if let Err(err) = flush_fleet(pool, app_id, &entries).await {
            tracing::warn!(slug = %slug, error = %format!("{err:#}"), "log flush failed; lines stay in the ring");
        }
    }
    Ok(())
}

/// One page of a `since` cursor: the OLDEST `limit` entries, so the next
/// poll from the last returned seq continues without a gap.
fn cursor_page<T>(mut entries: Vec<T>, limit: usize) -> Vec<T> {
    entries.truncate(limit);
    entries
}

/// One log line with its sequence cursor, plus the error-issue
/// fingerprint when the line opens (or continues) a grouped error event:
/// the console links those lines to their issue.
///
/// `ts` is unix seconds in storage (the ring's unit). [`LogLine::to_json`]
/// is the console API boundary and emits milliseconds, matching every
/// other console timestamp (`when`/`timeAgo` take ms); the MCP
/// `app_logs` shape (`read_merged`) still carries seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub seq: u64,
    pub ts: i64,
    pub stream: String,
    pub line: String,
    pub fingerprint: Option<String>,
}

impl LogLine {
    pub fn to_json(&self) -> Value {
        let mut out = json!({"seq": self.seq, "ts": self.ts.saturating_mul(1000), "stream": self.stream, "line": self.line});
        if let Some(fp) = &self.fingerprint {
            out["fingerprint"] = json!(fp);
        }
        out
    }
}

/// Server-side log filter (console Logs view, SSE follow). Everything is
/// optional; every bound is checked at the HTTP edge, so this struct
/// only carries validated values.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Substring match on the line, case-insensitive.
    pub q: Option<String>,
    /// `stdout` or `stderr`.
    pub stream: Option<String>,
    /// `error`, `warn` or `info` (lines without a level prefix count as
    /// info: plain `console.log` output carries no prefix).
    pub level: Option<String>,
    /// Millisecond time range, inclusive.
    pub from: Option<i64>,
    pub to: Option<i64>,
}

/// What [`read_filtered`] pages on: forward from `since` (follow/poll),
/// backward from `before` (the `Load older` button), or the newest tail.
#[derive(Debug, Clone, Copy)]
pub enum LogCursor {
    Since(u64),
    Before(u64),
    Tail,
}

/// The tracing level of one stored line for [`LogFilter::level`]:
/// the parsed prefix level, or `info` for plain output lines.
fn line_level(line: &str) -> &'static str {
    match split_log_prefix(line).0 {
        Some("ERROR") => "error",
        Some("WARN") => "warn",
        _ => "info",
    }
}

fn filter_match(filter: &LogFilter, ts: i64, stream: &str, line: &str) -> bool {
    if let Some(q) = &filter.q {
        if !line.to_lowercase().contains(q) {
            return false;
        }
    }
    if let Some(want) = &filter.stream {
        if stream != want {
            return false;
        }
    }
    if let Some(want) = &filter.level {
        if line_level(line) != want {
            return false;
        }
    }
    // Stored `ts` is unix seconds (the ring's unit); the API takes unix
    // milliseconds, so compare in ms. Second-granularity lines at the
    // edges stay inclusive.
    let ts_ms = ts.saturating_mul(1000);
    if let Some(from) = filter.from {
        if ts_ms < from {
            return false;
        }
    }
    if let Some(to) = filter.to {
        if ts_ms > to {
            return false;
        }
    }
    true
}

/// Persisted lines merged with the live ring on the same `seq` cursor,
/// filtered server-side and paged: `Since` keeps the OLDEST `limit` lines
/// past the seq (follow/poll), `Before` keeps the newest `limit` older
/// than the seq (`Load older`), `Tail` keeps the newest `limit` overall.
/// Lines that open a grouped error carry its fingerprint (computed over
/// the returned page, so `scan_errors` only ever sees `limit` lines).
pub async fn read_filtered(
    pool: &SqlitePool,
    fleets: &FleetManager,
    app_id: &str,
    slug: &str,
    filter: &LogFilter,
    cursor: LogCursor,
    limit: usize,
) -> Vec<LogLine> {
    let normalized = LogFilter {
        q: filter.q.as_ref().map(|q| q.to_lowercase()),
        stream: filter.stream.clone(),
        level: filter.level.clone(),
        from: filter.from,
        to: filter.to,
    };
    let persisted: Vec<(i64, i64, String, String)> =
        sqlx::query_as("SELECT seq, ts, stream, line FROM app_log WHERE app_id = ?1 ORDER BY seq")
            .bind(app_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let mut by_seq: std::collections::BTreeMap<u64, (i64, String, String)> = persisted
        .into_iter()
        .map(|(seq, ts, stream, line)| (seq as u64, (ts, stream, line)))
        .collect();
    if let Some(ring) = fleets.logs(slug) {
        for entry in ring_snapshot(&ring).await {
            by_seq.insert(
                entry.seq,
                (
                    entry.ts,
                    format!("{:?}", entry.stream).to_ascii_lowercase(),
                    entry.line,
                ),
            );
        }
    }
    let mut entries: Vec<(u64, (i64, String, String))> = by_seq.into_iter().collect();
    match cursor {
        LogCursor::Since(seq) => {
            entries.retain(|(s, _)| *s > seq);
            entries.truncate(limit);
        }
        LogCursor::Before(seq) => {
            entries.retain(|(s, _)| *s < seq);
            let skip = entries.len().saturating_sub(limit);
            entries = entries.into_iter().skip(skip).collect();
        }
        LogCursor::Tail => {
            let skip = entries.len().saturating_sub(limit);
            entries = entries.into_iter().skip(skip).collect();
        }
    }
    // Fingerprint the page's error events over exact line spans, so the
    // fingerprints match what the flush grouped (same `scan_spans`
    // core): every line of an event carries its issue fingerprint.
    let refs: Vec<(i64, &str)> = entries
        .iter()
        .map(|(_, (ts, _, line))| (*ts, line.as_str()))
        .collect();
    let mut fps: Vec<Option<String>> = vec![None; entries.len()];
    for span in scan_spans(&refs) {
        let fp = fingerprint(&span.event.message, span.event.frame.as_deref());
        let end = span.end.min(fps.len());
        for slot in &mut fps[span.start..end] {
            *slot = Some(fp.clone());
        }
    }
    entries
        .into_iter()
        .zip(fps)
        .filter(|((_, (ts, stream, line)), _)| filter_match(&normalized, *ts, stream, line))
        .map(|((seq, (ts, stream, line)), fingerprint)| LogLine {
            seq,
            ts,
            stream,
            line,
            fingerprint,
        })
        .collect()
}

/// One sparkline bucket: occurrences of one issue in `[start, start + 3600_000)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparkBucket {
    pub start: i64,
    pub count: i64,
}

/// Issue detail for the console: the issue, an hourly occurrence
/// sparkline over the trailing 24 hours (recomputed from the retained
/// lines, so buckets only cover what `app_log` still holds), and the
/// most recent matching log lines.
#[derive(Debug, Clone)]
pub struct IssueDetail {
    pub issue: Issue,
    pub sparkline: Vec<SparkBucket>,
    pub recent: Vec<LogLine>,
}

/// Buckets for one issue's sparkline: 24 hourly buckets ending at `now_ms`.
const SPARK_BUCKETS: usize = 24;
/// Most recent matching lines one detail answer carries.
const DETAIL_RECENT_LINES: usize = 30;

/// The issue plus its sparkline and recent lines, or `None` for an
/// unknown fingerprint.
///
/// One `error_issue` row: message, frame, count, first/last seen, status
/// and the triage stamp.
type IssueDetailRow = (String, Option<String>, i64, i64, i64, String, Option<i64>);

pub async fn issue_detail(
    pool: &SqlitePool,
    app_id: &str,
    wanted: &str,
    now_ms: i64,
) -> anyhow::Result<Option<IssueDetail>> {
    let row: Option<IssueDetailRow> = sqlx::query_as(
        "SELECT message, frame, count, first_seen, last_seen, status, resolved_at \
         FROM error_issue WHERE app_id = ?1 AND fingerprint = ?2",
    )
    .bind(app_id)
    .bind(wanted)
    .fetch_optional(pool)
    .await?;
    let Some((message, frame, count, first_seen, last_seen, status, resolved_at)) = row else {
        return Ok(None);
    };
    let issue = Issue {
        fingerprint: wanted.to_string(),
        message,
        frame,
        count,
        first_seen,
        last_seen,
        status,
        resolved_at,
    };
    // Re-scan the retained lines with the same grouping core the flush
    // uses, so fingerprints and spans match the stored issues exactly.
    let persisted: Vec<(i64, i64, String, String)> =
        sqlx::query_as("SELECT seq, ts, stream, line FROM app_log WHERE app_id = ?1 ORDER BY seq")
            .bind(app_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let refs: Vec<(i64, &str)> = persisted
        .iter()
        .map(|(_, ts, _, line)| (*ts, line.as_str()))
        .collect();
    let mut buckets = vec![0i64; SPARK_BUCKETS];
    let start = now_ms - SPARK_BUCKETS as i64 * 3_600_000;
    let mut recent_span: Option<(usize, usize)> = None;
    for span in scan_spans(&refs) {
        if fingerprint(&span.event.message, span.event.frame.as_deref()) != wanted {
            continue;
        }
        let last_ms = span.event.last_ts * 1000;
        if last_ms >= start {
            let idx = ((last_ms - start) / 3_600_000)
                .min(SPARK_BUCKETS as i64 - 1)
                .max(0) as usize;
            buckets[idx] += 1;
        }
        recent_span = Some((span.start, span.end));
    }
    // The newest matching event's own lines, oldest first.
    let mut recent: Vec<LogLine> = Vec::new();
    if let Some((span_start, span_end)) = recent_span {
        let from = span_start.max(span_end.saturating_sub(DETAIL_RECENT_LINES));
        for (seq, ts, stream, line) in &persisted[from..span_end.min(persisted.len())] {
            recent.push(LogLine {
                seq: *seq as u64,
                ts: *ts,
                stream: stream.clone(),
                line: line.clone(),
                fingerprint: Some(wanted.to_string()),
            });
        }
    }
    let sparkline = buckets
        .into_iter()
        .enumerate()
        .map(|(i, count)| SparkBucket {
            start: start + i as i64 * 3_600_000,
            count,
        })
        .collect();
    Ok(Some(IssueDetail {
        issue,
        sparkline,
        recent,
    }))
}

/// Persisted lines for one app merged with its live ring on the same `seq`
/// cursor: `since` keeps the OLDEST `limit` lines past it (polling), no
/// `since` keeps the newest `limit` tail. Same JSON shape as the ring-only
/// reader it replaces.
pub async fn read_merged(
    pool: &SqlitePool,
    fleets: &FleetManager,
    app_id: &str,
    slug: &str,
    since: Option<u64>,
    limit: usize,
) -> Vec<Value> {
    let persisted: Vec<(i64, i64, String, String)> =
        sqlx::query_as("SELECT seq, ts, stream, line FROM app_log WHERE app_id = ?1 ORDER BY seq")
            .bind(app_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let mut by_seq: std::collections::BTreeMap<u64, (i64, String, String)> = persisted
        .into_iter()
        .map(|(seq, ts, stream, line)| (seq as u64, (ts, stream, line)))
        .collect();
    if let Some(ring) = fleets.logs(slug) {
        for entry in ring_snapshot(&ring).await {
            by_seq.insert(
                entry.seq,
                (
                    entry.ts,
                    format!("{:?}", entry.stream).to_ascii_lowercase(),
                    entry.line,
                ),
            );
        }
    }
    let entries: Vec<(u64, (i64, String, String))> = by_seq.into_iter().collect();
    // `tail` is newest-first in spirit: ring order is chronological, so
    // the newest `limit` are the last ones.
    let entries = match since {
        Some(seq) => cursor_page(
            entries.into_iter().filter(|(s, _)| *s > seq).collect(),
            limit,
        ),
        None => {
            let skip = entries.len().saturating_sub(limit);
            entries.into_iter().skip(skip).collect()
        }
    };
    entries
        .into_iter()
        .map(|(seq, (ts, stream, line))| {
            json!({"seq": seq, "ts": ts, "stream": stream, "line": line})
        })
        .collect()
}

/// The app's grouped issues, newest first (at most [`MAX_ISSUES`]).
/// `status` filters to one of `open`/`resolved`/`ignored` when set.
pub async fn issues(
    pool: &SqlitePool,
    app_id: &str,
    status: Option<&str>,
) -> anyhow::Result<Vec<Issue>> {
    type IssueRow = (
        String,
        String,
        Option<String>,
        i64,
        i64,
        i64,
        String,
        Option<i64>,
    );
    let rows: Vec<IssueRow> = if let Some(status) = status {
        anyhow::ensure!(
            matches!(status, "open" | "resolved" | "ignored"),
            "unknown status {status:?}"
        );
        sqlx::query_as(
            "SELECT fingerprint, message, frame, count, first_seen, last_seen, status, resolved_at \
             FROM error_issue WHERE app_id = ?1 AND status = ?2 \
             ORDER BY last_seen DESC, fingerprint ASC LIMIT ?3",
        )
        .bind(app_id)
        .bind(status)
        .bind(MAX_ISSUES as i64)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            "SELECT fingerprint, message, frame, count, first_seen, last_seen, status, resolved_at \
             FROM error_issue WHERE app_id = ?1 \
             ORDER BY last_seen DESC, fingerprint ASC LIMIT ?2",
        )
        .bind(app_id)
        .bind(MAX_ISSUES as i64)
        .fetch_all(pool)
        .await?
    };
    Ok(rows
        .into_iter()
        .map(
            |(fingerprint, message, frame, count, first_seen, last_seen, status, resolved_at)| {
                Issue {
                    fingerprint,
                    message,
                    frame,
                    count,
                    first_seen,
                    last_seen,
                    status,
                    resolved_at,
                }
            },
        )
        .collect())
}

/// `true` when an `open` issue is a regression: it was resolved before
/// and a later event reopened it (see [`upsert_issue`]). The console
/// badges these `Regressed`.
pub fn is_regressed(issue: &Issue) -> bool {
    issue.status == "open" && issue.resolved_at.is_some()
}

/// Per-status issue totals for one app (uncapped, unlike [`issues`]), for
/// the console's Open / Resolved / Ignored tabs.
pub async fn status_counts(pool: &SqlitePool, app_id: &str) -> anyhow::Result<(i64, i64, i64)> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) FROM error_issue WHERE app_id = ?1 GROUP BY status",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await?;
    let mut open = 0;
    let mut resolved = 0;
    let mut ignored = 0;
    for (status, count) in rows {
        match status.as_str() {
            "open" => open = count,
            "resolved" => resolved = count,
            "ignored" => ignored = count,
            _ => {}
        }
    }
    Ok((open, resolved, ignored))
}

/// Most fingerprints one triage call touches: the `IN` list stays small
/// and a bulk call can't rewrite an app's whole issue table by accident.
pub const MAX_BULK_TRIAGE: usize = 100;

/// Move issues to `open` / `resolved` / `ignored`. Resolve and ignore
/// stamp `resolved_at` (drives automatic regression reopen in
/// [`upsert_issue`]); reopen clears it, so only an automatic reopen
/// reads as regressed. Returns the issues moved. Unknown fingerprints
/// simply match nothing.
pub async fn set_status(
    pool: &SqlitePool,
    app_id: &str,
    fingerprints: &[&str],
    status: &str,
    now_ms: i64,
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        matches!(status, "open" | "resolved" | "ignored"),
        "unknown status {status:?}"
    );
    anyhow::ensure!(
        !fingerprints.is_empty() && fingerprints.len() <= MAX_BULK_TRIAGE,
        "triage takes 1..={MAX_BULK_TRIAGE} fingerprints"
    );
    for fp in fingerprints {
        anyhow::ensure!(!fp.is_empty() && fp.len() <= 64, "bad fingerprint {fp:?}");
    }
    // The `IN` list is dynamic by length only: every element rides a
    // bind parameter, so fingerprints never touch the SQL text.
    let resolved_at: Option<i64> = match status {
        "open" => None,
        _ => Some(now_ms),
    };
    let mut builder: sqlx::QueryBuilder<sqlx::Sqlite> =
        sqlx::QueryBuilder::new("UPDATE error_issue SET status = ");
    builder.push_bind(status);
    builder.push(", resolved_at = ");
    builder.push_bind(resolved_at);
    builder.push(" WHERE app_id = ");
    builder.push_bind(app_id);
    builder.push(" AND fingerprint IN (");
    let mut separated = builder.separated(", ");
    for fp in fingerprints {
        separated.push_bind(fp);
    }
    separated.push_unseparated(")");
    Ok(builder.build().execute(pool).await?.rows_affected())
}

/// Proxy 5xx over the trailing hour, from the already-flushed per-minute
/// rows (spec §13). `now_ms` is [`crate::apps::now_ms`].
pub async fn s5xx_last_hour(pool: &SqlitePool, app_id: &str, now_ms: i64) -> anyhow::Result<i64> {
    let cutoff_minute = (now_ms - 3_600_000) / 60_000 * 60_000;
    let (sum,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(s5xx), 0) FROM metric_minute WHERE app_id = ?1 AND minute >= ?2",
    )
    .bind(app_id)
    .bind(cutoff_minute)
    .fetch_one(pool)
    .await?;
    Ok(sum)
}

/// Delete grouped errors whose last sighting is older than the owning
/// account's plan retention (spec §3, owner-set 2026-10-26: 24 h Free,
/// 30 d Pro). Returns the rows removed.
pub async fn prune_errors(pool: &SqlitePool, now_ms: i64) -> anyhow::Result<u64> {
    let free_cutoff = now_ms - crate::limits::FREE_LOG_RETENTION_MS;
    let pro_cutoff = now_ms - crate::limits::PRO_LOG_RETENTION_MS;
    let pro = sqlx::query(
        "DELETE FROM error_issue WHERE last_seen < ?1 AND app_id IN \
         (SELECT app.id FROM app JOIN user ON user.id = app.owner_id \
          WHERE user.plan = 'pro')",
    )
    .bind(pro_cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    let rest = sqlx::query(
        "DELETE FROM error_issue WHERE last_seen < ?1 AND app_id NOT IN \
         (SELECT app.id FROM app JOIN user ON user.id = app.owner_id \
          WHERE user.plan = 'pro')",
    )
    .bind(free_cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(pro + rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(lines: &[&str]) -> Vec<ErrorEvent> {
        scan_errors(
            lines
                .iter()
                .enumerate()
                .map(|(i, l)| (1_700_000_000 + i as i64, *l)),
        )
    }

    #[test]
    fn polling_pages_through_every_line() {
        let ring: Vec<u64> = (1..=7).collect();
        let mut seen = Vec::new();
        let mut since = 0;
        loop {
            let page = cursor_page(ring.iter().copied().filter(|seq| *seq > since).collect(), 3);
            let Some(last) = page.last().copied() else {
                break;
            };
            seen.extend(page);
            since = last;
        }
        assert_eq!(seen, ring, "no line falls behind the cursor");
    }

    /// Captured from a real fleet on the VPS (celld 0.6.2): a Worker's
    /// uncaught `JSON.parse` failure arrives as tracing INFO under
    /// `cell_console`, the console level as the first word, then V8 frames.
    #[test]
    fn real_celld_console_error_groups_into_one_event() {
        let lines = [
            "2026-10-24T11:01:41.495156Z  WARN celld: HTTP connections failed event=\"http_connection_failures\" total=1",
            "2026-10-24T11:01:42.855685Z  INFO cell_console: ERROR SyntaxError: Unexpected token 'o', \"not json {\" is not valid JSON",
            "    at JSON.parse (<anonymous>)",
            "    at worker.js:11034:57",
            "    at async handlePageMutation (worker.js:12973:32)",
            "2026-10-24T11:01:43.101010Z  INFO cell_console: LOG served / in 3ms",
            "2026-10-24T11:01:44.000001Z  INFO cell_console: ERROR SyntaxError: Unexpected token 'g', \"garbage\" is not valid JSON",
            "    at JSON.parse (<anonymous>)",
        ];
        let events = scan_errors(lines.iter().enumerate().map(|(i, l)| (i as i64, *l)));
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events[0].message.contains("SyntaxError: Unexpected token"),
            "{}",
            events[0].message
        );
        // Same error, different input: one issue.
        assert_eq!(
            fingerprint(&events[0].message, events[0].frame.as_deref()),
            fingerprint(&events[1].message, events[1].frame.as_deref()),
        );
        // console.log / console.warn through cell_console are not errors.
        assert!(!is_error_start(
            "2026-10-24T11:01:43Z  INFO cell_console: LOG all good"
        ));
        assert!(!is_error_start(
            "2026-10-24T11:01:43Z  INFO cell_console: WARN slow ERROR path"
        ));
    }

    #[test]
    fn error_line_shapes() {
        // workerd/V8 uncaught errors.
        assert!(is_error_start("Uncaught Error: boom"));
        assert!(is_error_start("Uncaught (in promise) TypeError: no"));
        assert!(is_error_start("Error: something broke"));
        assert!(is_error_start(
            "TypeError: Cannot read properties of undefined"
        ));
        assert!(is_error_start("ReferenceError: x is not defined"));
        // console.error through tracing / OTel severity text.
        assert!(is_error_start("ERROR something failed"));
        assert!(is_error_start(
            "2026-10-24T09:41:44.123Z  ERROR cell_console: console.error nope"
        ));
        assert!(is_error_start(
            "2026-10-24T09:41:44.123Z  ERROR celld::console: boom"
        ));
        assert!(is_error_start("error: script error"));
        assert!(is_error_start("Traceback (most recent call last):"));
        // Ordinary lines are not errors.
        assert!(!is_error_start("GET / 200 3ms"));
        assert!(!is_error_start("console.log hello"));
        assert!(!is_error_start("INFO cell_console: serving"));
        assert!(!is_error_start("error rate 1.2% over 5m"));
        assert!(!is_error_start("0 errors this hour"));
        // An ERROR token in the message body is not error-ness: only the
        // tracing level field (or an explicit marker) decides.
        assert!(!is_error_start(
            "2026-10-24T09:41:44.123Z  INFO celld::console: my ERROR handling broke nothing"
        ));
        assert!(!is_error_start(
            "INFO celld::console: visitor said ERROR loudly"
        ));
        assert!(!is_error_start("WARN celld::console: error counts are up"));
        // Explicit markers still fire under a non-error level.
        assert!(is_error_start("INFO celld::console: console.error boom"));
        assert!(is_error_start(
            "2026-10-24T09:41:44.123Z  INFO celld::console: TypeError: bad shape"
        ));
    }

    #[test]
    fn v8_stack_groups_as_one_event() {
        let events = scan(&[
            "GET / 200 1ms",
            "Error: boom",
            "    at handler (file:///app/index.js:10:5)",
            "    at fetch (file:///app/index.js:20:3)",
            "GET / 200 1ms",
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "Error: boom");
        assert_eq!(
            events[0].frame.as_deref(),
            Some("at handler (file:///app/index.js:10:5)")
        );
    }

    #[test]
    fn python_traceback_groups_with_trailing_message() {
        let events = scan(&[
            "Traceback (most recent call last):",
            "  File \"/app/entry.py\", line 10, in fetch",
            "    return 1 // 0",
            "ZeroDivisionError: division by zero",
        ]);
        assert_eq!(events.len(), 1, "{events:?}");
        // The trailing exception line carries the error; the header alone
        // would group every Python failure in one issue.
        assert_eq!(events[0].message, "ZeroDivisionError: division by zero");
        assert_eq!(
            events[0].frame.as_deref(),
            Some("File \"/app/entry.py\", line 10, in fetch")
        );
    }

    #[test]
    fn bare_error_lines_are_separate_events() {
        let events = scan(&["Error: one", "Error: two"]);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn non_error_lines_are_ignored() {
        assert!(scan(&["GET / 200", "console.log hi", "serving on :80"]).is_empty());
    }

    #[test]
    fn fingerprint_survives_ids_ports_and_hashes() {
        let a = fingerprint(
            "TypeError: Cannot read properties of undefined (reading 'user-4821')",
            Some("at handler (file:///app/index.js:10:5)"),
        );
        let b = fingerprint(
            "TypeError: Cannot read properties of undefined (reading 'user-9917')",
            Some("    at handler (file:///app/index.js:44:2)"),
        );
        assert_eq!(a, b, "same failure with different ids");
        let c = fingerprint("Error: disk full", None);
        assert_ne!(a, c, "different failures stay apart");
        // UUIDs and hex nonces normalize away too.
        let d =
            normalize_message("request 550e8400-e29b-41d4-a716-446655440000 failed after 3 tries");
        assert_eq!(d, "request # failed after # tries");
        let e = normalize_message("sha 3f2a9c1e5d port 20001");
        assert_eq!(e, "sha # port #");
    }

    #[test]
    fn log_prefix_does_not_move_the_fingerprint() {
        let a = fingerprint("Error: boom", None);
        let b = fingerprint(
            "2026-10-24T09:41:44.123Z  ERROR celld::console: Error: boom",
            None,
        );
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn flush_persists_groups_and_prunes() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let app_id = "app1";
        let mut ring = LogRing::new(2000);
        for line in [
            "serving on :8080",
            "Error: kaboom 12345",
            "    at handler (file:///app/index.js:10:5)",
            "TypeError: bad id user-9917",
        ] {
            ring.push(LogStream::Stderr, line.to_string());
        }
        let entries: Vec<LogEntry> = ring.tail(100);
        let n = flush_fleet(&pool, app_id, &entries).await.unwrap();
        assert_eq!(n, 4);
        // Re-flushing the same snapshot is a no-op.
        assert_eq!(flush_fleet(&pool, app_id, &entries).await.unwrap(), 0);
        // Lines persisted with global seqs.
        let seqs: Vec<i64> =
            sqlx::query_scalar("SELECT seq FROM app_log WHERE app_id = ?1 ORDER BY seq")
                .bind(app_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
        // Two issues grouped.
        let found = issues(&pool, app_id, None).await.unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].count, 1);
        assert!(found.iter().all(|i| i.status == "open"));
        // A repeat of the same failure upserts the same row.
        let mut ring2 = LogRing::new(2000);
        for line in [
            "serving on :8080",
            "Error: kaboom 12345",
            "    at handler (file:///app/index.js:10:5)",
            "TypeError: bad id user-9917",
            "Error: kaboom 67890",
            "    at handler (file:///app/index.js:11:9)",
        ] {
            ring2.push(LogStream::Stderr, line.to_string());
        }
        let entries2: Vec<LogEntry> = ring2.tail(100);
        flush_fleet(&pool, app_id, &entries2).await.unwrap();
        let found = issues(&pool, app_id, None).await.unwrap();
        assert_eq!(found.len(), 2, "same failures regroup: {found:?}");
        let kaboom = found.iter().find(|i| i.message.contains("kaboom")).unwrap();
        assert_eq!(kaboom.count, 2);
        // Status filter.
        assert_eq!(issues(&pool, app_id, Some("open")).await.unwrap().len(), 2);
        assert!(issues(&pool, app_id, Some("resolved"))
            .await
            .unwrap()
            .is_empty());
        assert!(issues(&pool, "missing", None).await.unwrap().is_empty());
        // Prune keeps the plan window (no app row reads as Free: 24 h).
        let old: i64 = crate::apps::now_ms() - crate::limits::FREE_LOG_RETENTION_MS - 1;
        sqlx::query("UPDATE error_issue SET last_seen = ?1 WHERE app_id = ?2")
            .bind(old)
            .bind(app_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(prune_errors(&pool, crate::apps::now_ms()).await.unwrap(), 2);
        assert!(issues(&pool, app_id, None).await.unwrap().is_empty());
        pool.close().await;
    }

    #[tokio::test]
    async fn flush_prunes_to_the_line_cap() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let app_id = "app1";
        let mut ring = LogRing::new(10_000);
        for i in 0..2100 {
            ring.push(LogStream::Stdout, format!("line {i}"));
        }
        let entries: Vec<LogEntry> = ring.tail(10_000);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app_log WHERE app_id = ?1")
            .bind(app_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, MAX_PERSISTED_LINES);
        let min: i64 = sqlx::query_scalar("SELECT MIN(seq) FROM app_log WHERE app_id = ?1")
            .bind(app_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(min, 100, "the newest 2,000 survive");
        pool.close().await;
    }

    #[tokio::test]
    async fn flush_caps_issues_per_app_with_overflow() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let app_id = "app1";
        // Fill the app to the cap with distinct errors (letters alone do
        // not normalize away, so each line is its own fingerprint).
        let mut ring = LogRing::new(10_000);
        for i in 0..MAX_ERROR_ISSUES {
            let ch = (b'a' + (i % 26) as u8) as char;
            ring.push(
                LogStream::Stderr,
                format!("Error: failure {ch} {}", "x".repeat(i / 26)),
            );
        }
        let entries: Vec<LogEntry> = ring.tail(10_000);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM error_issue WHERE app_id = ?1")
            .bind(app_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, MAX_ERROR_ISSUES as i64);
        // A repeat of a capped fingerprint still updates its row.
        let mut ring2 = LogRing::new_with_next(10, entries.len() as u64);
        ring2.push(LogStream::Stderr, "Error: failure a ".to_string());
        let entries2: Vec<LogEntry> = ring2.tail(10);
        flush_fleet(&pool, app_id, &entries2).await.unwrap();
        // New fingerprints past the cap land in the one overflow issue.
        let mut ring3 = LogRing::new_with_next(10, (entries.len() + entries2.len()) as u64);
        for word in ["zzznewone", "zzznewtwo"] {
            ring3.push(LogStream::Stderr, format!("Error: {word} happened"));
        }
        let entries3: Vec<LogEntry> = ring3.tail(10);
        flush_fleet(&pool, app_id, &entries3).await.unwrap();
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT fingerprint, count FROM error_issue WHERE app_id = ?1")
                .bind(app_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), MAX_ERROR_ISSUES + 1, "cap rows plus overflow");
        let overflow = rows
            .iter()
            .find(|(fp, _)| fp == OVERFLOW_FINGERPRINT)
            .expect("overflow issue exists");
        assert_eq!(overflow.1, 2, "both new fingerprints counted once each");
        let msg: String = sqlx::query_scalar(
            "SELECT message FROM error_issue WHERE app_id = ?1 AND fingerprint = ?2",
        )
        .bind(app_id)
        .bind(OVERFLOW_FINGERPRINT)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(msg, OVERFLOW_MESSAGE);
        pool.close().await;
    }

    #[tokio::test]
    async fn read_merged_sees_persisted_lines_with_an_empty_ring() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let app_id = "app1";
        let mut ring = LogRing::new(2000);
        ring.push(LogStream::Stdout, "hello persisted".to_string());
        let entries: Vec<LogEntry> = ring.tail(10);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        // A fake restart: the manager tracks no fleet, so only SQLite
        // answers — the persisted lines still serve.
        let dir2 = tempfile::tempdir().unwrap();
        let fleets = FleetManager::new(crate::fleet::manager::ManagerSettings {
            data_dir: dir2.path().to_path_buf(),
            celld_bin: "celld".to_string(),
            storage: crate::fleet::manager::FleetStorage {
                endpoint: "http://127.0.0.1:1".to_string(),
                region: "us-east-1".to_string(),
                bucket: "b".to_string(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "k".to_string(),
                    secret_access_key: "s".to_string(),
                },
            },
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: "1".to_string(),
            sleep_after: std::time::Duration::from_secs(60),
            credential_refresh_after: std::time::Duration::from_secs(60),
            wake_timeout: std::time::Duration::from_secs(1),
            wake_concurrency: 1,
            always_on_max: 10,
            preview_max: 20,
        });
        let lines = read_merged(&pool, &fleets, app_id, "ghost", None, 100).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["line"], "hello persisted");
        assert_eq!(lines[0]["seq"], 0);
        pool.close().await;
    }

    #[tokio::test]
    async fn s5xx_comes_from_metric_minute() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let now = crate::apps::now_ms();
        let minute = now / 60_000 * 60_000;
        for (ago_minutes, s5xx) in [(0, 4), (1, 7), (61, 100)] {
            sqlx::query(
                "INSERT INTO metric_minute (app_id, minute, requests, s4xx, s5xx, latency_hist, bytes_out, visitors) \
                 VALUES ('app1', ?1, 10, 0, ?2, '', 0, 0)",
            )
            .bind(minute - ago_minutes * 60_000)
            .bind(s5xx)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Only the trailing hour counts: 4 + 7, not the 100.
        assert_eq!(s5xx_last_hour(&pool, "app1", now).await.unwrap(), 11);
        assert_eq!(s5xx_last_hour(&pool, "other", now).await.unwrap(), 0);
        pool.close().await;
    }

    async fn triage_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        (dir, pool)
    }

    fn flush_lines(start: u64, lines: &[&str]) -> Vec<LogEntry> {
        use crate::fleet::supervisor::{LogRing, LogStream};
        // Blocking in a test: the ring is in-memory and tiny.
        let mut ring = LogRing::new_with_next(lines.len().max(1), start);
        for line in lines {
            ring.push(LogStream::Stderr, line.to_string());
        }
        ring.tail(lines.len().max(1))
    }

    #[tokio::test]
    async fn resolved_issue_recurs_reopens_as_regressed() {
        let (_dir, pool) = triage_pool().await;
        let app_id = "app1";
        let entries = flush_lines(0, &["Error: kaboom"]);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let fp = issues(&pool, app_id, None).await.unwrap()[0]
            .fingerprint
            .clone();
        // Stamp the resolve a minute back: stored event times are
        // second-granularity, so `now` would race the test's own flush.
        let resolved_at = crate::apps::now_ms() - 60_000;
        set_status(&pool, app_id, &[&fp], "resolved", resolved_at)
            .await
            .unwrap();
        let issue = issues(&pool, app_id, Some("resolved")).await.unwrap();
        assert_eq!(issue.len(), 1);
        assert_eq!(issue[0].resolved_at, Some(resolved_at));
        // A new event after resolved_at reopens automatically but keeps
        // the stamp, so the console badges it regressed.
        let entries = flush_lines(1, &["Error: kaboom"]);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let open = issues(&pool, app_id, Some("open")).await.unwrap();
        assert_eq!(open.len(), 1, "resolved issue recurs reopens");
        assert_eq!(open[0].fingerprint, fp);
        assert_eq!(open[0].count, 2, "recurrence counts");
        assert_eq!(
            open[0].resolved_at,
            Some(resolved_at),
            "stamp kept for the regressed badge"
        );
        assert!(is_regressed(&open[0]));
        assert!(issues(&pool, app_id, Some("resolved"))
            .await
            .unwrap()
            .is_empty());
        pool.close().await;
    }

    #[tokio::test]
    async fn ignored_issue_keeps_counting_and_stays_ignored() {
        let (_dir, pool) = triage_pool().await;
        let app_id = "app1";
        let entries = flush_lines(0, &["Error: noisy"]);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let fp = issues(&pool, app_id, None).await.unwrap()[0]
            .fingerprint
            .clone();
        set_status(&pool, app_id, &[&fp], "ignored", crate::apps::now_ms())
            .await
            .unwrap();
        let entries = flush_lines(1, &["Error: noisy"]);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let ignored = issues(&pool, app_id, Some("ignored")).await.unwrap();
        assert_eq!(ignored.len(), 1, "ignored issues never reopen");
        assert_eq!(ignored[0].count, 2, "recurrences still count");
        assert!(!is_regressed(&ignored[0]), "ignored is not a regression");
        pool.close().await;
    }

    #[tokio::test]
    async fn set_status_round_trips_and_rejects_shapes() {
        let (_dir, pool) = triage_pool().await;
        let app_id = "app1";
        let entries = flush_lines(0, &["Error: flip"]);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let fp = issues(&pool, app_id, None).await.unwrap()[0]
            .fingerprint
            .clone();
        let now = crate::apps::now_ms();
        assert_eq!(
            set_status(&pool, app_id, &[&fp], "resolved", now)
                .await
                .unwrap(),
            1
        );
        let resolved = issues(&pool, app_id, Some("resolved")).await.unwrap();
        assert_eq!(resolved[0].resolved_at, Some(now));
        // A manual reopen clears the stamp: only automatic reopens read
        // as regressed.
        assert_eq!(
            set_status(&pool, app_id, &[&fp], "open", now + 1)
                .await
                .unwrap(),
            1
        );
        let open = issues(&pool, app_id, Some("open")).await.unwrap();
        assert_eq!(open[0].resolved_at, None);
        assert!(!is_regressed(&open[0]));
        assert_eq!(
            set_status(&pool, app_id, &["nope"], "open", now)
                .await
                .unwrap(),
            0
        );
        assert!(set_status(&pool, app_id, &[], "open", now).await.is_err());
        assert!(set_status(&pool, app_id, &[&fp], "bogus", now)
            .await
            .is_err());
        assert!(set_status(&pool, app_id, &[""], "open", now).await.is_err());
        let too_many: Vec<&str> = (0..=MAX_BULK_TRIAGE).map(|_| "fp").collect();
        assert!(set_status(&pool, app_id, &too_many, "open", now)
            .await
            .is_err());
        pool.close().await;
    }

    #[tokio::test]
    async fn status_counts_total_per_status() {
        let (_dir, pool) = triage_pool().await;
        let app_id = "app1";
        let entries = flush_lines(0, &["Error: one", "Error: two", "Error: three"]);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let found = issues(&pool, app_id, None).await.unwrap();
        assert_eq!(found.len(), 3);
        set_status(&pool, app_id, &[&found[0].fingerprint], "resolved", 1)
            .await
            .unwrap();
        set_status(&pool, app_id, &[&found[1].fingerprint], "ignored", 1)
            .await
            .unwrap();
        assert_eq!(status_counts(&pool, app_id).await.unwrap(), (1, 1, 1));
        assert_eq!(status_counts(&pool, "missing").await.unwrap(), (0, 0, 0));
        pool.close().await;
    }

    fn empty_fleets() -> crate::fleet::manager::FleetManager {
        let dir = tempfile::tempdir().unwrap();
        // The TempDir drops at the end of this fn, but the manager only
        // keeps the path; the pool below is a separate tempdir that
        // outlives the test. Manager construction never touches the dir.
        std::mem::forget(dir);
        crate::fleet::manager::FleetManager::new(crate::fleet::manager::ManagerSettings {
            data_dir: std::path::PathBuf::from("/nonexistent-test-dir"),
            celld_bin: "celld".to_string(),
            storage: crate::fleet::manager::FleetStorage {
                endpoint: "http://127.0.0.1:1".to_string(),
                region: "us-east-1".to_string(),
                bucket: "b".to_string(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "k".to_string(),
                    secret_access_key: "s".to_string(),
                },
            },
            cgroups: None,
            memory_max_bytes: 0,
            cpu_max: "1".to_string(),
            sleep_after: std::time::Duration::from_secs(60),
            credential_refresh_after: std::time::Duration::from_secs(60),
            wake_timeout: std::time::Duration::from_secs(1),
            wake_concurrency: 1,
            always_on_max: 10,
            preview_max: 20,
        })
    }

    #[tokio::test]
    async fn read_filtered_search_stream_level_and_time() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let (_dir, pool) = triage_pool().await;
        let fleets = empty_fleets();
        let app_id = "app1";
        let mut ring = LogRing::new(10);
        ring.push(LogStream::Stdout, "INFO serving on :8080".to_string());
        ring.push(LogStream::Stdout, "console.log hello visitor".to_string());
        ring.push(LogStream::Stderr, "ERROR disk full on vol-9".to_string());
        ring.push(LogStream::Stderr, "WARN retrying fetch".to_string());
        let entries = ring.tail(10);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let all = LogFilter::default();
        assert_eq!(
            read_filtered(&pool, &fleets, app_id, "ghost", &all, LogCursor::Tail, 100)
                .await
                .len(),
            4
        );
        // Substring search is case-insensitive and bounded server-side.
        let q = LogFilter {
            q: Some("DISK".to_string()),
            ..Default::default()
        };
        let found = read_filtered(&pool, &fleets, app_id, "ghost", &q, LogCursor::Tail, 100).await;
        assert_eq!(found.len(), 1);
        assert!(found[0].line.contains("disk full"));
        // Stream filter.
        let stderr = LogFilter {
            stream: Some("stderr".to_string()),
            ..Default::default()
        };
        assert_eq!(
            read_filtered(
                &pool,
                &fleets,
                app_id,
                "ghost",
                &stderr,
                LogCursor::Tail,
                100
            )
            .await
            .len(),
            2
        );
        // Level filter: the ERROR line matches error, the WARN line warn,
        // and prefix-less stdout output counts as info.
        for (level, want) in [("error", 1), ("warn", 1), ("info", 2)] {
            let filter = LogFilter {
                level: Some(level.to_string()),
                ..Default::default()
            };
            assert_eq!(
                read_filtered(
                    &pool,
                    &fleets,
                    app_id,
                    "ghost",
                    &filter,
                    LogCursor::Tail,
                    100
                )
                .await
                .len(),
                want,
                "level {level}",
            );
        }
        // Time range in unix ms against second-granularity stored lines.
        let now_ms = crate::apps::now_ms();
        let wide = LogFilter {
            from: Some(now_ms - 3_600_000),
            to: Some(now_ms + 60_000),
            ..Default::default()
        };
        assert_eq!(
            read_filtered(&pool, &fleets, app_id, "ghost", &wide, LogCursor::Tail, 100)
                .await
                .len(),
            4
        );
        let past = LogFilter {
            to: Some(now_ms - 3_600_000),
            ..Default::default()
        };
        assert!(
            read_filtered(&pool, &fleets, app_id, "ghost", &past, LogCursor::Tail, 100)
                .await
                .is_empty()
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn read_filtered_paginates_since_and_before() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let (_dir, pool) = triage_pool().await;
        let fleets = empty_fleets();
        let app_id = "app1";
        let mut ring = LogRing::new(10);
        for i in 0..5 {
            ring.push(LogStream::Stdout, format!("line {i}"));
        }
        let entries = ring.tail(10);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let all = LogFilter::default();
        let tail = read_filtered(&pool, &fleets, app_id, "ghost", &all, LogCursor::Tail, 2).await;
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].line, "line 3");
        assert_eq!(tail[1].line, "line 4");
        // `Load older`: the newest lines strictly older than the seq.
        let older = read_filtered(
            &pool,
            &fleets,
            app_id,
            "ghost",
            &all,
            LogCursor::Before(tail[0].seq),
            2,
        )
        .await;
        assert_eq!(older.len(), 2);
        assert_eq!(older[0].line, "line 1");
        assert_eq!(older[1].line, "line 2");
        // Forward poll: the oldest lines past the cursor.
        let next = read_filtered(
            &pool,
            &fleets,
            app_id,
            "ghost",
            &all,
            LogCursor::Since(1),
            10,
        )
        .await;
        assert_eq!(next.len(), 3);
        assert_eq!(next[0].seq, 2);
        pool.close().await;
    }

    #[tokio::test]
    async fn page_lines_carry_their_issue_fingerprint() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let (_dir, pool) = triage_pool().await;
        let fleets = empty_fleets();
        let app_id = "app1";
        let mut ring = LogRing::new(10);
        ring.push(LogStream::Stdout, "serving on :8080".to_string());
        ring.push(LogStream::Stderr, "Error: kaboom".to_string());
        ring.push(
            LogStream::Stderr,
            "    at handler (file:///app/index.js:10:5)".to_string(),
        );
        let entries = ring.tail(10);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let fp = issues(&pool, app_id, None).await.unwrap()[0]
            .fingerprint
            .clone();
        let page = read_filtered(
            &pool,
            &fleets,
            app_id,
            "ghost",
            &LogFilter::default(),
            LogCursor::Tail,
            10,
        )
        .await;
        assert_eq!(page.len(), 3);
        assert_eq!(
            page[0].fingerprint, None,
            "plain lines carry no fingerprint"
        );
        assert_eq!(page[1].fingerprint.as_deref(), Some(fp.as_str()));
        assert_eq!(
            page[2].fingerprint.as_deref(),
            Some(fp.as_str()),
            "frames link too"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn issue_detail_carries_sparkline_and_recent_lines() {
        use crate::fleet::supervisor::{LogRing, LogStream};
        let (_dir, pool) = triage_pool().await;
        let fleets = empty_fleets();
        let app_id = "app1";
        let mut ring = LogRing::new(10);
        ring.push(LogStream::Stdout, "serving on :8080".to_string());
        ring.push(LogStream::Stderr, "Error: kaboom".to_string());
        ring.push(
            LogStream::Stderr,
            "    at handler (file:///app/index.js:10:5)".to_string(),
        );
        let entries = ring.tail(10);
        flush_fleet(&pool, app_id, &entries).await.unwrap();
        let fp = issues(&pool, app_id, None).await.unwrap()[0]
            .fingerprint
            .clone();
        let now = crate::apps::now_ms();
        let detail = issue_detail(&pool, app_id, &fp, now)
            .await
            .unwrap()
            .expect("issue exists");
        assert_eq!(detail.issue.fingerprint, fp);
        assert_eq!(detail.sparkline.len(), 24, "24 hourly buckets");
        assert_eq!(
            detail.sparkline.iter().map(|b| b.count).sum::<i64>(),
            1,
            "the one event lands in the trailing-day window"
        );
        assert_eq!(detail.recent.len(), 2, "the event's own lines");
        assert!(detail.recent[0].line.contains("kaboom"));
        assert!(detail
            .recent
            .iter()
            .all(|l| l.fingerprint.as_deref() == Some(fp.as_str())));
        assert!(issue_detail(&pool, app_id, "nope", now)
            .await
            .unwrap()
            .is_none());
        let _ = fleets;
        pool.close().await;
    }
}
