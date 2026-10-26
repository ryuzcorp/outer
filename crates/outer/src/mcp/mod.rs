//! The MCP endpoint (spec §8): a hand-written Axum handler for `POST /mcp`
//! on the platform host only.
//!
//! One JSON-RPC message per POST, one JSON response, no SSE, no sessions.
//! `initialize`, `ping`, `tools/list` and `tools/call` are the only methods;
//! `GET`/`DELETE` on `/mcp` answer `405` via the method router, unknown
//! methods `−32601`, bad envelopes `−32600`, bad JSON `−32700` and bad
//! params (including unknown tools) `−32602`.

pub mod actions;
pub mod docs;
pub mod error;
pub mod registry;
pub mod telemetry;
pub mod tools;
pub mod tools_page;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::apps::{Allocation, App};
use crate::auth::{AuthState, Principal};
use crate::deploy::{DeployOutcome, Deployer};
use crate::fleet::manager::FleetManager;
use crate::git::GitConfig;
use crate::git::GitSync;
use crate::store::Store;

/// Protocol revisions the handler speaks. The server answers the client's
/// revision when it knows it, else the newest one.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-11-25"];
pub const LATEST_PROTOCOL: &str = "2025-11-25";

/// Exactly the §8.1 text (Codex reads only this much). The length test pins
/// it at or under 512 chars.
pub const INSTRUCTIONS: &str = "Start with `open_app` to orient on the app. Read only what you need with `read_files` (batch up to 20 files, line ranges or one symbol), find code with `search_code`, change code with `commit` edits. Relay `user_message` verbatim. Guides: `docs {topic}` and /llms.txt.";

/// Largest `POST /mcp` body: one `commit` at the caps fits (spec §8.1).
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How the MCP layer runs deploys. Production wraps the real pipeline;
/// tests substitute a fake that writes terminal deploy rows without
/// touching celld or the network.
#[derive(Clone)]
pub struct Deploys {
    inner: Arc<DeploysInner>,
}

enum DeploysInner {
    Live(Box<Deployer>),
    #[cfg(test)]
    Fake(FakeDeploys),
}

impl Deploys {
    /// Run real deploys through the pipeline.
    pub fn live(deployer: Deployer) -> Self {
        Self {
            inner: Arc::new(DeploysInner::Live(Box::new(deployer))),
        }
    }

    /// A fake that answers every deploy with `outcome`, for tests.
    #[cfg(test)]
    pub fn fake(outcome: FakeOutcome) -> Self {
        Self {
            inner: Arc::new(DeploysInner::Fake(FakeDeploys {
                outcome: std::sync::Mutex::new(outcome),
                locks: std::sync::Mutex::new(HashMap::new()),
            })),
        }
    }

    /// Change what the fake answers (tests drive blocked/failed paths).
    #[cfg(test)]
    pub fn set_fake(&self, outcome: FakeOutcome) {
        if let DeploysInner::Fake(fake) = &*self.inner {
            *fake.outcome.lock().expect("fake outcome poisoned") = outcome;
        }
    }

    /// The per-app deploy mutex. Live runs serialize on the pipeline's own
    /// lock; the fake has no pipeline, so it keeps an equivalent map.
    /// `delete_app` holds it across the whole deletion so no deploy can
    /// start mid-delete.
    pub(crate) fn app_lock(&self, app_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        match &*self.inner {
            DeploysInner::Live(deployer) => deployer.app_lock(app_id),
            #[cfg(test)]
            DeploysInner::Fake(fake) => fake.app_lock(app_id),
        }
    }

    /// The live pipeline's settings, for the console data panels (scoped
    /// storage reads, celld calls, work-dir config reads). `None` for the
    /// test fake, which has no pipeline — those endpoints answer
    /// "unavailable" there.
    pub(crate) fn deploy_settings(&self) -> Option<&crate::deploy::pipeline::DeploySettings> {
        match &*self.inner {
            DeploysInner::Live(deployer) => Some(deployer.settings()),
            #[cfg(test)]
            DeploysInner::Fake(_) => None,
        }
    }

