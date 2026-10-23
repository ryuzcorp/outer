//! `outer`: the Outer platform binary.
//!
//! Boot order: config → store → snapshot restore (only when the local DB is
//! missing) → open and migrate the DB → snapshot sync → HTTP. Shutdown drains
//! HTTP, then writes a final snapshot.

use std::process::ExitCode;

use outer::config::{Config, StoreKind};
use outer::store::{S3Config, Store};
use outer::{db, http, store};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: outer [serve | check-config | version]";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("serve") {
        "serve" => {
            let Some(config) = load_config() else {
                return ExitCode::from(2);
            };
            init_tracing(&config.log_filter);
            match serve(config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    tracing::error!("outer stopped: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        "check-config" => match load_config() {
            Some(_) => {
                println!("configuration ok");
                ExitCode::SUCCESS
            }
            None => ExitCode::from(2),
        },
        "version" | "--version" | "-V" => {
            println!("outer {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command {other:?}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn init_tracing(filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .init();
}

/// Read and validate the config, printing every problem when it's invalid.
fn load_config() -> Option<Config> {
    match Config::from_env() {
        Ok(config) => Some(config),
        Err(problems) => {
            eprintln!("outer: invalid configuration:");
            for problem in problems {
                eprintln!("  - {problem}");
            }
            None
        }
    }
}

fn build_store(config: &Config) -> anyhow::Result<Store> {
    match &config.store {
        StoreKind::Memory => {
            tracing::warn!(
                "OUTER_STORE=memory: bucket objects (git bundles, snapshots) vanish on restart; only the SQLite file in OUTER_DATA persists (local development only)"
            );
            Ok(Store::memory())
        }
        StoreKind::R2(r2) => Store::s3(S3Config {
            endpoint: r2.endpoint.clone(),
            region: "auto".to_string(),
            bucket: r2.bucket.clone(),
            access_key_id: r2.access_key_id.clone(),
            secret_access_key: r2.secret_access_key.clone(),
            session_token: None,
        }),
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let store = build_store(&config)?;

    let snapshot_cfg = store::snapshot::SnapshotConfig {
        db_path: config.db_path(),
        key_prefix: "outer/state".to_string(),
        host_id: config.host_id.clone(),
    };
    if store::snapshot::restore_if_missing(&store, &snapshot_cfg).await? {
        tracing::info!("restored platform state from the bucket snapshot");
    }
    let pool = db::open(&config.db_path()).await?;
    // Bind before claiming the bucket: a second instance that can't listen
    // must not take ownership away from the one that's serving.
    let listener = tokio::net::TcpListener::bind(config.http_listen).await?;
    let sync = store::snapshot::StateSync::new(config.host_id.clone());
    sync.claim(&store, &snapshot_cfg).await?;
    store::snapshot::spawn_sync_task(
        sync.clone(),
        pool.clone(),
        store.clone(),
        snapshot_cfg.clone(),
    );

    let app = http::router(http::AppState {
        pool: pool.clone(),
        sync: sync.clone(),
    });
    tracing::info!(
        "outer {} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        config.http_listen
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutting down: writing the final state snapshot");
    store::snapshot::final_snapshot(&sync, &pool, &store, &snapshot_cfg).await;
    pool.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!("cannot listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
