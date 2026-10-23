//! The gate decision and its deploy integration (spec §15.1, Jev-only).
//!
//! Decision table (handwritten wordlists removed; classification is Jev only):
//!
//! | Signal | Result |
//! | --- | --- |
//! | `profanity ≥ 0.5` | block (reason `profanity`) |
//! | `minors > 0.3`, `phishing > 0.9`, `round(severity) == 3` | block (reason `harmful`) + admin queue |
//! | `hate ≥ 0.5` | record for the admin queue, never block |
//! | Jev unavailable (3 s timeout, one retry) | fail closed: the deploy ends `failed` at step `gate` |
//!
//! Chunk cache (`moderation_text`, per app) is keyed by sha256 of the chunk
//! text with only whitespace/case folding — no leetspeak folding anymore.
//! `clear` chunks are skipped on later deploys; blocked and queued chunks
//! are never recorded, so the next deploy re-checks them.

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use super::extract::{Chunk, TooLarge};
use super::jev::{ChunkScores, JevClient, JevError};
use super::{too_large_message, user_message};

/// Hate Noul at or above this is worth the admin queue (never a block).
const HATE_QUEUE_AT: f32 = 0.5;
/// Excerpts carry the chunk's own text, truncated to about this many chars.
const EXCERPT_CHARS: usize = 100;

/// Why a deploy blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    Profanity,
    Harmful,
    TooLarge,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::Profanity => "profanity",
            BlockReason::Harmful => "harmful",
            BlockReason::TooLarge => "too_large_to_review",
        }
    }
}

/// One gate finding, per the spec §15.1 `findings` shape plus the score.
/// `signal` is always `"jev"`: we no longer know a matched word, so the
/// excerpt is the chunk's own text (truncated), unmasked.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub path: String,
    pub line: u32,
    pub excerpt: String,
    pub category: &'static str,
    pub signal: &'static str,
    pub score: f32,
}

/// The gate's verdict over one deploy.
#[derive(Debug)]
pub enum Decision {
    Pass { queued: bool },
    Block { reason: BlockReason, queue: bool },
}

/// What one chunk's scores mean. Pure; boundary-tested below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkVerdict {
    Clear,
    HateQueue,
    BlockProfanity,
    BlockHarmful,
}

/// Classify one chunk's scores. Hate never blocks, whatever it says.
pub fn judge(scores: &ChunkScores) -> ChunkVerdict {
    if scores.profanity >= 0.5 {
        return ChunkVerdict::BlockProfanity;
    }
    if scores.minors > 0.3 || scores.phishing > 0.9 || scores.severity.round() == 3.0 {
        return ChunkVerdict::BlockHarmful;
    }
    if scores.hate >= HATE_QUEUE_AT {
        return ChunkVerdict::HateQueue;
    }
    ChunkVerdict::Clear
}

