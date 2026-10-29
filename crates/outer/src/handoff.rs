//! Zero-downtime release handoff (loopback flag file + deploy drain).
//!
//! Releases hand a live `outer` to a newcomer with no 443 blackout and no
//! killed in-flight deploys: the newcomer binds the same port with
//! `SO_REUSEPORT` while the old container is still serving, then the old
//! container drains and exits. The two processes share one SQLite file and
//! one bucket claim, so the handoff needs exactly-one-writer discipline:
//!
//! - [`is_active`]: true while `release.sh` holds the handoff open. A fresh
//!   boot under an active handoff skips [`fail_interrupted`] bookkeeping
//!   (the old process still owns its in-flight deploy rows), refuses new
//!   deploys with a retryable error, and delays the background writers
//!   (cron, backups, sweeps) until the flag clears. The snapshot sync and
//!   the bucket-owner fence keep running: the newcomer claims at boot and
//!   the old process stands down on its next tick.
//! - [`in_flight`]: deploy rows no terminal state has claimed. The old
//!   process waits for zero before exiting on SIGTERM, so no agent deploy

//!   dies mid-release.
//!
//! The flag file holds the heartbeat epoch the release script refreshes
//! while the handoff is open. A file older than [`FRESH_SECS`] reads as
//! inactive (and is removed best-effort): a crashed release heals itself
//! instead of refusing deploys forever. Recovery is `rm /data/handoff`.
//!
//! [`fail_interrupted`]: crate::deploy::fail_interrupted

use anyhow::Context as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;

/// Bind `listen` with `SO_REUSEPORT` (and `SO_REUSEADDR`): during a release
/// handoff the newcomer binds 443 while the old container is still serving,
/// and the kernel splits new connections between them. Either process
/// exiting never drops a SYN — at least one socket stays bound throughout
/// the swap — and the overlap needs no proxy component or fd passing.
pub fn bind_reuseport(listen: std::net::SocketAddr) -> anyhow::Result<std::net::TcpListener> {
    let domain = if listen.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    if listen.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    socket
        .bind(&listen.into())
        .with_context(|| format!("bind {listen}"))?;
    // Tokio (both `axum::serve` and axum-server's `from_tcp`) takes this
    // socket over and requires non-blocking mode; a blocking listener is a
    // boot error there, not a slowdown.
    socket.set_nonblocking(true)?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// The flag file name inside the data dir (`release.sh` owns it).
pub const FLAG: &str = "handoff";

/// A handoff flag older than this many seconds reads as inactive: the
/// release script refreshes the heartbeat every minute, so anything older
/// is a crashed release, not an open handoff.
pub const FRESH_SECS: i64 = 180;

/// What a refused deploy tells the caller to wait before retrying. The
/// handoff window is minutes (in-flight deploys plus the container swap),
/// so one minute backs the retry past most of it without a long stall.
pub const DRAIN_RETRY_AFTER_S: u64 = 60;

/// Refused while a release handoff is open (or this process is draining):
/// the agent committed nothing, burned nothing, and should retry shortly.
#[derive(Debug, thiserror::Error)]
#[error("outer is handing off to a new release; retry the deploy shortly")]
pub struct DrainRefused;

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FLAG)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// True while a release handoff is open: the flag file exists and its
/// heartbeat is fresh. A stale file reads as inactive and is removed
/// best-effort, so a crashed release heals itself.
pub fn is_active(data_dir: &Path) -> bool {
    let raw = match std::fs::read_to_string(path(data_dir)) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    let marked: i64 = match raw.trim().parse() {
        Ok(marked) => marked,
        Err(_) => {
            let _ = std::fs::remove_file(path(data_dir));
            return false;
        }
    };
    if now_secs() - marked < FRESH_SECS {
        return true;
    }
    let _ = std::fs::remove_file(path(data_dir));
    false
}

/// Deploy rows no terminal state has claimed (`queued`, `building`,
/// `reviewing`, `deploying`, or any future non-terminal state): the same
/// predicate [`fail_interrupted`] closes, inverted.
///
/// [`fail_interrupted`]: crate::deploy::fail_interrupted
pub async fn in_flight(pool: &SqlitePool) -> anyhow::Result<u64> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM deploy WHERE status NOT IN ('success', 'failed', 'blocked')",
    )
    .fetch_one(pool)
    .await?;
    Ok(count.max(0) as u64)
}

/// How the handoff ended: the release script removed the flag after the old
/// process exited (`Cleared`), or the heartbeat went stale and this process
/// reaped it (`Stale`, likely a crashed release with the old process still
/// alive — its in-flight deploy rows may still be live work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffEnd {
    Cleared,
    Stale,
}