    pub async fn deploy(
        &self,
        pool: &SqlitePool,
        git: &GitConfig,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        match &*self.inner {
            DeploysInner::Live(deployer) => deployer.deploy(pool, git, app, sha).await,
            #[cfg(test)]
            DeploysInner::Fake(fake) => fake.deploy(pool, app, sha).await,
        }
    }

    /// The preview counterpart: the row lands as `kind = 'preview'` and a
    /// success never touches `last_deploy_sha`. `app` is the base app; the
    /// fake resolves the preview row the same way the pipeline does.
    pub async fn deploy_preview(
        &self,
        pool: &SqlitePool,
        git: &GitConfig,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        match &*self.inner {
            DeploysInner::Live(deployer) => deployer.deploy_preview(pool, git, app, sha).await,
            #[cfg(test)]
            DeploysInner::Fake(fake) => fake.deploy_preview(pool, app, sha).await,
        }
    }
}
/// What the fake deploy answers with.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FakeOutcome {
    pub status: &'static str,
    pub failed_step: Option<&'static str>,
    pub error: Option<&'static str>,
    pub reason: Option<&'static str>,
    pub findings: Vec<crate::moderation::gate::Finding>,
    pub user_message: Option<&'static str>,
    /// Answer only after this long (timeout-path tests).
    pub delay: Duration,
    /// Fail the deploy itself instead of answering an outcome (the pipeline
    /// blew up): `deploy` answers `Err`, so callers take their
    /// infrastructure-failure path.
    pub transport_error: bool,
}
#[cfg(test)]
impl FakeOutcome {
    pub fn success() -> Self {
        Self {
            status: "success",
            failed_step: None,
            error: None,
            reason: None,
            findings: Vec::new(),
            user_message: None,
            delay: Duration::ZERO,
            transport_error: false,
        }
    }

    /// A successful deploy that answers only after 30 s: longer than any
    /// test `tool_wait`, so awaited calls take the in-progress path.
    pub fn slow() -> Self {
        Self {
            delay: Duration::from_secs(30),
            ..Self::success()
        }
    }

    pub fn failed() -> Self {
        Self {
            status: "failed",
            failed_step: Some("build"),
            error: Some("bun run build failed"),
            reason: None,
            findings: Vec::new(),
            user_message: None,
            delay: Duration::ZERO,
            transport_error: false,
        }
    }

    pub fn blocked() -> Self {
        Self {
            status: "blocked",
            failed_step: Some("gate"),
            error: Some("profanity"),
            reason: Some("profanity"),
            findings: vec![crate::moderation::gate::Finding {
                path: "pages/index.tsx".to_string(),
                line: 42,
                excerpt: "this list is fucking awesome".to_string(),
                category: "profanity",
                signal: "jev",
                score: 0.93,
            }],
            user_message: Some(
                "Outer didn't publish this change because the page text has strong profanity (pages/index.tsx, line 42). Your app still shows the previous version.",
            ),
            delay: Duration::ZERO,
            transport_error: false,
        }
    }

    /// The pipeline itself failed: `deploy` answers `Err` instead of an
    /// outcome, so callers take their infrastructure-failure path.
    pub fn transport_error() -> Self {
        Self {
            transport_error: true,
            ..Self::success()
        }
    }
}

#[cfg(test)]
struct FakeDeploys {
    outcome: std::sync::Mutex<FakeOutcome>,
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[cfg(test)]
impl FakeDeploys {
    fn app_lock(&self, app_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .expect("fake deploy locks poisoned")
            .entry(app_id.to_string())
            .or_default()
            .clone()
    }

    async fn deploy(
        &self,
        pool: &SqlitePool,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        self.deploy_inner(pool, app, sha, "main", true).await
    }

