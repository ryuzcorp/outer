//! The gate decision and its deploy integration (spec §15.1).
//!
//! Decision table, with Jev unavailable (M1 has no API key):
//!
//! | Signal | Result |
//! | --- | --- |
//! | `strong` wordlist hit | block |
//! | Jev `strong_profanity ≥ 0.5` | block |
//! | `ambiguous` hit with Jev `strong_profanity < 0.5` | pass |
//! | `sexual_minors > 0.3`, `phishing > 0.9`, `round(severity) == 3` | block + queue |
//! | Jev unavailable | `strong` blocks, `ambiguous` blocks (fail closed); other text passes as `recheck` |
//!
//! The decision takes an `Option<JevScores>`: `None` is "Jev unavailable".
//! A Jev client later calls the same function with `Some(scores)` — no
//! restructuring needed.
//!
//! Chunk cache (`moderation_text`, per app): chunks already `clear` or
//! `allowed` are skipped. New chunks are recorded: `clear` when only mild
//! (or no) words matched — the Jev question explicitly excludes mild
//! words, so nothing later can flag them; `recheck` when clean but Jev
//! couldn't confirm yet, so the next deploy re-checks them and a late Jev
//! hit can hold the app. Blocked chunks are never recorded, so the next
//! deploy re-examines them. Template chunks get no special-casing:
//! per-app persistence means a template's text is recorded `clear` on the
//! first clean deploy and skipped forever after in that app.

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use super::extract::{Chunk, TooLarge};
use super::normalize::{mask_excerpt, normalize, NormalizedText};
use super::wordlist::{find_evasion, find_spaced, Tier, WordHit};
use super::{too_large_message, user_message};

/// Jev's classification of one text, when available. M1 always passes
/// `None`: no API key yet, so the gate runs on wordlists only.
#[derive(Debug, Clone, Copy)]
pub struct JevScores {
    pub strong_profanity: f32,
    pub sexual_minors: f32,
    pub phishing: f32,
    pub hate: f32,
    pub severity: f32,
}

/// Why a deploy blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    Profanity,
    TooLarge,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::Profanity => "profanity",
            BlockReason::TooLarge => "too_large_to_review",
        }
    }
}

/// One blocked line, per the spec §15.1 `findings` shape.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub path: String,
    pub line: u32,
    pub excerpt: String,
    pub category: &'static str,
    pub signal: &'static str,
}

/// The gate's verdict over one deploy.
#[derive(Debug)]
pub enum Decision {
    Pass,
    Block { reason: BlockReason, queue: bool },
}