/// Wait until no handoff is open, polling every five seconds. Returns at
/// once when none is (or the flag went stale): the normal boot path never
/// blocks here. A missing flag is `Cleared`; a stale one is reaped and
/// reports `Stale`.
pub async fn wait_until_clear(data_dir: &std::path::Path) -> HandoffEnd {
    loop {
        let raw = std::fs::read_to_string(path(data_dir));
        let marked: Option<i64> = raw.ok().and_then(|text| text.trim().parse().ok());
        match marked {
            None => return HandoffEnd::Cleared,
            Some(marked) if now_secs() - marked < FRESH_SECS => {}
            _ => {
                let _ = std::fs::remove_file(path(data_dir));
                return HandoffEnd::Stale;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// First-signal shutdown gate: mark this process draining (new deploys
/// refuse with [`DrainRefused`]) and wait for in-flight deploys to reach a
/// terminal row, so a release handoff — or any stop — never kills an agent
/// deploy mid-flight. `cancel` is the second signal: a wedged deploy must
/// not wedge the stop, so it exits the process at once (the next boot's
/// `fail_interrupted` closes whatever is left behind).
pub async fn drain_and_wait(
    pool: &SqlitePool,
    drain: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    cancel: impl Future<Output = ()>,
) {
    let mut cancel = Box::pin(cancel);
    if let Some(drain) = drain {
        drain.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    loop {
        match in_flight(pool).await {
            Ok(0) => return,
            Ok(n) => {
                tracing::info!(n, "draining: waiting for in-flight deploys before shutdown");
            }
            Err(err) => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    "drain poll failed; shutting down anyway"
                );
                return;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            () = &mut cancel => {
                tracing::warn!("second shutdown signal: exiting with deploys in flight");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn missing_flag_is_inactive() {
        assert!(!is_active(dir().path()), "no flag file means no handoff");
    }

    #[test]
    fn fresh_flag_is_active() {
        let dir = dir();
        std::fs::write(path(dir.path()), now_secs().to_string()).unwrap();
        assert!(is_active(dir.path()), "a fresh heartbeat means handoff");
    }

    #[test]
    fn stale_flag_heals_itself() {
        let dir = dir();
        std::fs::write(path(dir.path()), (now_secs() - FRESH_SECS - 1).to_string()).unwrap();
        assert!(
            !is_active(dir.path()),
            "a stale heartbeat is a crashed release, not a handoff"
        );
        assert!(
            !path(dir.path()).exists(),
            "the stale flag is removed so deploys resume"
        );
    }

    #[test]
    fn unparseable_flag_heals_itself() {
        let dir = dir();
        std::fs::write(path(dir.path()), "not-an-epoch").unwrap();
        assert!(!is_active(dir.path()), "garbage never blocks deploys");
        assert!(!path(dir.path()).exists(), "garbage is removed");
    }

    #[test]
    fn boundary_heartbeat_is_stale() {
        let dir = dir();
        std::fs::write(path(dir.path()), (now_secs() - FRESH_SECS).to_string()).unwrap();
        assert!(
            !is_active(dir.path()),
            "exactly FRESH_SECS old is stale, not active"
        );
    }

    #[tokio::test]
    async fn in_flight_counts_only_non_terminal_rows() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE deploy (id TEXT PRIMARY KEY, app_id TEXT, sha TEXT, status TEXT, created_at INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, status) in [
            ("d1", "queued"),
            ("d2", "building"),
            ("d3", "reviewing"),
            ("d4", "deploying"),
            ("d5", "success"),
            ("d6", "failed"),
            ("d7", "blocked"),
        ] {
            sqlx::query("INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES (?, 'app1', 'abc', ?, 0)")
                .bind(id)
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(
            in_flight(&pool).await.unwrap(),
            4,
            "only the non-terminal rows block the handoff"
        );
    }

    /// The handoff overlap: two processes bind the same port at once, and a
    /// connection to it succeeds while both are bound.
    #[test]
    fn reuseport_binds_twice_and_serves() {
        let first = bind_reuseport("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(
            socket2::SockRef::from(&first).nonblocking().unwrap(),
            "Tokio takes this socket over and requires non-blocking mode"
        );
        let port = first.local_addr().unwrap().port();
        let second = bind_reuseport(std::net::SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
        assert_eq!(
            second.local_addr().unwrap().port(),
            port,
            "the newcomer shares the port instead of failing with EADDRINUSE"
        );
        let conn = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            std::time::Duration::from_secs(5),
        );
        assert!(conn.is_ok(), "a SYN lands while both are bound");
    }

    #[tokio::test]
    async fn wait_returns_cleared_without_a_flag() {
        assert_eq!(
            wait_until_clear(dir().path()).await,
            HandoffEnd::Cleared,
            "no flag means no wait"
        );
    }

    #[tokio::test]
    async fn wait_reaps_a_stale_flag() {
        let dir = dir();
        std::fs::write(path(dir.path()), (now_secs() - FRESH_SECS - 1).to_string()).unwrap();
        assert_eq!(
            wait_until_clear(dir.path()).await,
            HandoffEnd::Stale,
            "a stale flag unblocks the sweeps but is reported"
        );
        assert!(!path(dir.path()).exists(), "the stale flag is reaped");
    }

    async fn deploy_table(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE deploy (id TEXT PRIMARY KEY, app_id TEXT, sha TEXT, status TEXT, created_at INTEGER)",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn drain_waits_for_in_flight_then_returns() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        deploy_table(&pool).await;
        sqlx::query("INSERT INTO deploy (id, app_id, sha, status, created_at) VALUES ('d1', 'app1', 'abc', 'building', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flip = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            sqlx::query("UPDATE deploy SET status = 'success' WHERE id = 'd1'")
                .execute(&flip)
                .await
                .unwrap();
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drain_and_wait(&pool, Some(flag.clone()), std::future::pending()),
        )
        .await
        .expect("the drain returns once the deploy lands");
        assert!(
            flag.load(std::sync::atomic::Ordering::SeqCst),
            "the drain marks the process so new deploys refuse"
        );
        assert_eq!(in_flight(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn drain_returns_at_once_when_idle() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        deploy_table(&pool).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drain_and_wait(&pool, None, std::future::pending()),
        )
        .await
        .expect("an idle drain never blocks");
    }
}