    async fn deploy_preview(
        &self,
        pool: &SqlitePool,
        app: &App,
        sha: &str,
    ) -> anyhow::Result<DeployOutcome> {
        // The pipeline resolves the preview identity (ports, bucket prefix)
        // from the row; a deploy without preview state is a caller bug, not
        // a deploy failure. Pinning it here keeps the fake honest.
        let preview: Option<String> =
            sqlx::query_scalar("SELECT preview_sha FROM app WHERE id = ?1")
                .bind(&app.id)
                .fetch_optional(pool)
                .await?;
        if preview.is_none() {
            return Err(anyhow::anyhow!("fake preview deploy without preview state"));
        }
        self.deploy_inner(pool, app, sha, "preview", false).await
    }

    async fn deploy_inner(
        &self,
        pool: &SqlitePool,
        app: &App,
        sha: &str,
        kind: &str,
        touches_live: bool,
    ) -> anyhow::Result<DeployOutcome> {
        let outcome = self.outcome.lock().expect("fake outcome poisoned").clone();
        if outcome.transport_error {
            return Err(anyhow::anyhow!("fake deploy transport failure"));
        }
        if !outcome.delay.is_zero() {
            tokio::time::sleep(outcome.delay).await;
        }
        let deploy_id = uuid::Uuid::new_v4().to_string();
        let now = crate::apps::now_ms();
        let steps_json = r#"[{"name":"fake","ms":1,"skipped":false}]"#;
        let findings_json = serde_json::to_string(&outcome.findings)?;
        sqlx::query(
            "INSERT INTO deploy (id, app_id, sha, kind, status, steps, error, findings, log_tail, created_at, finished_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'fake deploy ok', ?9, ?9)",
        )
        .bind(&deploy_id)
        .bind(&app.id)
        .bind(sha)
        .bind(kind)
        .bind(outcome.status)
        .bind(steps_json)
        .bind(outcome.error)
        .bind(if outcome.findings.is_empty() { None } else { Some(findings_json) })
        .bind(now)
        .execute(pool)
        .await?;
        if touches_live && outcome.status == "success" {
            crate::apps::set_last_deploy(pool, &app.id, sha).await?;
        }
        Ok(DeployOutcome {
            deploy_id,
            sha: sha.to_string(),
            status: outcome.status,
            failed_step: outcome.failed_step.map(str::to_string),
            error: outcome.error.map(str::to_string),
            reason: outcome.reason.map(str::to_string),
            findings: outcome.findings,
            user_message: outcome.user_message.map(str::to_string),
            steps: vec![crate::deploy::Step {
                name: "fake".to_string(),
                ms: 1,
                skipped: false,
            }],
            log_tail: "fake deploy ok".to_string(),
            version: None,
            reloaded: false,
        })
    }
}

/// At most one rollback runs per app: `try_lock` fails fast with
/// `rate_limited` instead of queueing another full build behind one.
#[derive(Debug, Clone, Default)]
pub struct RollbackLocks {
    inner: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl RollbackLocks {
    fn slot(&self, app_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .lock()
            .expect("rollback locks poisoned")
            .entry(app_id.to_string())
            .or_default()
            .clone()
    }

    pub fn try_lock(&self, app_id: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.slot(app_id).try_lock_owned().ok()
    }

    /// The app's rollback guard, waiting. `delete_app` holds it across the
    /// whole deletion so no rollback can start (or be mid-commit) mid-delete;
    /// a rollback racing a delete fails fast on `try_lock` instead.
    pub async fn lock(&self, app_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.slot(app_id).lock_owned().await
    }
}

/// Everything one MCP call touches. Built once in `main.rs` from config;
/// no globals.
#[derive(Clone)]
pub struct McpState {
    pub pool: SqlitePool,
    pub auth: AuthState,
    pub store: Store,
    pub git: GitConfig,
    pub sync: GitSync,
    pub deploys: Deploys,
    pub fleets: FleetManager,
    pub alloc: Allocation,
    /// AES-256-GCM key for app secrets, derived from `OUTER_SECRET` at boot.
    pub secrets_key: crate::secrets::SecretsKey,
    /// HMAC key for alert unsubscribe tokens, `OUTER_SECRET` bytes: the
    /// sweep and the deploy hook sign with it, the console verifies.
    pub unsub_key: Vec<u8>,
    /// Noreply identities are `<account-id>@users.noreply.<domain>`.
    pub domain: String,
    /// Apps live at `<slug>.<apps_domain>`.
    pub apps_domain: String,
    /// `http` on localhost dev, `https` behind Cloudflare.
    pub app_scheme: String,
    /// `:port` when the listener is off the scheme's default port, else "".
    pub app_port_suffix: String,
    /// Jev client for the slug/name check on `create_app` and `rename_app`.
    /// `None` is localhost dev without the Cloudflare pair: the check logs
    /// a warning and passes, exactly like the pre-live gate.
    pub jev: Option<crate::moderation::JevClient>,
    /// Awaited calls return the in-progress status after this long (§10.3).
    pub tool_wait: Duration,
    /// Cloudflare browser rendering for `look_at_app`. `None` on localhost
    /// dev without the Cloudflare pair: the tool refuses, exactly like the
    /// Jev gate when the pair is missing.
    pub browser: Option<crate::browser::BrowserRender>,
    /// Single-use console-capture tokens `look_at_app` mints: the proxy
    /// half of the same table claims them. Shared, never global.
    pub look_tokens: crate::look_tokens::LookTokens,
    /// At most one rollback runs per app (fails fast otherwise).
    pub rollback_locks: RollbackLocks,
    /// Stripe billing (spec §14.2): checkout/portal links, the
    /// per-account checkout-starter links refusals carry, and webhook
    /// state. `Disabled` on localhost dev without `STRIPE_*`.
    pub billing: crate::billing::BillingState,
    /// What `delete_app` wipes and purges: dirs, edge host, zone.
    pub delete_cfg: crate::apps::delete::DeleteConfig,
}
impl McpState {
    /// `https://<slug>.outer.now`, or the localhost equivalent in dev.
    pub fn app_url(&self, slug: &str) -> String {
        format!(
            "{}://{}.{}{}",
            self.app_scheme, slug, self.apps_domain, self.app_port_suffix
        )
    }