/// Pure decision function: wordlist hit counts plus an optional Jev
/// verdict. Later stages (migrations, celld deploy) never see this; the
/// caller maps it to a deploy outcome.
pub fn decide(strong_hits: usize, ambiguous_hits: usize, jev: Option<JevScores>) -> Decision {
    let Some(scores) = jev else {
        // Jev unavailable (M1): fail closed. Strong blocks; ambiguous
        // blocks too; anything else passes and is marked recheck.
        if strong_hits > 0 || ambiguous_hits > 0 {
            return Decision::Block {
                reason: BlockReason::Profanity,
                queue: false,
            };
        }
        return Decision::Pass;
    };
    if scores.strong_profanity >= 0.5 {
        return Decision::Block {
            reason: BlockReason::Profanity,
            queue: false,
        };
    }
    if scores.sexual_minors > 0.3 || scores.phishing > 0.9 || scores.severity.round() == 3.0 {
        return Decision::Block {
            reason: BlockReason::Profanity,
            queue: true,
        };
    }
    if strong_hits > 0 {
        // Belt and braces: a wordlist strong hit blocks even when Jev
        // disagrees, since Jev is weaker outside English (spec §15.4).
        return Decision::Block {
            reason: BlockReason::Profanity,
            queue: false,
        };
    }
    if ambiguous_hits > 0 {
        return Decision::Pass; // Jev cleared it (strong_profanity < 0.5)
    }
    Decision::Pass
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

/// Run the gate over extracted `chunks` for `app_id`: cache lookup,
/// wordlist matching, decision, cache + `moderation_check` writes.
///
/// `deploy_id`/`sha` identify the check row (stage `gate`). `jev` is
/// always `None` in M1.
pub async fn run_gate(
    pool: &SqlitePool,
    app_id: &str,
    deploy_id: &str,
    sha: &str,
    chunks: Vec<Chunk>,
    jev: Option<JevScores>,
) -> anyhow::Result<GateOutcome> {
    let now = crate::apps::now_ms();
    let mut stats = GateStats {
        chunks: chunks.len(),
        ..GateStats::default()
    };
    let mut checked: Vec<(Chunk, NormalizedText)> = Vec::new();
    let mut hashes: Vec<String> = Vec::new();
    for chunk in chunks {
        let n = normalize(&chunk.text);
        let hash = sha256_hex(&n.spaced);
        hashes.push(hash);
        checked.push((chunk, n));
    }
    stats.files = checked
        .iter()
        .map(|(c, _)| &c.path)
        .collect::<std::collections::HashSet<_>>()
        .len();

    let cached = cached_verdicts(pool, app_id, &hashes).await?;
    let mut findings = Vec::new();
    let mut strong_hits = 0;
    let mut ambiguous_hits = 0;
    let mut to_record: Vec<(String, &'static str)> = Vec::new();

    for ((chunk, n), hash) in checked.into_iter().zip(hashes) {
        if let Some(v) = cached.get(&hash) {
            if v == "clear" || v == "allowed" {
                stats.skipped += 1;
                continue;
            }
        }
        stats.checked += 1;
        let hits = all_hits(&n);
        let strong = hits.iter().filter(|h| h.tier == Tier::Strong).count();
        let ambiguous = hits.iter().filter(|h| h.tier == Tier::Ambiguous).count();
        let mild = hits.iter().any(|h| h.tier == Tier::Mild);
        strong_hits += strong;
        ambiguous_hits += ambiguous;
        // Blocked chunks are never recorded: the next deploy re-examines
        // them. Passed chunks record `clear` when only mild (or no) words
        // matched, `recheck` otherwise.
        let blocked_chunk = strong > 0 || (ambiguous > 0 && jev.is_none());
        if !blocked_chunk {
            to_record.push((hash, if mild { "clear" } else { "recheck" }));
        }
        for hit in hits {
            if hit.tier == Tier::Mild || findings.len() >= MAX_FINDINGS {
                continue;
            }
            if hit.tier == Tier::Ambiguous && jev.is_some() {
                continue; // decided by Jev below, not by wordlist
            }
            findings.push(mask_hit(&chunk, &n, &hit));
        }
    }

    let outcome = match decide(strong_hits, ambiguous_hits, jev) {
        Decision::Pass => {
            record_chunks(pool, app_id, &to_record, now).await?;
            write_check(pool, app_id, deploy_id, sha, "pass", "[]", None, now).await?;
            GateOutcome::Pass { stats }
        }
        Decision::Block { reason, queue } => {
            let _ = queue; // M2 wires the admin queue; recorded in the row.
            let message = findings.first().map(user_message).unwrap_or_else(|| {
                "Outer didn't publish this change because the page text was blocked by the profanity gate. Your app still shows the previous version.".to_string()
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
                None,
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

fn all_hits(n: &NormalizedText) -> Vec<WordHit> {
    let mut hits = find_spaced(n);
    hits.extend(find_evasion(n));
    // Deterministic finding order: base position, spaced hits before
    // evasion hits on the same span (one finding per span and tier).
    hits.sort_by(|a, b| {
        (a.base.start, a.base.end, a.evasion, &a.pattern).cmp(&(
            b.base.start,
            b.base.end,
            b.evasion,
            &b.pattern,
        ))
    });
    hits.dedup_by(|a, b| a.base == b.base && a.tier == b.tier);
    hits
}

/// letter. The excerpt is the full chunk when short, else a window
/// around the hit.
fn mask_hit(chunk: &Chunk, n: &NormalizedText, hit: &WordHit) -> Finding {
    let orig_chars: Vec<char> = chunk.text.chars().collect();
    let orig_range = base_to_orig(n, &hit.base);
    let excerpt = window(&chunk.text, &orig_chars, orig_range.clone());
    Finding {
        path: chunk.path.clone(),
        line: chunk.line,
        excerpt: mask_excerpt(&excerpt.text, excerpt.range),
        category: "profanity",
        signal: "wordlist",
    }
}

struct Window {
    text: String,
    range: std::ops::Range<usize>,
}

fn window(text: &str, chars: &[char], orig: std::ops::Range<usize>) -> Window {
    const MAX_EXCERPT: usize = 200;
    const CONTEXT: usize = 60;
    if chars.len() <= MAX_EXCERPT {
        return Window {
            text: text.to_string(),
            range: orig,
        };
    }
    let start = orig.start.saturating_sub(CONTEXT);
    let end = (orig.end + CONTEXT).min(chars.len());
    let slice: String = chars[start..end].iter().collect();
    let prefix = if start > 0 { "…" } else { "" };
    Window {
        text: format!("{prefix}{slice}…"),
        range: orig.start - start + prefix.len()..orig.end - start + prefix.len(),
    }
}

/// Map a base-text span back to original-excerpt char offsets.
fn base_to_orig(n: &NormalizedText, base: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    let start = n.base_orig.get(base.start).copied().unwrap_or(0);
    let end = base
        .end
        .checked_sub(1)
        .and_then(|i| n.base_orig.get(i))
        .map(|e| e + 1)
        .unwrap_or(start);
    start..end.max(start)
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
    // Small deploys: one query per chunk keeps the SQL static; chunk
    // counts are bounded by the text cap.
    for hash in hashes {
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT verdict FROM moderation_text WHERE app_id = ?1 AND text_hash = ?2",
        )
        .bind(app_id)
        .bind(hash)
        .fetch_optional(pool)
        .await?;
        if let Some((verdict,)) = row {
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
            "INSERT INTO moderation_text (app_id, text_hash, verdict, checked_at) VALUES (?1, ?2, ?3, ?4)
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
    use super::*;

    fn chunk(path: &str, line: u32, text: &str) -> Chunk {
        Chunk {
            path: path.to_string(),
            line,
            text: text.to_string(),
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

    #[test]
    fn decide_table_matches_spec() {
        // Jev unavailable: strong blocks, ambiguous blocks (fail closed).
        assert!(matches!(decide(1, 0, None), Decision::Block { .. }));
        assert!(matches!(decide(0, 1, None), Decision::Block { .. }));
        assert!(matches!(decide(0, 0, None), Decision::Pass));
        // Jev available: strong_profanity >= 0.5 blocks even alone.
        let jev = |sp: f32| {
            Some(JevScores {
                strong_profanity: sp,
                sexual_minors: 0.0,
                phishing: 0.0,
                hate: 0.0,
                severity: 0.0,
            })
        };
        assert!(matches!(decide(0, 0, jev(0.7)), Decision::Block { .. }));
        // Ambiguous cleared by Jev passes.
        assert!(matches!(decide(0, 1, jev(0.1)), Decision::Pass));
        // Severe signals block with queue.
        let severe = JevScores {
            strong_profanity: 0.1,
            sexual_minors: 0.4,
            phishing: 0.0,
            hate: 0.0,
            severity: 1.0,
        };
        assert!(matches!(
            decide(0, 0, Some(severe)),
            Decision::Block { queue: true, .. }
        ));
    }

    #[tokio::test]
    async fn plain_strong_word_blocks_with_path_and_line() {
        let (_d, pool) = test_pool().await;
        let out = run_gate(
            &pool,
            "a",
            "d1",
            "sha",
            vec![chunk("pages/index.tsx", 42, "this list is fuck awesome")],
            None,
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
        assert_eq!(findings[0].path, "pages/index.tsx");
        assert_eq!(findings[0].line, 42);
        assert_eq!(findings[0].excerpt, "this list is f*** awesome");
        assert_eq!(findings[0].category, "profanity");
        assert_eq!(findings[0].signal, "wordlist");
        assert!(user_message.contains("pages/index.tsx, line 42"));
    }

    #[tokio::test]
    async fn leetspeak_portuguese_and_sql_seed_block() {
        let (_d, pool) = test_pool().await;
        for (path, text) in [
            ("pages/a.tsx", "you are sh1t"),
            ("pages/b.tsx", "que porra é essa"),
            ("migrations/0002.sql", "INSERT INTO t VALUES ('merda');"),
        ] {
            let out = run_gate(&pool, "a", "d1", "s", vec![chunk(path, 7, text)], None)
                .await
                .unwrap();
            assert!(
                matches!(out, GateOutcome::Block { .. }),
                "{path} must block"
            );
        }
    }

    #[tokio::test]
    async fn evasions_are_caught_and_masked() {
        let (_d, pool) = test_pool().await;
        for evil in ["f.u.c.k you", "oh f-uck", "fu\u{200b}ck", "fück yeah"] {
            let out = run_gate(&pool, "a", "d1", "s", vec![chunk("a.tsx", 1, evil)], None)
                .await
                .unwrap();
            let GateOutcome::Block { findings, .. } = out else {
                panic!("{evil:?} must block");
            };
            assert!(
                findings[0].excerpt.contains("f***"),
                "{evil:?} masked as {:?}",
                findings[0].excerpt
            );
        }
    }

    #[tokio::test]
    async fn mild_and_scunthorpe_pass() {
        let (_d, pool) = test_pool().await;
        for ok in [
            "Damn, this is crap",
            "a cocktail at dusk",
            "welcome to Scunthorpe",
        ] {
            let out = run_gate(&pool, "a", "d1", "s", vec![chunk("a.tsx", 3, ok)], None)
                .await
                .unwrap();
            assert!(matches!(out, GateOutcome::Pass { .. }), "{ok:?} passes");
        }
    }

    #[tokio::test]
    async fn ambiguous_blocks_while_jev_is_unavailable() {
        // Departure from the M1 bullet ("the cock crowed at dawn deploys"):
        // fail closed per the §15.1 decision table. Passes once Jev is wired.
        let (_d, pool) = test_pool().await;
        let out = run_gate(
            &pool,
            "a",
            "d1",
            "s",
            vec![chunk("a.tsx", 1, "the cock crowed at dawn")],
            None,
        )
        .await
        .unwrap();
        assert!(matches!(out, GateOutcome::Block { .. }));
    }

    #[tokio::test]
    async fn cache_skips_cleared_text_but_rechecks_blocks() {
        let (_dir, pool) = test_pool().await;
        let chunks = || vec![chunk("a.tsx", 1, "Damn, this is crap")];
        let first = run_gate(&pool, "a", "d1", "s", chunks(), None)
            .await
            .unwrap();
        let GateOutcome::Pass { stats } = first else {
            panic!("mild passes");
        };
        assert!(stats.checked > 0);
        let second = run_gate(&pool, "a", "d2", "s", chunks(), None)
            .await
            .unwrap();
        let GateOutcome::Pass { stats } = second else {
            panic!("mild passes again");
        };
        assert_eq!(stats.checked, 0, "cached clear chunks are skipped");
        assert!(stats.skipped > 0);

        // Blocked text is never recorded: the next deploy re-examines it.
        let evil = || vec![chunk("a.tsx", 9, "oh fuck")];
        assert!(matches!(
            run_gate(&pool, "a", "d3", "s", evil(), None).await.unwrap(),
            GateOutcome::Block { .. }
        ));
        let again = run_gate(&pool, "a", "d4", "s", evil(), None).await.unwrap();
        let GateOutcome::Block { stats, .. } = again else {
            panic!("still blocked");
        };
        assert!(stats.checked > 0, "blocked text is re-checked");
    }
}