/// The gate failed in a way the pipeline must distinguish: Jev down (fail
/// closed as a `failed` deploy) versus a genuine bug or DB error.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("review unavailable")]
    Unavailable(#[source] JevError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Gate statistics for tests and the deploy log.
#[derive(Debug, Default)]
pub struct GateStats {
    pub files: usize,
    pub chunks: usize,
    pub checked: usize,
    pub skipped: usize,
}

/// The gate result the pipeline maps to a deploy outcome.
#[derive(Debug)]
pub enum GateOutcome {
    Pass {
        stats: GateStats,
        queued: bool,
    },
    Block {
        reason: BlockReason,
        findings: Vec<Finding>,
        user_message: String,
        stats: GateStats,
    },
}

/// Cap on findings in one deploy row: the first hit writes `user_message`.
const MAX_FINDINGS: usize = 25;

/// Run the gate over extracted `chunks` for `app_id`: cache lookup, Jev
/// classification of the new chunks, decision, cache + `moderation_check`
/// writes. Jev errors fail closed: `Err(GateError::Unavailable)` caches
/// nothing and records nothing.
///
/// `deploy_id`/`sha` identify the check row (stage `gate`).
pub async fn run_gate(
    pool: &SqlitePool,
    app_id: &str,
    deploy_id: &str,
    sha: &str,
    chunks: Vec<Chunk>,
    jev: &JevClient,
) -> Result<GateOutcome, GateError> {
    let now = crate::apps::now_ms();
    let mut stats = GateStats {
        chunks: chunks.len(),
        ..GateStats::default()
    };
    let mut hashes: Vec<String> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        hashes.push(cache_key(&chunk.text));
    }
    stats.files = chunks
        .iter()
        .map(|c| &c.path)
        .collect::<std::collections::HashSet<_>>()
        .len();

    let cached = cached_verdicts(pool, app_id, &hashes).await?;
    let mut fresh: Vec<(usize, String)> = Vec::new();
    for (i, hash) in hashes.iter().enumerate() {
        if cached
            .get(hash)
            .is_some_and(|v| v == "clear" || v == "allowed")
        {
            stats.skipped += 1;
        } else {
            fresh.push((i, chunks[i].text.clone()));
        }
    }
    stats.checked = fresh.len();

    let mut scores = vec![ChunkScores::default(); chunks.len()];
    if !fresh.is_empty() {
        let texts: Vec<String> = fresh.iter().map(|(_, t)| t.clone()).collect();
        let fresh_scores = jev.classify(&texts).await.map_err(GateError::Unavailable)?;
        for ((i, _), s) in fresh.iter().zip(fresh_scores) {
            scores[*i] = s;
        }
    }

    let mut findings = Vec::new();
    let mut to_record: Vec<(String, &'static str)> = Vec::new();
    let mut profane = false;
    let mut harmful = false;
    let mut hateful = false;
    let mut scored_rows = Vec::with_capacity(chunks.len());
    for ((chunk, hash), chunk_scores) in chunks.iter().zip(&hashes).zip(&scores) {
        let cached_clear = cached
            .get(hash)
            .is_some_and(|v| v == "clear" || v == "allowed");
        if !cached_clear {
            scored_rows.push(scored_row(chunk, chunk_scores));
        }
        match judge(chunk_scores) {
            ChunkVerdict::Clear => {
                if !cached_clear {
                    to_record.push((hash.clone(), "clear"));
                }
            }
            ChunkVerdict::HateQueue => {
                hateful = true;
                push_findings(&mut findings, chunk, chunk_scores);
            }
            ChunkVerdict::BlockProfanity => {
                profane = true;
                push_findings(&mut findings, chunk, chunk_scores);
            }
            ChunkVerdict::BlockHarmful => {
                harmful = true;
                push_findings(&mut findings, chunk, chunk_scores);
            }
        }
    }

    let decision = if profane {
        Decision::Block {
            reason: BlockReason::Profanity,
            queue: harmful,
        }
    } else if harmful {
        Decision::Block {
            reason: BlockReason::Harmful,
            queue: true,
        }
    } else {
        Decision::Pass { queued: hateful }
    };
    let queued = matches!(decision, Decision::Block { queue: true, .. })
        || matches!(decision, Decision::Pass { queued: true });
    let scores_json = serde_json::json!({"queued": queued, "chunks": scored_rows}).to_string();

    let outcome = match decision {
        Decision::Pass { queued } => {
            record_chunks(pool, app_id, &to_record, now).await?;
            write_check(
                pool,
                app_id,
                deploy_id,
                sha,
                if queued { "queued" } else { "pass" },
                &serde_json::to_string(&findings).unwrap_or_else(|_| "[]".to_string()),
                Some(&scores_json),
                now,
            )
            .await?;
            GateOutcome::Pass { stats, queued }
        }
        Decision::Block { reason, .. } => {
            // Blocked and queued chunks are never recorded: the next deploy
            // re-checks them.
            let message = findings.first().map(user_message).unwrap_or_else(|| {
                "Outer didn't publish this change because the page text was blocked by the content gate. Your app still shows the previous version.".to_string()
            });
            let findings_json =
                serde_json::to_string(&findings).unwrap_or_else(|_| "[]".to_string());
            write_check(
                pool,
                app_id,
                deploy_id,
                sha,
                "blocked",
                &findings_json,
                Some(&scores_json),
                now,
            )
            .await?;
            GateOutcome::Block {
                reason,
                findings,
                user_message: message,
                stats,
            }
        }
    };
    Ok(outcome)
}

/// The localhost-dev path: no Jev client (no Cloudflare pair), so nothing
/// is classified. Still writes the `moderation_check` row so the pass is
/// auditable; off localhost the config refuses to boot without the pair,
/// so this never runs there.
pub async fn run_skipped(
    pool: &SqlitePool,
    app_id: &str,
    deploy_id: &str,
    sha: &str,
    chunks: &[Chunk],
) -> anyhow::Result<GateOutcome> {
    let now = crate::apps::now_ms();
    let stats = GateStats {
        files: chunks
            .iter()
            .map(|c| &c.path)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        chunks: chunks.len(),
        ..GateStats::default()
    };
    write_check(pool, app_id, deploy_id, sha, "pass", "[]", None, now).await?;
    Ok(GateOutcome::Pass {
        stats,
        queued: false,
    })
}

/// The over-caps path: no chunks to check, but still a `moderation_check`
/// row so the block is auditable.
pub async fn run_too_large(
    pool: &SqlitePool,
    app_id: &str,
    deploy_id: &str,
    sha: &str,
    too_large: &TooLarge,
) -> anyhow::Result<GateOutcome> {
    let now = crate::apps::now_ms();
    write_check(pool, app_id, deploy_id, sha, "blocked", "[]", None, now).await?;
    Ok(GateOutcome::Block {
        reason: BlockReason::TooLarge,
        findings: Vec::new(),
        user_message: too_large_message(too_large),
        stats: GateStats::default(),
    })
}

/// Cache key: sha256 over the text with only whitespace/case folding. No
/// leetspeak folding anymore — Jev sees the raw text, so the key must not
/// merge texts Jev would judge differently.
pub fn cache_key(text: &str) -> String {
    let folded = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    sha256_hex(&folded)
}

/// One finding per triggered signal on the chunk.
fn push_findings(findings: &mut Vec<Finding>, chunk: &Chunk, scores: &ChunkScores) {
    let triggers: &[(&'static str, f32)] = &[
        ("profanity", scores.profanity),
        ("sexual_minors", scores.minors),
        ("phishing", scores.phishing),
        ("severity", scores.severity),
        ("hate", scores.hate),
    ];
    for (category, value) in triggers {
        let fires = match *category {
            "profanity" => scores.profanity >= 0.5,
            "sexual_minors" => scores.minors > 0.3,
            "phishing" => scores.phishing > 0.9,
            "severity" => scores.severity.round() == 3.0,
            _ => scores.hate >= HATE_QUEUE_AT,
        };
        if fires && findings.len() < MAX_FINDINGS {
            findings.push(Finding {
                path: chunk.path.clone(),
                line: chunk.line,
                excerpt: excerpt(&chunk.text),
                category,
                signal: "jev",
                score: *value,
            });
        }
    }
}

/// The chunk's own text, truncated to ~100 chars. No masking: Jev reports
/// no matched word, and the text is the owner's own.
fn excerpt(text: &str) -> String {
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_string();
    }
    text.chars().take(EXCERPT_CHARS).collect()
}

fn scored_row(chunk: &Chunk, scores: &ChunkScores) -> serde_json::Value {
    serde_json::json!({
        "path": chunk.path,
        "line": chunk.line,
        "profanity": scores.profanity,
        "minors": scores.minors,
        "phishing": scores.phishing,
        "severity": scores.severity,
        "hate": scores.hate,
    })
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

async fn cached_verdicts(
    pool: &SqlitePool,
    app_id: &str,
    hashes: &[String],
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    for hash in hashes {
        if let Some((verdict,)) = sqlx::query_as::<_, (String,)>(
            "SELECT verdict FROM moderation_text WHERE app_id = ? AND text_hash = ?",
        )
        .bind(app_id)
        .bind(hash)
        .fetch_optional(pool)
        .await?
        {
            out.insert(hash.clone(), verdict);
        }
    }
    Ok(out)
}

async fn record_chunks(
    pool: &SqlitePool,
    app_id: &str,
    rows: &[(String, &'static str)],
    now: i64,
) -> anyhow::Result<()> {
    for (hash, verdict) in rows {
        sqlx::query(
            "INSERT INTO moderation_text (app_id, text_hash, verdict, checked_at) VALUES (?, ?, ?, ?)
             ON CONFLICT (app_id, text_hash) DO UPDATE SET verdict = excluded.verdict, checked_at = excluded.checked_at",
        )
        .bind(app_id)
        .bind(hash)
        .bind(*verdict)
        .bind(now)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn write_check(
    pool: &SqlitePool,
    app_id: &str,
    deploy_id: &str,
    sha: &str,
    verdict: &str,
    findings_json: &str,
    scores_json: Option<&str>,
    now: i64,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO moderation_check (id, app_id, deploy_id, sha, stage, findings, scores, verdict, created_at)
         VALUES (?1, ?2, ?3, ?4, 'gate', ?5, ?6, ?7, ?8)",
    )
    .bind(id)
    .bind(app_id)
    .bind(deploy_id)
    .bind(sha)
    .bind(findings_json)
    .bind(scores_json)
    .bind(verdict)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::extract::Chunk;
    use super::super::jev::fake::*;
    use super::*;
    use std::sync::Arc;

    fn chunk(path: &str, line: u32, text: &str) -> Chunk {
        Chunk {
            path: path.to_string(),
            line,
            text: text.to_string(),
        }
    }

    fn clear() -> ChunkScores {
        ChunkScores {
            profanity: 0.01,
            minors: 0.0,
            phishing: 0.0,
            hate: 0.0,
            severity: 0.0,
        }
    }

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO account (id, email, last_active_at, created_at) VALUES ('o', 'o@x.com', 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO app (id, slug, owner_id, uid, last_activity_at, created_at) VALUES ('a', 's', 'o', 1, 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        (dir, pool)
    }

    fn test_client(base_url: &str) -> JevClient {
        let mut config = super::super::jev::JevConfig::new(
            "test-account".to_string(),
            "test-token".to_string(),
            "outer".to_string(),
        );
        config.api_base = base_url.to_string();
        config.timeout = std::time::Duration::from_millis(500);
        JevClient::new(config).unwrap()
    }

    /// Answers from marker words in the chunk text, so tests control Jev.
    fn marker_responder() -> Responder {
        Arc::new(|body| {
            let mut answers = serde_json::Map::new();
            let state = body.pointer("/input/state").unwrap().as_object().unwrap();
            let questions = body
                .pointer("/input/questions")
                .unwrap()
                .as_object()
                .unwrap();
            let mut scored = std::collections::HashMap::new();
            for (key, text) in state {
                let text = text.as_str().unwrap_or("");
                let mut s = clear();
                if text.contains("PROFANE") {
                    s.profanity = 0.9;
                }
                if text.contains("MINORS") {
                    s.minors = 0.8;
                }
                if text.contains("PHISH") {
                    s.phishing = 0.95;
                }
                if text.contains("SEVERE") {
                    s.severity = 3.0;
                }
                if text.contains("HATEFUL") {
                    s.hate = 0.9;
                }
                scored.insert(key.clone(), s);
            }
            for (qid, question) in questions {
                let slot = qid.split('_').next().unwrap_or("");
                let s = scored.get(slot).cloned().unwrap_or_else(clear);
                let value = if question.get("type").and_then(|t| t.as_str()) == Some("score") {
                    serde_json::json!({"type": "score", "score": s.severity})
                } else if qid.ends_with("_profanity") {
                    serde_json::json!({"type": "noul", "noul": s.profanity})
                } else if qid.ends_with("_minors") {
                    serde_json::json!({"type": "noul", "noul": s.minors})
                } else if qid.ends_with("_phishing") {
                    serde_json::json!({"type": "noul", "noul": s.phishing})
                } else {
                    serde_json::json!({"type": "noul", "noul": s.hate})
                };
                answers.insert(qid.clone(), value);
            }
            (200, envelope(serde_json::json!({"answers": answers})))
        })
    }

    #[test]
    fn judge_matches_the_decision_table_at_its_boundaries() {
        // Profanity blocks at exactly 0.5.
        assert_eq!(
            judge(&ChunkScores {
                profanity: 0.5,
                ..clear()
            }),
            ChunkVerdict::BlockProfanity
        );
        assert_eq!(
            judge(&ChunkScores {
                profanity: 0.499,
                ..clear()
            }),
            ChunkVerdict::Clear
        );
        // Minors blocks just above 0.3.
        assert_eq!(
            judge(&ChunkScores {
                minors: 0.3,
                ..clear()
            }),
            ChunkVerdict::Clear
        );
        assert_eq!(
            judge(&ChunkScores {
                minors: 0.3001,
                ..clear()
            }),
            ChunkVerdict::BlockHarmful
        );
        // Phishing blocks just above 0.9.
        assert_eq!(
            judge(&ChunkScores {
                phishing: 0.9,
                ..clear()
            }),
            ChunkVerdict::Clear
        );
        assert_eq!(
            judge(&ChunkScores {
                phishing: 0.9001,
                ..clear()
            }),
            ChunkVerdict::BlockHarmful
        );
        // Severity blocks when it rounds to 3.
        assert_eq!(
            judge(&ChunkScores {
                severity: 2.5,
                ..clear()
            }),
            ChunkVerdict::BlockHarmful
        );
        assert_eq!(
            judge(&ChunkScores {
                severity: 2.4,
                ..clear()
            }),
            ChunkVerdict::Clear
        );
        // Hate queues but never blocks, however certain.
        assert_eq!(
            judge(&ChunkScores {
                hate: 0.499,
                ..clear()
            }),
            ChunkVerdict::Clear
        );
        assert_eq!(
            judge(&ChunkScores {
                hate: 0.5,
                ..clear()
            }),
            ChunkVerdict::HateQueue
        );
        assert_eq!(
            judge(&ChunkScores {
                hate: 0.99,
                ..clear()
            }),
            ChunkVerdict::HateQueue
        );
        // Profanity outranks everything; one bad chunk doesn't clear others.
        assert_eq!(
            judge(&ChunkScores {
                profanity: 0.9,
                minors: 0.9,
                ..clear()
            }),
            ChunkVerdict::BlockProfanity
        );
    }

    #[tokio::test]
    async fn profanity_blocks_with_path_line_excerpt_and_score() {
        let (_d, pool) = test_pool().await;
        let fake = FakeJev::start(marker_responder()).await;
        let out = run_gate(
            &pool,
            "a",
            "d1",
            "sha",
            vec![chunk("pages/index.tsx", 42, "this list is PROFANE awesome")],
            &test_client(&fake.base_url),
        )
        .await
        .unwrap();
        let GateOutcome::Block {
            reason,
            findings,
            user_message,
            ..
        } = out
        else {
            panic!("must block");
        };
        assert_eq!(reason, BlockReason::Profanity);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, "pages/index.tsx");
        assert_eq!(findings[0].line, 42);
        assert_eq!(findings[0].excerpt, "this list is PROFANE awesome");
        assert_eq!(findings[0].category, "profanity");
        assert_eq!(findings[0].signal, "jev");
        assert!((findings[0].score - 0.9).abs() < 1e-6);
        assert!(user_message.contains("pages/index.tsx, line 42"));
        assert!(user_message.contains("strong profanity"));
    }

    #[tokio::test]
    async fn severe_signals_block_as_harmful_and_queue() {
        let (_d, pool) = test_pool().await;
        let fake = FakeJev::start(marker_responder()).await;
        let jev = test_client(&fake.base_url);
        for (text, category) in [
            ("MINORS content", "sexual_minors"),
            ("PHISH content", "phishing"),
            ("SEVERE content", "severity"),
        ] {
            let out = run_gate(&pool, "a", "d1", "s", vec![chunk("a.tsx", 7, text)], &jev)
                .await
                .unwrap();
            let GateOutcome::Block {
                reason, findings, ..
            } = out
            else {
                panic!("{text} must block");
            };
            assert_eq!(reason, BlockReason::Harmful, "{text}");
            assert!(
                findings.iter().any(|f| f.category == category),
                "{text}: {findings:?}"
            );
        }
        let (verdict, scores): (String, String) = sqlx::query_as(
            "SELECT verdict, scores FROM moderation_check WHERE app_id = 'a' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(verdict, "blocked");
        assert!(scores.contains("\"queued\":true"), "{scores}");
    }

    #[tokio::test]
    async fn hate_queues_without_blocking() {
        let (_d, pool) = test_pool().await;
        let fake = FakeJev::start(marker_responder()).await;
        let out = run_gate(
            &pool,
            "a",
            "d1",
            "s",
            vec![chunk("a.tsx", 3, "HATEFUL remarks")],
            &test_client(&fake.base_url),
        )
        .await
        .unwrap();
        let GateOutcome::Pass { queued, .. } = out else {
            panic!("hate never blocks");
        };
        assert!(queued);
        let (verdict, findings): (String, String) =
            sqlx::query_as("SELECT verdict, findings FROM moderation_check WHERE app_id = 'a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(verdict, "queued");
        assert!(findings.contains("\"category\":\"hate\""), "{findings}");
    }

    #[tokio::test]
    async fn long_chunks_are_truncated_to_about_100_chars() {
        let (_d, pool) = test_pool().await;
        let fake = FakeJev::start(marker_responder()).await;
        let text = format!("PROFANE {}", "x".repeat(500));
        let out = run_gate(
            &pool,
            "a",
            "d1",
            "s",
            vec![chunk("a.tsx", 1, &text)],
            &test_client(&fake.base_url),
        )
        .await
        .unwrap();
        let GateOutcome::Block { findings, .. } = out else {
            panic!("must block");
        };
        assert_eq!(findings[0].excerpt.chars().count(), super::EXCERPT_CHARS);
        assert!(findings[0].excerpt.starts_with("PROFANE"));
    }

    #[tokio::test]
    async fn cache_skips_cleared_text_but_rechecks_blocks_and_queues() {
        let (_dir, pool) = test_pool().await;
        let fake = FakeJev::start(marker_responder()).await;
        let jev = test_client(&fake.base_url);
        let chunks = || vec![chunk("a.tsx", 1, "an ordinary sentence")];
        let first = run_gate(&pool, "a", "d1", "s", chunks(), &jev)
            .await
            .unwrap();
        let GateOutcome::Pass { stats, .. } = first else {
            panic!("clear passes");
        };
        assert!(stats.checked > 0);
        let calls = fake.calls();
        let second = run_gate(&pool, "a", "d2", "s", chunks(), &jev)
            .await
            .unwrap();
        let GateOutcome::Pass { stats, .. } = second else {
            panic!("clear passes again");
        };
        assert_eq!(stats.checked, 0, "cached clear chunks are skipped");
        assert!(stats.skipped > 0);
        assert_eq!(fake.calls(), calls, "no Jev call for cached text");

        // Blocked text is never recorded: the next deploy re-examines it.
        let evil = || vec![chunk("a.tsx", 9, "PROFANE again")];
        assert!(matches!(
            run_gate(&pool, "a", "d3", "s", evil(), &jev).await.unwrap(),
            GateOutcome::Block { .. }
        ));
        let before = fake.calls();
        let again = run_gate(&pool, "a", "d4", "s", evil(), &jev).await.unwrap();
        let GateOutcome::Block { stats, .. } = again else {
            panic!("still blocked");
        };
        assert!(stats.checked > 0, "blocked text is re-checked");
        assert_eq!(fake.calls(), before + 1);

        // Queued (hate) text is re-checked too.
        let mean = || vec![chunk("a.tsx", 5, "HATEFUL again")];
        run_gate(&pool, "a", "d5", "s", mean(), &jev).await.unwrap();
        let before = fake.calls();
        let GateOutcome::Pass { stats, .. } =
            run_gate(&pool, "a", "d6", "s", mean(), &jev).await.unwrap()
        else {
            panic!("hate passes");
        };
        assert!(stats.checked > 0, "queued text is re-checked");
        assert_eq!(fake.calls(), before + 1);
    }

    #[tokio::test]
    async fn unavailable_caches_and_records_nothing() {
        let (_dir, pool) = test_pool().await;
        let fake = FakeJev::start(Arc::new(|_| (500, serde_json::json!({"error": "down"})))).await;
        let err = run_gate(
            &pool,
            "a",
            "d1",
            "s",
            vec![chunk("a.tsx", 1, "anything")],
            &test_client(&fake.base_url),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GateError::Unavailable(_)), "{err:?}");
        let (texts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM moderation_text")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(texts, 0, "nothing is cached as clear");
        let (checks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM moderation_check")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(checks, 0, "nothing is recorded");
    }

    #[tokio::test]
    async fn empty_input_passes_without_a_call() {
        let (_dir, pool) = test_pool().await;
        let fake = FakeJev::start(marker_responder()).await;
        let out = run_gate(&pool, "a", "d1", "s", vec![], &test_client(&fake.base_url))
            .await
            .unwrap();
        assert!(matches!(out, GateOutcome::Pass { queued: false, .. }));
        assert_eq!(fake.calls(), 0);
    }

    #[test]
    fn cache_key_folds_only_whitespace_and_case() {
        assert_eq!(cache_key("Hello  World"), cache_key("hello world"));
        assert_eq!(cache_key("a\n\tb"), cache_key("A B"));
        assert_ne!(cache_key("f.u.c.k"), cache_key("fuck"));
        assert_ne!(cache_key("damn"), cache_key("Damn!"));
    }
}
