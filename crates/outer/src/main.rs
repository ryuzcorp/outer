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
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use outer::apps::{self, Allocation};
use outer::config::{Config, StoreKind};
use outer::deploy::{DeploySettings, Deployer};
use outer::fleet::manager::{FleetManager, FleetStorage, ManagerSettings};
use outer::git::{GitConfig, GitSync};
use outer::ops::{self, Platform};
use outer::store::credentials::{r2_jurisdiction, CredentialSource, R2TempConfig, R2TempMinter};
use outer::store::{S3Config, Store};
use outer::{db, http, store, templates};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  outer serve
  outer check-config
  outer templates sync [--sha SHA] [--url URL]
  outer app create SLUG --owner EMAIL [--name NAME]
  outer app deploy SLUG
  outer app delete SLUG
  outer app list
  outer docs tools
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
            if let Err(err) = prepare_host(&config) {
                return report(Err(err));
            }
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
        ["app", "delete", slug] => {
            let slug = slug.to_string();
            run(move |config| app_delete(config, slug)).await
        }
        ["app", "list"] => run(app_list).await,
        ["docs", "tools"] => {
            // Generated site content: the registry rendered as Markdown.
            print!(
                "{}",
                outer::mcp::tools_page::render(&outer::mcp::registry::registry())
            );
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
    if let Err(err) = prepare_host(&config) {
        return report(Err(err));
    }
    report(command(config).await)
}

