//! Whether an app may serve visitors: one rule, one place.
//!
//! [`decide`] is the pure function the proxy applies to a loaded [`App`];
//! [`SERVING_SQL`] is the same rule as a SQL predicate over the `app` table
//! aliased `a`, used by the sweeps that must skip apps that cannot serve.
//! The two encodings must agree: a row matches `SERVING_SQL` exactly when
//! `decide` answers [`Serving::Live`], and the invariant test below pins
//! them together.

use crate::apps::{App, ModerationState};

/// The `app.desired_state` column value: whether the app is meant to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    /// The `app.desired_state` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            DesiredState::Running => "running",
            DesiredState::Stopped => "stopped",
        }
    }

    /// Parse the `app.desired_state` column value.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "running" => Ok(DesiredState::Running),
            "stopped" => Ok(DesiredState::Stopped),
            other => anyhow::bail!("unknown desired_state {other:?}"),
        }
    }
}

/// Why an app is [`DesiredState::Stopped`]. Read off the row: a downgrade
/// stamps `app.downgraded_stopped_at`, a ban (or any other operator stop)
/// does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// A plan downgrade past the Free app cap. Re-upgrading restarts it.
    Plan,
    /// An account ban or another operator stop. Re-upgrading leaves it
    /// stopped.
    Operator,
}

/// What a visitor gets from the app's host, first match wins (see [`decide`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serving {
    /// The app may serve. Build readiness and the access gate are the
    /// proxy's remaining gates.
    Live,
    /// Inactivity past its deadline: the expired page until restore or purge.
    Expired,
    /// Moderation suspended: the 451 page.
    Suspended,
    /// Moderation held: the under-review page.
    Held,
    /// The owner's plan or an operator stopped the app: the stopped page,
    /// and the fleet never wakes.
    Stopped(StopCause),
    /// Quota pause, carrying its reason.
    Paused(String),
}

/// The serving rule: expired beats suspended beats held beats stopped beats
/// paused; anything left serves. Build readiness (`last_deploy_sha`, preview
/// deploys) stays in the proxy: it depends on the host kind, not the row.
pub fn decide(app: &App) -> Serving {
    if app.expired_at.is_some() {
        return Serving::Expired;
    }
    match app.moderation_state {
        ModerationState::Suspended => return Serving::Suspended,
        ModerationState::Held => return Serving::Held,
        ModerationState::Ok => {}
    }
    if app.desired_state == DesiredState::Stopped {
        let cause = match app.downgraded_stopped_at {
            Some(_) => StopCause::Plan,
            None => StopCause::Operator,
        };
        return Serving::Stopped(cause);
    }
    if let Some(reason) = app.paused_reason.as_deref() {
        return Serving::Paused(reason.to_string());
    }
    Serving::Live
}

/// The serving predicate as one string literal, so the sweeps can splice it
/// into a `concat!` query and keep sqlx's compile-time check. Not a `const`:
/// `concat!` needs the literal, not its value.
macro_rules! serving_sql {
    () => {
        "a.desired_state = 'running' AND a.moderation_state = 'ok' AND a.paused_reason IS NULL AND a.expired_at IS NULL"
    };
}
pub(crate) use serving_sql;

/// [`decide`] compared to [`Serving::Live`] as a SQL predicate over the
/// `app` table aliased `a`. The sweeps that skip apps which cannot serve
/// (cron due, D1 backup, after-live crawl, keep-warm) use this instead of
/// spelling the four fields again, so the rule has one home. Must agree
/// with `decide(app) == Serving::Live`; the invariant test pins the two
/// together.
pub const SERVING_SQL: &str = serving_sql!();

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::{ModerationState, Visibility};

    fn live_app() -> App {
        App {
            id: "app1".to_string(),
            slug: "shop".to_string(),
            owner_id: "a1".to_string(),
            name: None,
            uid: 10001,
            port: 18001,
            internal_port: 18002,
            visibility: Visibility::Public,
            moderation_state: ModerationState::Ok,
            desired_state: DesiredState::Running,
            paused_reason: None,
            asleep_since: None,
            last_request_at: None,
            last_deploy_sha: Some("a".repeat(40)),
            last_activity_at: 0,
            expired_at: None,
            downgraded_stopped_at: None,
            created_at: 0,
            always_on: false,
            access_mode: crate::access::AccessMode::Open,
            access_hash: None,
            access_version: 0,
            preview_sha: None,
            preview_updated_at: None,
            preview_port: None,
            preview_internal_port: None,
        }
    }

    /// Each gate added on top of the previous one loses to it: the order is
    /// expired → suspended → held → stopped → paused → live.
    #[test]
    fn decide_precedence_is_expired_suspended_held_stopped_paused() {
        let mut app = live_app();
        assert_eq!(decide(&app), Serving::Live);

        app.paused_reason = Some("over quota".to_string());
        assert_eq!(decide(&app), Serving::Paused("over quota".to_string()));

        app.desired_state = DesiredState::Stopped;
        app.downgraded_stopped_at = Some(1);
        assert_eq!(decide(&app), Serving::Stopped(StopCause::Plan));

        app.moderation_state = ModerationState::Held;
        assert_eq!(decide(&app), Serving::Held);

        app.moderation_state = ModerationState::Suspended;
        assert_eq!(decide(&app), Serving::Suspended);

        app.expired_at = Some(1);
        assert_eq!(decide(&app), Serving::Expired);
    }

    #[test]
    fn stop_cause_reads_the_downgrade_stamp() {
        let mut app = live_app();
        app.desired_state = DesiredState::Stopped;
        assert_eq!(decide(&app), Serving::Stopped(StopCause::Operator));
        app.downgraded_stopped_at = Some(1);
        assert_eq!(decide(&app), Serving::Stopped(StopCause::Plan));
    }

    /// The pure rule and the SQL predicate are two encodings of one rule:
    /// every combination of the four fields must agree.
    #[tokio::test]
    async fn serving_sql_matches_decide_for_every_combination() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("serving.sqlite"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user (id, email, lastActiveAt, createdAt, updatedAt) \
             VALUES ('a1', 'a@example.com', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut n = 0i64;
        for expired in [false, true] {
            for moderation in ["ok", "held", "suspended"] {
                for desired in ["running", "stopped"] {
                    for paused in [false, true] {
                        let id = format!("app{n}");
                        sqlx::query(
                            "INSERT INTO app (id, slug, owner_id, uid, port, internal_port, \
                             desired_state, moderation_state, paused_reason, expired_at, \
                             last_activity_at, created_at) \
                             VALUES (?1, ?1, 'a1', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0)",
                        )
                        .bind(&id)
                        .bind(300000 + n)
                        .bind(21000 + n * 2)
                        .bind(21001 + n * 2)
                        .bind(desired)
                        .bind(moderation)
                        .bind(paused.then_some("over quota"))
                        .bind(expired.then_some(1000))
                        .execute(&pool)
                        .await
                        .unwrap();
                        n += 1;
                    }
                }
            }
        }
        for i in 0..n {
            let id = format!("app{i}");
            let app = crate::apps::get_by_id(&pool, &id).await.unwrap().unwrap();
            let matches: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM app AS a WHERE a.id = ?1 AND {SERVING_SQL}"
            )))
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
            let verdict = decide(&app);
            assert_eq!(
                matches == 1,
                verdict == Serving::Live,
                "row {id} ({verdict:?}) disagrees with SERVING_SQL"
            );
        }
    }
}
