//! `outer`: the Outer platform binary.
//!
//! `serve` boots config → store → snapshot restore (only when the local DB is
//! missing) → DB → bind → bucket claim → snapshot sync → fleets → HTTP, and on
//! shutdown drains HTTP, stops every fleet and writes a final snapshot.
//! The `app` and `templates` commands run app operations against the same
//! database and bucket; `serve` picks their results up (it reads app rows per
//! request, and fleets adopt new deploys on reload or their own poll).

use std::future::Future;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use outer::apps::{self, Allocation};
use outer::config::{Config, StoreKind};
use outer::deploy::{DeploySettings, Deployer};
use outer::fleet::manager::{FleetManager, FleetStorage, ManagerSettings};
use outer::git::{GitConfig, GitSync};
use outer::ops::{self, Platform};
use outer::store::{S3Config, Store};
use outer::{db, http, store, templates};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  outer serve
  outer check-config
  outer templates sync [--sha SHA] [--url URL]
  outer app create SLUG --owner EMAIL [--name NAME]
  outer app deploy SLUG
  outer app list
  outer version";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["serve"] => {
            let Some(config) = load_config() else {
                return ExitCode::from(2);
            };
            init_tracing(&config.log_filter);
            report(serve(config).await)
        }
        ["check-config"] => match load_config() {
            Some(_) => {
                println!("configuration ok");
                ExitCode::SUCCESS
            }
            None => ExitCode::from(2),
        },
        ["templates", "sync", rest @ ..] => {
            run(|config| templates_sync(config, rest.to_vec())).await
        }
        ["app", "create", slug, rest @ ..] => {
            let (slug, rest) = (slug.to_string(), rest.to_vec());
            run(move |config| app_create(config, slug, rest)).await
        }
        ["app", "deploy", slug] => {
            let slug = slug.to_string();
            run(move |config| app_deploy(config, slug)).await
        }
        ["app", "list"] => run(app_list).await,
        ["version" | "--version" | "-V"] => {
            println!("outer {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unknown command {:?}\n{USAGE}", args.join(" "));
            ExitCode::from(2)
        }
    }
}

/// Load config, set up logging, run one command, map its result to an exit code.
async fn run<F, Fut>(command: F) -> ExitCode
where
    F: FnOnce(Config) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let Some(config) = load_config() else {
        return ExitCode::from(2);
    };
    init_tracing(&config.log_filter);
    report(command(config).await)
}

fn report(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("outer: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(std::io::stderr)
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
            region: r2.region.clone(),
            bucket: r2.bucket.clone(),
            access_key_id: r2.access_key_id.clone(),
            secret_access_key: r2.secret_access_key.clone(),
            session_token: None,
        }),
    }
}

/// What fleets and `celld deploy` use to reach the bucket. celld only speaks
/// S3, so apps can't run on the memory store.
///
/// Fleets hold the root bucket keys until R2 temporary credentials scoped to
/// `fleets/{slug}/` land (M0 infra); that's refused off localhost.
fn fleet_storage(config: &Config) -> anyhow::Result<FleetStorage> {
    let StoreKind::R2(r2) = &config.store else {
        anyhow::bail!(
            "apps need an S3-compatible bucket (celld has no in-memory store); set OUTER_STORE=r2"
        );
    };
    anyhow::ensure!(
        config.is_local(),
        "fleets would hold the root bucket keys; scoped R2 credentials are required off localhost and not built yet"
    );
    Ok(FleetStorage {
        endpoint: r2.endpoint.clone(),
        region: r2.region.clone(),
        bucket: r2.bucket.clone(),
        access_key_id: r2.access_key_id.clone(),
        secret_access_key: r2.secret_access_key.clone(),
        session_token: None,
    })
}

/// Per-fleet cgroups when this process may manage them (root); dev runs
/// without containment.
fn cgroups() -> Option<std::path::PathBuf> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        tracing::info!(
            "not root: fleets and builds run as this user without cgroup containment (development)"
        );
        return None;
    }
    outer::fleet::cgroups::setup(&outer::fleet::cgroups::CgroupConfig::default())
}

fn git_config(config: &Config) -> GitConfig {
    GitConfig {
        repos_dir: config.data_dir.join("git"),
    }
}

/// The app-operation context for CLI commands: the same DB and bucket
/// `serve` uses. Commands never restore or claim the snapshot; that's
/// `serve`'s job.
async fn platform(config: &Config) -> anyhow::Result<Platform> {
    std::fs::create_dir_all(&config.data_dir)?;
    let store = build_store(config)?;
    let pool = db::open(&config.db_path()).await?;
    let rt = &config.runtime;
    let deployer = Deployer::new(
        DeploySettings {
            data_dir: config.data_dir.clone(),
            celld_bin: rt.celld_bin.clone(),
            esbuild_bin: rt.esbuild_bin.clone(),
            bun_bin: rt.bun_bin.clone(),
            storage: fleet_storage(config)?,
            step_timeout: Duration::from_secs(120),
            cgroups: cgroups(),
        },
        rt.build_concurrency,
    );
    Ok(Platform {
        pool,
        store,
        git: git_config(config),
        sync: GitSync::default(),
        deployer,
        alloc: Allocation {
            uid_range: rt.app_uid_range,
            port_range: rt.fleet_ports,
        },
        domain: config.domain.clone(),
    })
}