    /// The absolute MCP endpoint clients connect to.
    pub fn mcp_url(&self) -> String {
        self.auth.config.resource.clone()
    }
}

/// `Authorization: Bearer <token>`, or `None` when the request carries none.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

async fn resolve_caller(state: &McpState, headers: &HeaderMap) -> Option<Principal> {
    let token = bearer_token(headers)?;
    crate::auth::resolve_bearer(&state.pool, &state.auth.config, token)
        .await
        .unwrap_or(None)
}

fn rpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Response {
    let id = id.unwrap_or(Value::Null);
    (
        StatusCode::OK,
        axum::Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message.into()},
        })),
    )
        .into_response()
}
fn rpc_result(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
    )
        .into_response()
}

#[cfg(test)]
impl McpState {
    /// A live state for tests: memory store, temp dirs, fake deploys, no
    /// network. The data dir is leaked so mirrors survive the whole test.
    pub fn for_tests(pool: SqlitePool, auth: AuthState) -> Self {
        let dir = tempfile::tempdir().expect("mcp test dir");
        let data_dir = dir.path().to_path_buf();
        std::mem::forget(dir);
        let fleets = FleetManager::new(crate::fleet::manager::ManagerSettings {
            data_dir: data_dir.clone(),
            celld_bin: "celld".to_string(),
            storage: crate::fleet::manager::FleetStorage {
                endpoint: "http://127.0.0.1:1".to_string(),
                region: "us-east-1".to_string(),
                bucket: "outer-mcp-test".to_string(),
                credentials: crate::store::credentials::CredentialSource::Static {
                    access_key_id: "outeraccess".to_string(),
                    secret_access_key: "outersecretoutersecretoutersecret12".to_string(),
                },
            },
            cgroups: None,
            memory_max_bytes: 512 * 1024 * 1024,
            cpu_max: "50000 100000".to_string(),
            sleep_after: Duration::from_secs(1800),
            credential_refresh_after: Duration::from_secs(6 * 86400),
            wake_timeout: Duration::from_secs(30),
            wake_concurrency: 4,
            always_on_max: 10,
            preview_max: 20,
        });
        Self {
            pool,
            auth,
            store: Store::memory(),
            git: GitConfig::new(data_dir.join("git"), None),
            sync: GitSync::default(),
            deploys: Deploys::fake(FakeOutcome::success()),
            fleets,
            alloc: Allocation {
                uid_range: (210000, 210003),
                port_range: (18000, 18003),
            },
            secrets_key: crate::secrets::SecretsKey::test_key(),
            unsub_key: b"outer-test-unsub-key".to_vec(),
            domain: "outer.localhost".to_string(),
            apps_domain: "outer.localhost".to_string(),
            app_scheme: "http".to_string(),
            app_port_suffix: String::new(),
            jev: None,
            tool_wait: Duration::from_millis(200),
            browser: None,
            look_tokens: crate::look_tokens::LookTokens::new(),
            rollback_locks: RollbackLocks::default(),
            billing: crate::billing::BillingState::Disabled {
                platform_base: "http://outer.localhost".to_string(),
            },
            delete_cfg: crate::apps::delete::DeleteConfig {
                data_dir: data_dir.clone(),
                cgroups: None,
                apps_domain: "outer.localhost".to_string(),
                cache_purge: None,
            },
        }
    }