/// Root-only host preparation shared by serve and every command: the data
/// dir layout app uids may traverse, and a private umask. An unprivileged
/// (dev) outer runs every step as itself, so there is nothing to protect.
fn prepare_host(config: &Config) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    outer::fleet::sandbox::prepare_data_dir(&config.data_dir)
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
/// With the Cloudflare pair set, fleets and deploys mint scoped R2 temporary
/// credentials per spawn/deploy and never hold the parent keys; on localhost
/// dev without the pair they use the parent keys directly.
fn fleet_storage(config: &Config) -> anyhow::Result<FleetStorage> {
    let StoreKind::R2(r2) = &config.store else {
        anyhow::bail!(
            "apps need an S3-compatible bucket (celld has no in-memory store); set OUTER_STORE=r2"
        );
    };
    let credentials = match &config.cloudflare {
        Some(cf) => CredentialSource::R2Temp(Arc::new(R2TempMinter::new(R2TempConfig {
            api_base: "https://api.cloudflare.com/client/v4".to_string(),
            account_id: cf.account_id.clone(),
            api_token: cf.api_token.clone(),
            bucket: r2.bucket.clone(),
            jurisdiction: r2_jurisdiction(&r2.endpoint),
            parent_access_key_id: r2.access_key_id.clone(),
            ttl: cf.credential_ttl,
            refresh_after: cf.credential_refresh_after,
        })?)),
        None => {
            anyhow::ensure!(
                config.is_local(),
                "fleets would hold the root bucket keys; set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN to mint scoped R2 credentials off localhost"
            );
            CredentialSource::Static {
                access_key_id: r2.access_key_id.clone(),
                secret_access_key: r2.secret_access_key.clone(),
            }
        }
    };
    Ok(FleetStorage {
        endpoint: r2.endpoint.clone(),
        region: r2.region.clone(),
        bucket: r2.bucket.clone(),
        credentials,
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
    GitConfig::new(config.data_dir.join("git"), Some(config.oxfmt_bin.clone()))
}

/// The app-operation context for CLI commands: the same DB and bucket
/// `serve` uses. Commands never restore or claim the snapshot; that's
/// `serve`'s job.
/// The pre-live gate classifies through Jev on AI Gateway with the same
/// Cloudflare pair that mints R2 credentials. Without the pair (localhost dev
/// only; config refuses it elsewhere) the gate logs a warning and passes.
fn jev_client(config: &Config) -> anyhow::Result<Option<outer::moderation::JevClient>> {
    let Some(cf) = &config.cloudflare else {
        return Ok(None);
    };
    Ok(Some(outer::moderation::JevClient::new(
        outer::moderation::JevConfig::new(
            cf.account_id.clone(),
            cf.api_token.clone(),
            config.ai_gateway.clone(),
        ),
    )?))
}

/// What deploys, rollbacks and deletes purge from the edge cache: the zone
/// plus the API token. `None` skips the purge at debug (localhost dev
/// without `CLOUDFLARE_ZONE_ID`).
fn cache_purge(config: &Config) -> Option<outer::cloudflare::CachePurge> {
    config.cloudflare.as_ref().and_then(|cf| {
        cf.zone_id
            .clone()
            .map(|zone_id| outer::cloudflare::CachePurge::new(zone_id, cf.api_token.clone()))
    })
}

/// What `look_at_app` renders through: the account plus the API token
/// (needs `Browser Rendering Write`). `None` skips the tool at debug
/// (localhost dev without the Cloudflare pair).
fn browser_render(config: &Config) -> Option<outer::browser::BrowserRender> {
    config
        .cloudflare
        .as_ref()
        .map(|cf| outer::browser::BrowserRender::new(cf.account_id.clone(), cf.api_token.clone()))
}

async fn platform(config: &Config) -> anyhow::Result<Platform> {
    std::fs::create_dir_all(&config.data_dir)?;
    let store = build_store(config)?;
    let pool = db::open(&config.db_path()).await?;
    let rt = &config.runtime;
    let jev = jev_client(config)?;
    let secrets_key = outer::secrets::SecretsKey::derive(&config.outer_secret);
    let deployer = Deployer::new(
        DeploySettings {
            data_dir: config.data_dir.clone(),
            celld_bin: rt.celld_bin.clone(),
            esbuild_bin: rt.esbuild_bin.clone(),
            bun_bin: rt.bun_bin.clone(),
            storage: fleet_storage(config)?,
            step_timeout: Duration::from_secs(120),
            cgroups: cgroups(),
            apps_domain: rt.apps_domain.clone(),
            cache_purge: cache_purge(config),
            jev,
            // The operator CLI runs no fleet manager: a migrate step that
            // finds pending files fails loudly when no fleet serves the app —
            // before any upload for an existing app, after the upload for a
            // first deploy (which uploads first).
            wake: None,
            secrets_key: secrets_key.clone(),
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
        data_dir: config.data_dir.clone(),
        cgroups: cgroups(),
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
    p.deployer.purges().drain().await;
    print_outcome(&config, &app, &outcome, outcome.reloaded)?;
    end_outcome(&outcome)?;
    Ok(())
}

async fn app_deploy(config: Config, slug: String) -> anyhow::Result<()> {
    let p = platform(&config).await?;
    let outcome = ops::deploy_app(&p, &slug).await?;
    let app = apps::get_by_slug(&p.pool, &slug)
        .await?
        .expect("deployed app exists");
    p.deployer.purges().drain().await;
    print_outcome(&config, &app, &outcome, outcome.reloaded)?;
    end_outcome(&outcome)?;
    Ok(())
}

/// Delete an app: the same core the MCP tool runs (the slug itself is the
/// confirmation). Prints the outcome as JSON.
async fn app_delete(config: Config, slug: String) -> anyhow::Result<()> {
    let p = platform(&config).await?;
    let app = apps::get_by_slug(&p.pool, &slug)
        .await?
        .with_context(|| format!("no app {slug:?}"))?;
    let deploy = p.deployer.app_lock(&app.id).lock_owned().await;
    let locks = outer::apps::delete::DeleteLocks {
        deploy: &deploy,
        rollback: None,
        wake: None,
    };
    let cfg = outer::apps::delete::DeleteConfig {
        data_dir: config.data_dir.clone(),
        cgroups: cgroups(),
        apps_domain: config.runtime.apps_domain.clone(),
        cache_purge: cache_purge(&config),
    };
    let out = ops::delete_app(&p, &cfg, &slug, &slug, locks).await?;
    println!(
        "{}",
        serde_json::json!({
            "deleted": true,
            "slug": out.slug,
            "quarantined_until": out.quarantined_until,
        })
    );
    Ok(())
}

/// The CLI prints the whole outcome as JSON first; the exit status then
/// distinguishes a gate block (nothing reached the fleet) from a failure.
fn end_outcome(outcome: &outer::deploy::DeployOutcome) -> anyhow::Result<()> {
    match outcome.status {
        "success" => Ok(()),
        "blocked" => anyhow::bail!("deploy blocked"),
        _ => anyhow::bail!("deploy failed"),
    }
}

fn print_outcome(
    config: &Config,
    app: &apps::App,
    outcome: &outer::deploy::DeployOutcome,
    reloaded: bool,
) -> anyhow::Result<()> {
    let url = format!(
        "{}://{}.{}{}",
        scheme(config),
        app.slug,
        config.runtime.apps_domain,
        port_suffix(config.listen, config.tls_enabled())
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

/// Build the auth module's state from config: issuer from scheme + domain +
/// port, Cloudflare Email Service when the Cloudflare pair is set, log-only
/// codes on localhost without it.
fn auth_state(config: &Config, pool: &sqlx::SqlitePool) -> anyhow::Result<outer::auth::AuthState> {
    let issuer = format!(
        "{}://{}{}",
        scheme(config),
        config.domain,
        port_suffix(config.listen, config.tls_enabled())
    );
    let email = match &config.cloudflare {
        Some(cf) => outer::auth::EmailSender::cloudflare(
            cf.account_id.clone(),
            cf.api_token.clone(),
            outer::auth::EmailFrom::parse(&config.email_from),
        ),
        // Codes in the log are a localhost convenience only: off localhost
        // a missing sender must stop the boot, never leak live codes.
        None if config.is_local() => outer::auth::EmailSender::Log,
        None => anyhow::bail!(
            "no email sender: set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN (Cloudflare Email Service)"
        ),
    };
    Ok(outer::auth::AuthState {
        pool: pool.clone(),
        config: outer::auth::AuthConfig {
            issuer: issuer.clone(),
            resource: format!("{issuer}/mcp"),
            admin_emails: config.admin_emails.clone(),
            signup_open: config.signup_open,
            terms_version: config.terms_version.clone(),
            access_ttl: config.oauth_access_ttl,
            refresh_ttl: config.oauth_refresh_ttl,
            email,
            is_local: config.is_local(),
            allow_loopback_fetch: false,
            origin_locked: config.tls_enabled(),
        },
    })
}

/// `:8080` unless the listener is on the scheme's default port.
fn port_suffix(listen: SocketAddr, tls: bool) -> String {
    match (listen.port(), tls) {
        (80, false) | (443, true) => String::new(),
        (port, _) => format!(":{port}"),
    }
}

/// `https` when TLS terminates locally, else `http`.
fn scheme(config: &Config) -> &'static str {
    if config.tls_enabled() {
        "https"
    } else {
        "http"
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
    if !config.oxfmt_available() {
        // An explicitly configured binary that isn't executable is a prod
        // misconfiguration; the default path missing is the localhost case.
        if config.oxfmt_bin_explicit {
            tracing::error!(
                "oxfmt binary {} explicitly set via OUTER_OXFMT_BIN but not executable: commits skip formatting",
                config.oxfmt_bin.display(),
            );
        } else {
            tracing::warn!(
                "oxfmt binary {} missing or not executable: commits skip formatting (localhost/dev case)",
                config.oxfmt_bin.display(),
            );
        }
    }
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
    let interrupted = outer::deploy::fail_interrupted(&pool).await?;
    if interrupted > 0 {
        tracing::warn!(
            interrupted,
            "closed deploy rows a restart cut off (marked failed)"
        );
    }
    // A crash mid-upload leaves injected plaintext in the worktree: put the
    // stashed pre-inject bytes back and delete the sidecars (next boot
    // retries whatever still fails the symlink check).
    let scrubbed = outer::deploy::scrub_interrupted_injections(&config.data_dir)?;
    if scrubbed > 0 {
        tracing::warn!(
            scrubbed,
            "restored deploy configs a restart cut off mid secret-injection"
        );
    }
    // Bind before claiming the bucket: a second instance that can't listen
    // must not take ownership away from the one that's serving.
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    // Read the TLS triple right after binding (still before the bucket
    // claim): an unreadable cert must fail the boot, never the claim.
    let tls_files = match (&config.tls_cert, &config.tls_key, &config.origin_pull_ca) {
        (Some(cert), Some(key), Some(ca)) => Some(http::tls::TlsFiles {
            cert: cert.clone(),
            key: key.clone(),
            ca: ca.clone(),
        }),
        _ => None,
    };
    let rustls_config = tls_files
        .as_ref()
        .map(http::tls::load_server_config)
        .transpose()?;
    let sync = store::snapshot::StateSync::new(config.host_id.clone());
    sync.claim(&store, &snapshot_cfg).await?;
    store::snapshot::spawn_sync_task(
        sync.clone(),
        pool.clone(),
        store.clone(),
        snapshot_cfg.clone(),
    );

    let rt = &config.runtime;
    let storage = fleet_storage(&config)?;
    // Static local credentials never expire, but the sweep still recycles
    // long-lived dev fleets on the same schedule.
    let credential_refresh_after = config
        .cloudflare
        .as_ref()
        .map(|cf| cf.credential_refresh_after)
        .unwrap_or(Duration::from_secs(6 * 86400));
    let fleets = FleetManager::new(ManagerSettings {
        data_dir: config.data_dir.clone(),
        celld_bin: rt.celld_bin.clone(),
        storage: storage.clone(),
        cgroups: cgroups(),
        memory_max_bytes: rt.fleet_memory_mb * 1024 * 1024,
        cpu_max: rt.fleet_cpu_max.clone(),
        sleep_after: Duration::from_secs(rt.sleep_after_secs),
        credential_refresh_after,
        wake_timeout: Duration::from_secs(30),
        wake_concurrency: rt.wake_concurrency,
        always_on_max: rt.always_on_max,
    });
    // A previous process may have died between a rename's commit and its
    // post-commit deletes: drain pending prefix cleanups before serving, so
    // no old slug stays blocked (and no leftover prefix stays adoptable)
    // across a restart. The sleep sweep below retries this every tick.
    if let Err(err) = outer::apps::rename::drain_prefix_cleanup(&pool, &store, Some(&fleets)).await
    {
        tracing::warn!(error = %format!("{err:#}"), "startup prefix cleanup drain failed");
    }
    fleets.spawn_sleep_sweep(
        pool.clone(),
        store.clone(),
        Duration::from_secs(60.min(rt.sleep_after_secs.max(1))),
    );
    outer::cron::spawn_cron_task(
        pool.clone(),
        fleets.clone(),
        outer::cron::TICK_EVERY,
        outer::cron::WAKE_LEAD,
    );
    // Daily D1 backups (wave 3, feature #4): dump each active app's
    // databases to `outer/backups/{slug}/` and prune past retention.
    outer::d1_backup::spawn_backup_task(
        pool.clone(),
        store.clone(),
        fleets.clone(),
        outer::d1_backup::TICK_EVERY,
    );

    let secrets_key = outer::secrets::SecretsKey::derive(&config.outer_secret);
    // One client for the pipeline gate and the slug/name check alike: both
    // fail closed when it is `None` on localhost dev, and pass with a
    // warning there.
    let jev = jev_client(&config)?;
    let deployer = outer::deploy::Deployer::new(
        outer::deploy::DeploySettings {
            data_dir: config.data_dir.clone(),
            celld_bin: rt.celld_bin.clone(),
            esbuild_bin: rt.esbuild_bin.clone(),
            bun_bin: rt.bun_bin.clone(),
            storage,
            step_timeout: Duration::from_secs(120),
            cgroups: cgroups(),
            apps_domain: rt.apps_domain.clone(),
            cache_purge: cache_purge(&config),
            jev: jev.clone(),
            // The migrate step wakes a sleeping fleet before applying D1
            // migrations (`celld d1` reaches the database through a node).
            wake: Some(fleets.clone()),
            secrets_key: secrets_key.clone(),
        },
        rt.build_concurrency,
    );
    let purges = deployer.purges();
    let auth = auth_state(&config, &pool)?;
    // Console-capture tokens: `look_at_app` mints into this table, the app
    // proxy claims from it. One table shared by both states, never global.
    let look_tokens = outer::look_tokens::LookTokens::new();
    let mcp = outer::mcp::McpState {
        pool: pool.clone(),
        auth: auth.clone(),
        store: store.clone(),
        git: git_config(&config),
        sync: outer::git::GitSync::default(),
        deploys: outer::mcp::Deploys::live(deployer),
        fleets: fleets.clone(),
        alloc: outer::apps::Allocation {
            uid_range: rt.app_uid_range,
            port_range: rt.fleet_ports,
        },
        domain: config.domain.clone(),
        apps_domain: rt.apps_domain.clone(),
        app_scheme: scheme(&config).to_string(),
        app_port_suffix: port_suffix(config.listen, config.tls_enabled()),
        tool_wait: config.tool_wait,
        jev,
        rollback_locks: outer::mcp::RollbackLocks::default(),
        browser: browser_render(&config),
        look_tokens: look_tokens.clone(),
        delete_cfg: outer::apps::delete::DeleteConfig {
            data_dir: config.data_dir.clone(),
            cgroups: cgroups(),
            apps_domain: rt.apps_domain.clone(),
            cache_purge: cache_purge(&config),
        },
        secrets_key,
        unsub_key: config.outer_secret.as_bytes().to_vec(),
    };
    // Email alerts (wave 3, feature #2): error spikes are evaluated from
    // `error_issue` every five minutes; deploy failures and quota
    // thresholds notify inline at their trigger sites.
    outer::alerts::spawn_sweep(
        pool.clone(),
        mcp.auth.config.email.clone(),
        mcp.auth.config.issuer.clone(),
        mcp.unsub_key.clone(),
        std::time::Duration::from_secs(300),
    );
    // Request metrics (spec §13): owned by the proxy state, flushed to
    // SQLite every few seconds by the sweep below (and once more on
    // shutdown), pruned per plan by the same sweep.
    let metrics = outer::observe::metrics::MetricsAgg::new();
    let metrics_sweep = outer::observe::metrics::spawn_sweep(pool.clone(), metrics.clone());
    let app = http::app(
        http::AppState {
            pool: pool.clone(),
            sync: sync.clone(),
            auth,
            mcp,
            site: match &config.webmcp_origin_trial {
                Some(token) => {
                    http::site::SiteDir::new(&config.site_dir).with_origin_trial(token.clone())
                }
                None => http::site::SiteDir::new(&config.site_dir),
            },
        },
        http::proxy::ProxyState::new(
            pool.clone(),
            fleets.clone(),
            rt.apps_domain.clone(),
            config.tls_enabled(),
            metrics.clone(),
        )
        .with_look_tokens(look_tokens),
        config.domain.clone(),
    );
    tracing::info!(
        "outer {} listening on {}://{} (platform {}, apps *.{})",
        env!("CARGO_PKG_VERSION"),
        scheme(&config),
        config.listen,
        config.domain,
        rt.apps_domain
    );
    if let Some(rustls_config) = rustls_config {
        // Origin pull (mTLS): axum-server owns the accept loop, so a failed
        // handshake drops just that connection and never stops serving.
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(rustls_config));
        let std_listener = listener.into_std()?;
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            // Bounded drain: stop_all and the final snapshot run after
            // serve returns, all inside compose's 35 s stop_grace_period.
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        axum_server::tls_rustls::from_tcp_rustls(std_listener, rustls_config)?
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
    } else {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    }

    tracing::info!("shutting down: stopping fleets, then writing the final state snapshot");
    // The sweep may be mid-flush; aborting it is safe (its transaction
    // rolls back) and the drain below re-ships the same deltas.
    metrics_sweep.abort();
    if let Err(err) = metrics.flush(&pool).await {
        tracing::warn!(error = %err, "final metrics flush failed; counts stay in memory only");
    }
    purges.drain().await;
    // Persist the rings before they die with the fleets: the next boot
    // serves these lines from `app_log`. A failure still stops the fleets.
    if let Err(err) = outer::observe::logs::flush_all(&pool, &fleets).await {
        tracing::warn!(error = %format!("{err:#}"), "final log flush failed; unflushed lines are lost");
    }
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
