//! Per-tool telemetry (Wave 4, migration 0015): one `tool_call` row per MCP
//! `tools/call` — tool name, account, app slug when the call names one,
//! client name from `initialize.clientInfo`, a session-ish key (the caller's
//! credential, so sequences stay visible), latency, ok vs error code,
//! response bytes and timestamp.
//!
//! Arguments and outputs are NEVER stored: they hold code and secrets.
//! Recording runs spawned off the tool call (one cheap insert, client name
//! resolved inside the task), so it never slows the response. Rows older
//! than 30 days go out in the metrics sweep.

use serde_json::Value;
use sqlx::SqlitePool;

use crate::auth::{Credential, Principal};
use crate::mcp::tools::CallError;

/// Tool-call rows older than this are pruned.
pub const RETENTION_MS: i64 = 30 * 86_400_000;

/// What `handle_call` hands the recorder. Only scalar facts: the tool, who
/// called, the app slug when the arguments name one, how long it took,
/// whether it worked and how big the answer was.
#[derive(Debug, Clone)]
pub struct RecordReq {
    pub tool: String,
    pub principal: Option<Principal>,
    pub client_ip: Option<String>,
    pub app_slug: Option<String>,
    pub duration_ms: i64,
    pub ok: bool,
    pub error_code: Option<String>,
    pub response_bytes: i64,
}