    /// Tests override this when they need an enabled signer: the default
    /// is disabled (no `STRIPE_*` in the test env), and refusals point at
    /// the console plan card.
    pub fn with_billing(mut self, billing: crate::billing::BillingState) -> Self {
        self.billing = billing;
        self
    }
}

/// `POST /mcp`: one JSON-RPC message in, one JSON response out. The whole
/// request is taken so oversized bodies are refused before they load.
pub async fn mcp_handler(
    State(app): State<crate::http::AppState>,
    req: axum::extract::Request,
) -> Response {
    let state = &app.mcp;
    let (parts, body) = req.into_parts();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body over the 32 MiB cap",
            )
                .into_response();
        }
    };
    let headers = parts.headers;
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => return rpc_error(extract_id(&body), -32700, "Parse error"),
    };
    let Some(message) = message.as_object() else {
        return rpc_error(None, -32600, "Invalid Request: expected an object");
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return rpc_error(
            clone_id(message),
            -32600,
            "Invalid Request: jsonrpc must be \"2.0\"",
        );
    }
    let id = clone_id(message);
    let notification = id.is_none();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        if notification {
            return StatusCode::ACCEPTED.into_response();
        }
        return rpc_error(id, -32600, "Invalid Request: missing method");
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let id = match id {
                Some(id) => id,
                None => return StatusCode::ACCEPTED.into_response(),
            };
            handle_initialize(state, &headers, id, &params).await
        }
        "ping" => {
            let id = match id {
                Some(id) => id,
                None => return StatusCode::ACCEPTED.into_response(),
            };
            rpc_result(id, json!({}))
        }
        "tools/list" => {
            let id = match id {
                Some(id) => id,
                None => return StatusCode::ACCEPTED.into_response(),
            };
            let caller = resolve_caller(state, &headers).await;
            let tools: Vec<Value> = registry::visible_tools(caller.is_some())
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "title": registry::title_of(t.name),
                        "description": t.description,
                        "inputSchema": t.input_schema,
                        "annotations": t.hints.to_json(&registry::title_of(t.name)),
                    })
                })
                .collect();
            rpc_result(id, json!({"tools": tools}))
        }
        "tools/call" => {
            let id = match id {
                Some(id) => id,
                None => return StatusCode::ACCEPTED.into_response(),
            };
            handle_call(state, &headers, peer, id, &params).await
        }
        _ if method.starts_with("notifications/") => StatusCode::ACCEPTED.into_response(),
        _ => {
            if notification {
                return StatusCode::ACCEPTED.into_response();
            }
            rpc_error(id, -32601, format!("Method not found: {method}"))
        }
    }
}

