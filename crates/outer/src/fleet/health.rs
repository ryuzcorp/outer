// Derived from noite apps/runner/src/host/sleep.rs @ 58a977a (Apache-2.0)
// (`wait_healthy`).
// Outer change: standalone function over an explicit host/port, no config or
// Caddy plumbing.

use std::time::Duration;

/// Poll the fleet's health route until celld's ready gate opens: 200 on
/// `/.well-known/celld/health`, every 250 ms, each attempt capped at 2 s.
/// Returns false once `timeout` runs out.
pub async fn wait_healthy(host: &str, port: u16, timeout: Duration) -> bool {
    static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default()
    });
    let url = format!("http://{host}:{port}/.well-known/celld/health");
    let deadline = tokio::time::Instant::now() + timeout;
    let started = tokio::time::Instant::now();
    loop {
        let attempt = tokio::time::Instant::now();
        match CLIENT.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return true,
            Ok(resp) => tracing::debug!(
                port,
                status = resp.status().as_u16(),
                attempt_ms = attempt.elapsed().as_millis() as u64,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "fleet not healthy yet"
            ),
            Err(err) => tracing::debug!(
                port,
                error = %err,
                attempt_ms = attempt.elapsed().as_millis() as u64,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "fleet health probe failed"
            ),
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal celld health gate: 503 for the first `flaps` requests, then
    /// 200 on `/.well-known/celld/health`.
    async fn serve_health(listener: tokio::net::TcpListener, flaps: usize) -> u16 {
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut request = Vec::new();
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                request.extend_from_slice(&buf[..n]);
                                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let hit = seen.fetch_add(1, Ordering::SeqCst);
                    let status = if hit >= flaps {
                        "200 OK"
                    } else {
                        "503 Starting"
                    };
                    let body = if hit >= flaps { "ok" } else { "starting" };
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn healthy_after_flaps_then_200() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = serve_health(listener, 2).await;
        assert!(wait_healthy("127.0.0.1", port, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn never_healthy_hits_the_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // Always 503: more flaps than any poll budget can outlast.
        let port = serve_health(listener, usize::MAX).await;
        let start = tokio::time::Instant::now();
        assert!(!wait_healthy("127.0.0.1", port, Duration::from_millis(600)).await);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must stop at the deadline"
        );
    }

    #[tokio::test]
    async fn refused_port_is_not_healthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!wait_healthy("127.0.0.1", port, Duration::from_millis(600)).await);
    }
}