impl RecordReq {
    /// Build from the dispatch outcome. `args` is read for the `app` slug
    /// only — never stored.
    pub fn from_outcome(
        tool: &str,
        principal: Option<Principal>,
        client_ip: Option<String>,
        args: Option<&Value>,
        outcome: &Result<Value, CallError>,
        duration_ms: i64,
    ) -> Self {
        let app_slug = args
            .and_then(|a| a.get("app"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let (ok, error_code, response_bytes) = match outcome {
            Ok(result) => (true, None, result.to_string().len() as i64),
            Err(CallError::Rpc(code, message)) => {
                (false, Some(format!("rpc_{code}")), message.len() as i64)
            }
            Err(CallError::Tool(err)) => {
                (false, Some(err.code.to_string()), err.message.len() as i64)
            }
        };
        Self {
            tool: tool.to_string(),
            principal,
            client_ip,
            app_slug,
            duration_ms,
            ok,
            error_code,
            response_bytes,
        }
    }

    fn account_id(&self) -> Option<&str> {
        self.principal.as_ref().map(|p| p.account_id.as_str())
    }

    /// The session-ish key: the caller's credential id, so one client's
    /// sequence of calls stays together. Anonymous calls key on the IP.
    fn session(&self) -> String {
        match self.principal.as_ref().map(|p| &p.credential) {
            Some(Credential::ApiKey { key_id }) => format!("key:{key_id}"),
            Some(Credential::OAuth { grant_id }) => format!("grant:{grant_id}"),
            None => format!("ip:{}", self.client_ip.as_deref().unwrap_or("?")),
        }
    }
}

/// Record one call without slowing it: spawn the insert (plus the client
/// name lookup) onto the runtime. A failed insert only warns.
pub fn spawn_record(pool: SqlitePool, req: RecordReq) {
    tokio::spawn(async move {
        if let Err(err) = record(&pool, &req).await {
            tracing::warn!(error = %format!("{err:#}"), tool = %req.tool, "tool telemetry record failed");
        }
    });
}

/// The insert itself, awaited in tests. Resolves the `initialize.clientInfo`
/// name stored on the credential row; unknown credentials record NULL.
pub async fn record(pool: &SqlitePool, req: &RecordReq) -> anyhow::Result<()> {
    let client = match req.principal.as_ref().map(|p| &p.credential) {
        Some(Credential::ApiKey { key_id }) => {
            sqlx::query_scalar::<_, Option<String>>("SELECT clientName FROM apikey WHERE id = ?1")
                .bind(key_id)
                .fetch_optional(pool)
                .await?
                .flatten()
        }
        Some(Credential::OAuth { grant_id }) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT clientName FROM oauthConsent WHERE id = ?1",
        )
        .bind(grant_id)
        .fetch_optional(pool)
        .await?
        .flatten(),
        None => None,
    };
    let app_id: Option<String> = match req.app_slug.as_deref() {
        Some(slug) => {
            sqlx::query_scalar("SELECT id FROM app WHERE slug = ?1")
                .bind(slug)
                .fetch_optional(pool)
                .await?
        }
        None => None,
    };
    sqlx::query(
        "INSERT INTO tool_call (id, tool, account_id, app_id, app_slug, client, session, \
         duration_ms, ok, error_code, response_bytes, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&req.tool)
    .bind(req.account_id())
    .bind(app_id)
    .bind(req.app_slug.as_deref())
    .bind(client)
    .bind(req.session())
    .bind(req.duration_ms)
    .bind(i64::from(req.ok))
    .bind(req.error_code.as_deref())
    .bind(req.response_bytes)
    .bind(crate::apps::now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete rows older than the 30-day retention. Returns the rows removed.
pub async fn prune(pool: &SqlitePool, now: i64) -> anyhow::Result<u64> {
    let removed = sqlx::query("DELETE FROM tool_call WHERE created_at < ?1")
        .bind(now - RETENTION_MS)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed)
}

/// One tool's aggregates over a window: calls, p50/p95 latency, error rate
/// and average response bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStats {
    pub tool: String,
    pub calls: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub error_rate: f64,
    pub avg_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySummary {
    pub tools: Vec<ToolStats>,
    /// Mean calls per commit across sessions that committed at least once:
    /// the efficiency metric (target: under 5 calls per typical change).
    pub calls_per_commit: f64,
    pub sessions_with_commits: u64,
    pub commits: u64,
    pub calls: u64,
}

fn percentile(sorted: &[u64], rank: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((rank * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// Aggregate `days` (1 or 7) of `tool_call` rows, newest window first.
pub async fn summary(pool: &SqlitePool, days: i64, now: i64) -> anyhow::Result<TelemetrySummary> {
    let days = days.clamp(1, 7);
    let since = now - days * 86_400_000;
    let tools: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT tool FROM tool_call WHERE created_at >= ?1 ORDER BY tool",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    let mut total_calls = 0u64;
    for tool in tools {
        let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT duration_ms, ok, response_bytes FROM tool_call \
             WHERE tool = ?1 AND created_at >= ?2",
        )
        .bind(&tool)
        .bind(since)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            continue;
        }
        let mut durations: Vec<u64> = rows.iter().map(|(d, _, _)| (*d).max(0) as u64).collect();
        durations.sort_unstable();
        let errors = rows.iter().filter(|(_, ok, _)| *ok == 0).count();
        let bytes: i64 = rows.iter().map(|(_, _, b)| *b).sum();
        let calls = rows.len() as u64;
        total_calls += calls;
        out.push(ToolStats {
            tool,
            calls,
            p50_ms: percentile(&durations, 0.5),
            p95_ms: percentile(&durations, 0.95),
            error_rate: errors as f64 / calls as f64,
            avg_bytes: (bytes.max(0) as u64) / calls.max(1),
        });
    }
    // Calls per commit: per session with ≥1 commit, mean(calls / commits).
    let sessions: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT session FROM tool_call WHERE tool = 'commit' AND created_at >= ?1",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    let mut ratios = Vec::new();
    let mut commits = 0u64;
    for session in &sessions {
        let (calls, session_commits): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), SUM(CASE WHEN tool = 'commit' THEN 1 ELSE 0 END) \
             FROM tool_call WHERE session = ?1 AND created_at >= ?2",
        )
        .bind(session)
        .bind(since)
        .fetch_one(pool)
        .await?;
        let session_commits = session_commits.max(0) as u64;
        commits += session_commits;
        if session_commits > 0 {
            ratios.push(calls.max(0) as f64 / session_commits as f64);
        }
    }
    let calls_per_commit = if ratios.is_empty() {
        0.0
    } else {
        ratios.iter().sum::<f64>() / ratios.len() as f64
    };
    Ok(TelemetrySummary {
        tools: out,
        calls_per_commit,
        sessions_with_commits: sessions.len() as u64,
        commits,
        calls: total_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str) -> RecordReq {
        RecordReq {
            tool: tool.to_string(),
            principal: None,
            client_ip: Some("127.0.0.1".to_string()),
            app_slug: None,
            duration_ms: 12,
            ok: true,
            error_code: None,
            response_bytes: 345,
        }
    }

    async fn pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        std::mem::forget(dir);
        pool
    }

    #[derive(Debug, sqlx::FromRow)]
    struct ToolCallRow {
        tool: String,
        account_id: Option<String>,
        app_slug: Option<String>,
        client: Option<String>,
        session: String,
        duration_ms: i64,
        ok: i64,
        error_code: Option<String>,
        response_bytes: i64,
    }

    #[tokio::test]
    async fn records_a_call_without_args_or_output() {
        let pool = pool().await;
        record(&pool, &req("open_app")).await.unwrap();
        let row: ToolCallRow = sqlx::query_as(
            "SELECT tool, account_id, app_slug, client, session, duration_ms, ok, error_code, response_bytes \
             FROM tool_call",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.tool, "open_app");
        assert_eq!(row.account_id, None);
        assert_eq!(row.app_slug, None);
        assert_eq!(row.client, None);
        assert_eq!(row.session, "ip:127.0.0.1");
        assert_eq!(
            (
                row.duration_ms,
                row.ok,
                row.error_code.as_deref(),
                row.response_bytes
            ),
            (12, 1, None, 345)
        );
        // No argument or output column exists: the schema cannot hold them.
        let cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('tool_call')")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(!cols
            .iter()
            .any(|c| c == "args" || c == "arguments" || c == "output"));
    }
    #[tokio::test]
    async fn prunes_rows_past_30_days() {
        let pool = pool().await;
        let now = crate::apps::now_ms();
        for (id, at) in [("fresh", now), ("old", now - RETENTION_MS - 1)] {
            sqlx::query(
                "INSERT INTO tool_call (id, tool, session, duration_ms, ok, response_bytes, created_at) \
                 VALUES (?1, 'open_app', 's', 1, 1, 10, ?2)",
            )
            .bind(id)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(prune(&pool, now).await.unwrap(), 1);
        let left: Vec<String> = sqlx::query_scalar("SELECT id FROM tool_call")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["fresh".to_string()]);
    }

    #[tokio::test]
    async fn summary_stats_and_calls_per_commit() {
        let pool = pool().await;
        let now = crate::apps::now_ms();
        // Session A: 8 calls, 2 commits → 4.0. Session B: 3 calls, 1 commit → 3.0.
        let rows = [
            ("open_app", "a", 10, 1, 100),
            ("read_file", "a", 20, 1, 200),
            ("commit", "a", 500, 1, 50),
            ("commit", "a", 600, 0, 60),
            ("open_app", "a", 30, 1, 100),
            ("read_file", "a", 40, 1, 100),
            ("search_code", "a", 50, 1, 100),
            ("app_logs", "a", 60, 1, 100),
            ("open_app", "b", 15, 1, 100),
            ("commit", "b", 700, 1, 50),
            ("read_file", "b", 25, 0, 100),
        ];
        for (i, (tool, session, dur, ok, bytes)) in rows.iter().enumerate() {
            sqlx::query(
                "INSERT INTO tool_call (id, tool, session, duration_ms, ok, response_bytes, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(format!("r{i}"))
            .bind(tool)
            .bind(session)
            .bind(dur)
            .bind(ok)
            .bind(bytes)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }
        let s = summary(&pool, 1, now).await.unwrap();
        let open = s.tools.iter().find(|t| t.tool == "open_app").unwrap();
        assert_eq!(open.calls, 3);
        assert_eq!((open.p50_ms, open.p95_ms), (15, 30));
        assert_eq!(open.error_rate, 0.0);
        let commit = s.tools.iter().find(|t| t.tool == "commit").unwrap();
        // 1 error in 3 commits.
        assert!((commit.error_rate - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!((s.sessions_with_commits, s.commits, s.calls), (2, 3, 11));
        assert!((s.calls_per_commit - 3.5).abs() < 1e-9);
    }
}