/// The `id` of a body that failed to parse as JSON: best effort, else null.
fn extract_id(body: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("id").cloned())
}

fn clone_id(message: &serde_json::Map<String, Value>) -> Option<Value> {
    match message.get("id") {
        None | Some(Value::Null) => None,
        Some(id) => Some(id.clone()),
    }
}

async fn handle_initialize(
    state: &McpState,
    headers: &HeaderMap,
    id: Value,
    params: &Value,
) -> Response {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let Some(requested) = requested else {
        return rpc_error(
            Some(id),
            -32602,
            "Invalid params: protocolVersion is required",
        );
    };
    let negotiated = if PROTOCOL_VERSIONS.contains(&requested) {
        requested.to_string()
    } else {
        LATEST_PROTOCOL.to_string()
    };
    // Record who is connecting, for the validation stats (spec §8.1).
    if let Some(caller) = resolve_caller(state, headers).await {
        if let (Some(name), Some(version)) = (
            params
                .get("clientInfo")
                .and_then(|c| c.get("name"))
                .and_then(Value::as_str),
            params
                .get("clientInfo")
                .and_then(|c| c.get("version"))
                .and_then(Value::as_str),
        ) {
            if !name.is_empty() {
                let _ =
                    crate::auth::record_client_info(&state.pool, &caller.credential, name, version)
                        .await;
            }
        }
    }
    rpc_result(
        id,
        json!({
            "protocolVersion": negotiated,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "outer", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        }),
    )
}

async fn handle_call(
    state: &McpState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    id: Value,
    params: &Value,
) -> Response {
    let name = params.get("name").and_then(Value::as_str);
    let Some(name) = name else {
        return rpc_error(Some(id), -32602, "Invalid params: name is required");
    };
    let Some(def) = registry::registry()
        .iter()
        .find(|t| t.name == name)
        .cloned()
    else {
        return rpc_error(Some(id), -32602, format!("unknown tool {name:?}"));
    };
    let args = params.get("arguments").cloned();
    // Lazy auth (spec §6.1): the gate lives in the HTTP handler, before the
    // tool runs. Anonymous callers may still list account tools — only
    // calling one without a token answers 401.
    let caller = resolve_caller(state, headers).await;
    if def.access != registry::Access::Public && caller.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                crate::auth::www_authenticate(&state.auth.config),
            )],
            "Sign in to Outer to use this tool",
        )
            .into_response();
    }
    let client_ip =
        crate::http::client_ip::client_ip(headers, state.auth.config.origin_locked, peer);
    let ctx = tools::Ctx {
        state,
        principal: caller,
        client_ip: client_ip.clone(),
    };
    // The agent action log (spec §13.1): time the call, then record one row
    // for app-targeting tools. Recording runs after the result is computed
    // and never alters it: a failed insert only warns.
    let started = std::time::Instant::now();
    let outcome = tools::dispatch(name, args.clone(), &ctx).await;
    let duration_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    if let Some(caller) = ctx.principal.as_ref() {
        actions::record(
            &state.pool,
            caller,
            name,
            args.as_ref(),
            &outcome,
            duration_ms,
        )
        .await;
    }
    // Tool telemetry (migration 0015): every call, spawned so the insert
    // never slows the response. Only scalar facts reach the row — never
    // arguments or outputs.
    telemetry::spawn_record(
        state.pool.clone(),
        telemetry::RecordReq::from_outcome(
            name,
            ctx.principal.clone(),
            client_ip.clone(),
            args.as_ref(),
            &outcome,
            duration_ms,
        ),
    );
    match outcome {
        Ok(result) => rpc_result(id, result),
        Err(tools::CallError::Rpc(code, message)) => rpc_error(Some(id), code, message),
        Err(tools::CallError::Tool(err)) => rpc_result(id, err.into_result()),
    }
}