/// `--flag value` pairs from the words after a subcommand.
fn flags<'a>(rest: &[&'a str], allowed: &[&str]) -> anyhow::Result<Vec<(&'a str, &'a str)>> {
    let mut out = Vec::new();
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        anyhow::ensure!(allowed.contains(word), "unknown option {word:?}\n{USAGE}");
        let Some(value) = words.next() else {
            anyhow::bail!("{word} needs a value");
        };
        out.push((*word, *value));
    }
    Ok(out)
}

fn flag<'a>(flags: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    flags.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

async fn templates_sync(config: Config, rest: Vec<&str>) -> anyhow::Result<()> {
    let flags = flags(&rest, &["--sha", "--url"])?;
    let store = build_store(&config)?;
    let url = flag(&flags, "--url").unwrap_or(templates::ILHA_VOID_URL);
    let sha = flag(&flags, "--sha").unwrap_or(templates::ILHA_VOID_SHA);
    let template = templates::sync(&store, url, sha).await?;
    println!("{}", serde_json::to_string_pretty(&template)?);
    Ok(())
}

async fn app_create(config: Config, slug: String, rest: Vec<&str>) -> anyhow::Result<()> {
    let flags = flags(&rest, &["--owner", "--name"])?;
    let Some(owner) = flag(&flags, "--owner") else {
        anyhow::bail!("app create needs --owner EMAIL");
    };
    let p = platform(&config).await?;
    let (app, outcome) = ops::create_app(&p, owner, &slug, flag(&flags, "--name")).await?;
    let reloaded = outcome.status == "success" && ops::reload_if_awake(&app).await;
    print_outcome(&config, &app, &outcome, reloaded)?;
    anyhow::ensure!(outcome.status == "success", "deploy failed");
    Ok(())
}

async fn app_deploy(config: Config, slug: String) -> anyhow::Result<()> {
    let p = platform(&config).await?;
    let outcome = ops::deploy_app(&p, &slug).await?;
    let app = apps::get_by_slug(&p.pool, &slug)
        .await?
        .expect("deployed app exists");
    let reloaded = outcome.status == "success" && ops::reload_if_awake(&app).await;
    print_outcome(&config, &app, &outcome, reloaded)?;
    anyhow::ensure!(outcome.status == "success", "deploy failed");
    Ok(())
}

fn print_outcome(
    config: &Config,
    app: &apps::App,
    outcome: &outer::deploy::DeployOutcome,
    reloaded: bool,
) -> anyhow::Result<()> {
    let url = format!(
        "http://{}.{}{}",
        app.slug,
        config.runtime.apps_domain,
        port_suffix(config.http_listen)
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "app": app.slug,
            "url": url,
            "reloaded": reloaded,
            "deploy": outcome,
        }))?
    );
    Ok(())
}

/// `:8080` unless the listener is on the default HTTP port.
fn port_suffix(listen: SocketAddr) -> String {
    match listen.port() {
        80 => String::new(),
        port => format!(":{port}"),
    }
}

async fn app_list(config: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let pool = db::open(&config.db_path()).await?;
    for app in apps::list(&pool).await? {
        let state = if app.asleep_since.is_some() {
            "asleep"
        } else {
            "awake-or-unknown"
        };
        println!(
            "{}\t{}\tport {}\t{}",
            app.slug,
            app.last_deploy_sha.as_deref().unwrap_or("-"),
            app.port,
            state
        );
    }
    Ok(())
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

    let rt = &config.runtime;
    let storage = match fleet_storage(&config) {
        Ok(storage) => storage,
        Err(err) => {
            tracing::warn!("apps can't run: {err:#}");
            // A manager whose fleets can't start: every app answers 503
            // with the reason in the log, and the platform host still serves.
            FleetStorage {
                endpoint: String::new(),
                region: String::new(),
                bucket: String::new(),
                access_key_id: String::new(),
                secret_access_key: String::new(),
                session_token: None,
            }
        }
    };
    let fleets = FleetManager::new(ManagerSettings {
        data_dir: config.data_dir.clone(),
        celld_bin: rt.celld_bin.clone(),
        storage,
        cgroups: cgroups(),
        memory_max_bytes: rt.fleet_memory_mb * 1024 * 1024,
        cpu_max: rt.fleet_cpu_max.clone(),
        sleep_after: Duration::from_secs(rt.sleep_after_secs),
        wake_timeout: Duration::from_secs(30),
        wake_concurrency: rt.wake_concurrency,
    });
    fleets.spawn_sleep_sweep(
        pool.clone(),
        Duration::from_secs(60.min(rt.sleep_after_secs.max(1))),
    );

    let app = http::app(
        http::AppState {
            pool: pool.clone(),
            sync: sync.clone(),
        },
        http::proxy::ProxyState::new(pool.clone(), fleets.clone(), rt.apps_domain.clone()),
        config.domain.clone(),
    );
    tracing::info!(
        "outer {} listening on http://{} (platform {}, apps *.{})",
        env!("CARGO_PKG_VERSION"),
        config.http_listen,
        config.domain,
        rt.apps_domain
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("shutting down: stopping fleets, then writing the final state snapshot");
    fleets.stop_all().await;
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
