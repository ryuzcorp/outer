//! Operator seeding: `outer seed demo --owner EMAIL [--url URL]` prepares one
//! review account (Pro, terms accepted) and then acts as an MCP client of
//! that account: it mints a short-lived API key, creates the two demo apps
//! through `create_app`, commits the demo pages and their migration through
//! `commit`, waits for each deploy through `deploy_status`, and revokes the
//! key. The server runs the deploys: a first deploy with D1 migrations needs
//! the fleet manager, which only `serve` has.
//!
//! Idempotent: an app owned by the account whose latest deploy succeeded and
//! whose head still carries the demo migration is left alone; a failed or
//! incomplete app is recommitted and redeployed.

use std::time::Duration;

use anyhow::Context as _;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::apps;
use crate::auth;
use crate::config::ReviewLogin;

/// The API key each run mints for the account, revoked before the run ends.
const SEED_KEY_NAME: &str = "seed demo";
/// The file every demo deploy must carry; absent means the seed never landed.
const DEMO_MIGRATION: &str = "db/migrations/0001_demo.sql";
/// Cap on one `POST /mcp`: above a long tool wait (`OUTER_TOOL_WAIT_S`), so a
/// slow deploy answers are followed by `deploy_status` rather than cut off by
/// the client.
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Cap on connecting to the platform: a stopped or unreachable server fails
/// quickly instead of holding the CLI.
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on one deploy the seed follows; a deploy still running is reported
/// instead of polled forever.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The most characters a slug may have (`apps::slug`).
const MAX_SLUG_LEN: usize = 48;

/// One seeded app and where it serves.
#[derive(Debug)]
pub struct SeededApp {
    pub slug: String,
    pub url: String,
}

struct DemoApp {
    /// The slug suffix after the owner's local part.
    suffix: &'static str,
    name: &'static str,
    description: &'static str,
    /// Files written into the materialized template before the seed commit.
    files: &'static [(&'static str, &'static str)],
}

const DEMO_APPS: [DemoApp; 2] = [
    DemoApp {
        suffix: "demo-notes",
        name: "Demo Notes",
        description: "A short list of notes that saves to the app's database.",
        files: &[
            ("db/migrations/0001_demo.sql", NOTES_SQL),
            ("pages/_notes.ts", NOTES_RULES),
            ("pages/notes.server.ts", NOTES_SERVER),
            ("pages/notes.tsx", NOTES_PAGE),
            ("pages/_nav.ts", NOTES_NAV),
        ],
    },
    DemoApp {
        suffix: "demo-tasks",
        name: "Demo Tasks",
        description: "A small task list with a done toggle, saved in the app's database.",
        files: &[
            ("db/migrations/0001_demo.sql", TASKS_SQL),
            ("pages/_tasks.ts", TASKS_RULES),
            ("pages/tasks.server.ts", TASKS_SERVER),
            ("pages/tasks.tsx", TASKS_PAGE),
            ("pages/_nav.ts", TASKS_NAV),
        ],
    },
];

/// Seed the demo account and both demo apps for one review login. The email
/// must be listed in `OUTER_REVIEW_LOGINS`: that env list is the allowlist.
/// Only the account preparation touches the database directly; the apps are
/// created, committed and deployed by the running server, driven over MCP
/// with a key minted for the account and revoked before returning. Fails
/// when a slug belongs to someone else or a deploy is not a success.
pub async fn seed_demo(
    pool: &SqlitePool,
    terms_version: &str,
    owner_email: &str,
    review_logins: &[ReviewLogin],
    mcp_url: &str,
) -> anyhow::Result<Vec<SeededApp>> {
    let owner_email = auth::normalize_email(owner_email);
    anyhow::ensure!(
        review_logins.iter().any(|login| login.email == owner_email),
        "{owner_email} is not in OUTER_REVIEW_LOGINS; list it before seeding its demo apps"
    );
    // Pro, verified and terms-accepted before any app: `create_app` reads the
    // plan for the app limit and the keep-warm default, and a reviewer must
    // never stop on the terms page.
    let now = apps::now_ms();
    let owner_id = apps::ensure_account(pool, &owner_email).await?;
    sqlx::query(
        "UPDATE user SET plan = 'pro', planStatus = 'active', emailVerified = 1, \
         termsVersion = ?, termsAcceptedAt = ?, lastActiveAt = ?, updatedAt = ? WHERE id = ?",
    )
    .bind(terms_version)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(&owner_id)
    .execute(pool)
    .await
    .context("marking the review account Pro")?;
    let key = auth::create_api_key(pool, &owner_id, Some(SEED_KEY_NAME))
        .await
        .context("minting the seed API key")?;
    let client = SeedMcp::new(mcp_url, key.key.clone());
    let seeded = seed_apps(&client, &owner_email).await;
    // The key must not outlive the run, including when seeding failed. A
    // revocation failure is loud: a live key with no owner watching is a
    // credential leak.
    let revoked = auth::revoke_api_key(pool, &owner_id, &key.prefix).await;
    match (seeded, revoked) {
        (Ok(seeded), Ok(true)) => Ok(seeded),
        (Ok(_), Ok(false)) => anyhow::bail!(
            "seeded the demo apps but the seed API key (prefix {}) was not found to revoke; revoke it in the console",
            key.prefix
        ),
        (Ok(_), Err(err)) => anyhow::bail!(
            "seeded the demo apps but revoking the seed API key (prefix {}) failed: {err}; revoke it in the console",
            key.prefix
        ),
        (Err(err), Ok(_)) => Err(err),
        (Err(err), Err(revoke_err)) => Err(err.context(format!(
            "also failed to revoke the seed API key (prefix {}): {revoke_err}; revoke it in the console",
            key.prefix
        ))),
    }
}

/// The per-app loop: read what the server already has, decide, then create or
/// recommit through MCP.
async fn seed_apps(client: &SeedMcp, owner_email: &str) -> anyhow::Result<Vec<SeededApp>> {
    let local = owner_email.split('@').next().unwrap_or("review");
    let mut seeded = Vec::with_capacity(DEMO_APPS.len());
    for demo in &DEMO_APPS {
        let slug = demo_slug(local, demo.suffix)?;
        match client.get_app(&slug).await {
            Ok(info) => {
                let role = info
                    .get("role")
                    .and_then(Value::as_str)
                    .with_context(|| format!("get_app {slug:?} returned no role"))?;
                let latest = info.pointer("/last_deploy/status").and_then(Value::as_str);
                let demo_files = role == "owner" && client.has_demo_migration(&slug).await?;
                match decide(&slug, Some(role), latest, demo_files)? {
                    SeedStep::Skip => {}
                    SeedStep::Redeploy => {
                        client.commit_demo(demo, &slug).await?;
                        client.wait_deploy(&slug).await?;
                    }
                    SeedStep::Create => {
                        anyhow::bail!("get_app {slug:?} answered but the seed planned no create")
                    }
                }
                let url = info
                    .get("url")
                    .and_then(Value::as_str)
                    .with_context(|| format!("get_app {slug:?} returned no url"))?
                    .to_string();
                seeded.push(SeededApp { slug, url });
            }
            Err(err) if err.is_not_found() => {
                // A slug owned by another account is equally invisible here;
                // `create_app` then refuses it as taken, which the mapping
                // below turns into the ownership message.
                let created = match client.create_app(demo, &slug).await {
                    Ok(created) => created,
                    Err(McpError::Tool { message, .. }) if message.contains("slug is taken") => {
                        anyhow::bail!(
                            "slug {slug:?} already belongs to another account; choose a review login whose local part is free"
                        )
                    }
                    Err(err) => {
                        return Err(anyhow::anyhow!(
                            "creating {slug} through the MCP server: {err}"
                        ))
                    }
                };
                let url = created
                    .pointer("/app/url")
                    .and_then(Value::as_str)
                    .with_context(|| format!("create_app {slug:?} returned no url"))?
                    .to_string();
                client.wait_deploy(&slug).await?;
                client.commit_demo(demo, &slug).await?;
                client.wait_deploy(&slug).await?;
                seeded.push(SeededApp { slug, url });
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "reading {slug} through the MCP server: {err}"
                ))
            }
        }
    }
    Ok(seeded)
}

/// `<sanitized local part>-<suffix>`: lowercase letters, digits and single
/// hyphens, at most 48 characters, valid per `apps::validate_slug`. The local
/// part keeps several review accounts from colliding on one slug.
fn demo_slug(local: &str, suffix: &str) -> anyhow::Result<String> {
    let mut base = String::new();
    let mut last_dash = false;
    for ch in local.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            base.push(ch);
            last_dash = false;
        } else if !last_dash && !base.is_empty() {
            base.push('-');
            last_dash = true;
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str("review");
    }
    let room = MAX_SLUG_LEN.saturating_sub(suffix.len() + 1);
    if base.len() > room {
        base.truncate(room);
        while base.ends_with('-') {
            base.pop();
        }
    }
    if base.is_empty() {
        base.push_str("review");
    }
    let slug = format!("{base}-{suffix}");
    apps::validate_slug(&slug)?;
    Ok(slug)
}

// --- MCP client ------------------------------------------------------------

/// What the seed does for one demo slug after reading the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeedStep {
    /// Not visible to the account: create it (a slug owned by someone else
    /// is also invisible; `create_app` refuses that as taken).
    Create,
    /// Owned by the account, but the latest deploy failed or the demo
    /// migration is missing from the head: commit the demo files again.
    Redeploy,
    /// Owned, latest deploy a success, demo migration present.
    Skip,
}

/// The idempotency decision. `role` is `get_app`'s role (`None` when the app
/// is not visible to the account); `latest` is its last deploy's status;
/// `demo_files` says the demo migration is at the head.
fn decide(
    slug: &str,
    role: Option<&str>,
    latest: Option<&str>,
    demo_files: bool,
) -> anyhow::Result<SeedStep> {
    match role {
        None => Ok(SeedStep::Create),
        Some("owner") if latest == Some("success") && demo_files => Ok(SeedStep::Skip),
        Some("owner") => Ok(SeedStep::Redeploy),
        Some(_) => anyhow::bail!(
            "slug {slug:?} already belongs to another account; choose a review login whose local part is free"
        ),
    }
}

/// The one JSON-RPC message the seed posts: a `tools/call`.
fn rpc_request(tool: &str, args: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
}

/// `{base}/mcp`, tolerating a trailing slash on the base or a base that
/// already names `/mcp`.
fn mcp_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/mcp") {
        base.to_string()
    } else {
        format!("{base}/mcp")
    }
}

#[derive(Debug, thiserror::Error)]
enum McpError {
    #[error("MCP request failed: {0}")]
    Transport(String),
    #[error("MCP error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("MCP tool {code}: {message}")]
    Tool { code: String, message: String },
}

impl McpError {
    /// A tool that answered `not_found`: the app is not visible to this
    /// account (it is free or owned by someone else).
    fn is_not_found(&self) -> bool {
        matches!(self, Self::Tool { code, .. } if code == "not_found")
    }
}

/// The narrow MCP client the seed drives: one API key, one tool call per
/// POST, no session. [`seed_demo`] mints and revokes the key.
struct SeedMcp {
    endpoint: String,
    key: String,
    http: reqwest::Client,
}

impl SeedMcp {
    fn new(base_url: &str, key: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(MCP_REQUEST_TIMEOUT)
            .connect_timeout(MCP_CONNECT_TIMEOUT)
            .build()
            .expect("the reqwest client builds with default settings");
        Self {
            endpoint: mcp_endpoint(base_url),
            key,
            http,
        }
    }

    /// One `tools/call`, returning its `structuredContent`. A tool-level
    /// refusal (`isError`) or a JSON-RPC error is an error here, never a
    /// value.
    async fn call(&self, tool: &str, args: Value) -> Result<Value, McpError> {
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .json(&rpc_request(tool, &args))
            .send()
            .await
            .map_err(|err| McpError::Transport(one_line(&err.to_string())))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| McpError::Transport(one_line(&err.to_string())))?;
        if !status.is_success() {
            return Err(McpError::Transport(format!(
                "POST /mcp answered {status}: {}",
                first_line(&body)
            )));
        }
        let message: Value = serde_json::from_str(&body)
            .map_err(|err| McpError::Transport(format!("POST /mcp answered non-JSON: {err}")))?;
        if let Some(error) = message.get("error") {
            return Err(McpError::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
                    .to_string(),
            });
        }
        let result = message
            .get("result")
            .ok_or_else(|| McpError::Transport("POST /mcp answered no result".to_string()))?;
        if result.get("isError") == Some(&Value::Bool(true)) {
            let structured = result.get("structuredContent");
            let code = structured
                .and_then(|value| value.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            let text = structured
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .or_else(|| {
                    result
                        .get("content")
                        .and_then(Value::as_array)
                        .and_then(|blocks| blocks.first())
                        .and_then(|block| block.get("text"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("the tool failed without a message");
            return Err(McpError::Tool {
                code: code.to_string(),
                message: text.to_string(),
            });
        }
        Ok(result
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn get_app(&self, slug: &str) -> Result<Value, McpError> {
        self.call("get_app", json!({"app": slug})).await
    }

    async fn has_demo_migration(&self, slug: &str) -> Result<bool, McpError> {
        let listed = self
            .call(
                "list_files",
                json!({"app": slug, "path_prefix": "db/migrations"}),
            )
            .await?;
        Ok(listed
            .get("files")
            .and_then(Value::as_array)
            .is_some_and(|files| {
                files
                    .iter()
                    .any(|file| file.get("path").and_then(Value::as_str) == Some(DEMO_MIGRATION))
            }))
    }

    async fn create_app(&self, demo: &DemoApp, slug: &str) -> Result<Value, McpError> {
        self.call(
            "create_app",
            json!({
                "slug": slug,
                "name": demo.name,
                "description": demo.description,
            }),
        )
        .await
    }

    /// Commit the demo files and wait for the deploy. Recommitting the same
    /// files is intentional: it is how a failed or incomplete app gets a new
    /// deploy (a commit always mints a new sha).
    async fn commit_demo(&self, demo: &DemoApp, slug: &str) -> Result<Value, McpError> {
        let writes: Vec<Value> = demo
            .files
            .iter()
            .map(|(path, content)| json!({"path": path, "content": content}))
            .collect();
        self.call(
            "commit",
            json!({
                "app": slug,
                "message": format!("Add the {} demo page and its database", demo.name),
                "writes": writes,
            }),
        )
        .await
    }

    /// Follow the app's latest deploy to a terminal state. `deploy_status`
    /// with `wait: true` returns early when the server's tool wait runs out,
    /// so the loop repeats until the deploy ends or the cap is hit.
    async fn wait_deploy(&self, slug: &str) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + DEPLOY_TIMEOUT;
        loop {
            let deploy = self
                .call("deploy_status", json!({"app": slug, "wait": true}))
                .await
                .map_err(|err| anyhow::anyhow!("deploy_status {slug}: {err}"))?;
            let status = deploy.get("status").and_then(Value::as_str).unwrap_or("");
            match status {
                "success" => return Ok(()),
                "failed" | "blocked" => {
                    let step = deploy
                        .get("failed_step")
                        .and_then(Value::as_str)
                        .map(|step| format!(" at {step}"))
                        .unwrap_or_default();
                    let error = deploy
                        .get("error")
                        .and_then(Value::as_str)
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default();
                    anyhow::bail!("seeding {slug} ended as {status}{step}{error}");
                }
                other => {
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "seeding {slug}: the deploy is still {:?} after {} minutes",
                            if other.is_empty() { "unknown" } else { other },
                            DEPLOY_TIMEOUT.as_secs() / 60
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

/// One bounded line of a transport error body.
fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

/// One bounded line: reqwest errors can run long and carry the URL (never
/// the key, which travels in a header).
fn one_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or(text)
        .chars()
        .take(300)
        .collect()
}

// --- Demo app sources ------------------------------------------------------
//
// Plain, neutral product copy (the pre-live gate reads page text and SQL
// literals). Every page follows the vendored template's conventions: a
// `*.server.ts` loader/action pair, a `useForm` form, and a migration under
// `db/migrations/`.

const NOTES_SQL: &str = "\
CREATE TABLE IF NOT EXISTS note (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL);
INSERT INTO note (text) VALUES ('This list lives in the app''s own database.');
INSERT INTO note (text) VALUES ('Use the form below to add another note.');
INSERT INTO note (text) VALUES ('Rows appear here right after you save them.');
INSERT INTO note (text) VALUES ('The data survives restarts and sleeps.');
INSERT INTO note (text) VALUES ('That is the whole app: one table, one form.');
";

const NOTES_RULES: &str = "\
// Shared rules for the notes page: the text limit and its inline message.
// Both the server action and the rendered form use this one file.
export const MAX_TEXT = 200;

export function noteError(text: string): string | null {
  const value = text.trim();
  if (!value) {
    return \"Write a note first.\";
  }
  if ([...value].length > MAX_TEXT) {
    return `Keep it under ${MAX_TEXT} characters.`;
  }
  return null;
}
";

const NOTES_SERVER: &str = "\
import { env } from \"cloudflare:workers\";
import { defineHandler, defineHead } from \"void\";
import type { InferProps } from \"void\";

import { noteError } from \"./_notes.ts\";

export interface Note {
  id: number;
  text: string;
}

interface Row {
  id: number;
  text: string;
}

// The table comes from db/migrations/0001_demo.sql. The CREATE below is the
// backstop for preview renders that run no migrations; it matches the
// migrated schema, so the two can never disagree. Nothing is kept in module
// scope: the app can sleep between requests, and only D1 survives.
async function database() {
  await env.DB.prepare(
    \"CREATE TABLE IF NOT EXISTS note (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL)\",
  ).run();
  return env.DB;
}

export interface Props {
  notes: Note[];
}

export const loader = defineHandler(async (): Promise<Props> => {
  const db = await database();
  const { results } = await db.prepare(\"SELECT id, text FROM note ORDER BY id\").all<Row>();
  return { notes: results ?? [] };
});

export const head = defineHead(() => ({
  title: \"Notes\",
  meta: [{ name: \"description\", content: \"A short list of notes saved in this app's database.\" }],
}));

// One action; the `intent` field tells forms apart. Return nothing on
// success so Void re-runs the loader in place.
export const action = defineHandler(async (c) => {
  const body = await c.req.json<{ intent?: string; text?: string }>();
  if (body.intent !== \"add\") {
    return;
  }
  const message = noteError(body.text ?? \"\");
  if (message) {
    return c.json({ errors: { text: message } }, 422);
  }
  const db = await database();
  await db
    .prepare(\"INSERT INTO note (text) VALUES (?)\")
    .bind((body.text ?? \"\").trim())
    .run();
});

export type PageProps = InferProps<typeof loader>;
";

const NOTES_PAGE: &str = "\
import { Form, useForm } from \"@ilha/void\";

import { MAX_TEXT } from \"./_notes.ts\";
import type { Props } from \"./notes.server.ts\";

export default function Notes({ notes }: Props) {
  const add = useForm(\"/notes\" as string, { intent: \"add\", text: \"\" });
  return (
    <div class=\"flex flex-col gap-5\">
      <section class=\"card bg-base-100 shadow-sm\">
        <div class=\"card-body gap-2\">
          <h1 class=\"card-title text-2xl sm:text-3xl\">Notes</h1>
          <p class=\"opacity-70\">A short list saved in this app's database.</p>
        </div>
      </section>
      <section aria-label=\"Notes\" class=\"card bg-base-100 shadow-sm\">
        <div class=\"card-body gap-4\">
          {notes.length ? (
            <ul class=\"list rounded-box bg-base-200\">
              {notes.map((note) => (
                <li key={note.id} class=\"list-row break-words\">
                  {note.text}
                </li>
              ))}
            </ul>
          ) : (
            <p role=\"status\" class=\"opacity-70\">Nothing here yet — add the first note below.</p>
          )}
          <Form form={add} class=\"join w-full\">
            <input
              name=\"text\"
              aria-label=\"New note\"
              placeholder=\"Add a note\"
              maxlength={MAX_TEXT}
              value={add.data.text}
              oninput={(event) => add.setData(\"text\", event.currentTarget.value)}
              class=\"input join-item min-w-0 grow\"
            />
            <button type=\"submit\" disabled={add.pending} class=\"btn btn-primary join-item\">
              {add.pending ? \"Adding…\" : \"Add\"}
            </button>
          </Form>
          {add.errors.text ? <p class=\"text-error\">{add.errors.text}</p> : null}
        </div>
      </section>
    </div>
  );
}
";

const NOTES_NAV: &str = "\
// The nav in one place: `layout.tsx` renders these links in the header, the
// desktop sidebar, and the phone tab bar. Adding a page means one new route
// file plus one line here. Icons are Lucide icon nodes — pick any name from
// https://lucide.dev/icons and import it; no new component needed.
import { House, NotebookPen, Settings } from \"lucide\"; // outer:nav-icons
import type { IconNode } from \"lucide\";

export interface NavItem {
  href: string;
  label: string;
  icon: IconNode;
}

export const NAV: readonly NavItem[] = [
  { href: \"/\", label: \"Home\", icon: House },
  // outer:nav-items — add_example inserts pages above this line
  { href: \"/notes\", label: \"Notes\", icon: NotebookPen },
  { href: \"/settings\", label: \"Settings\", icon: Settings },
];
";

const TASKS_SQL: &str = "\
CREATE TABLE IF NOT EXISTS task (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0);
INSERT INTO task (text, done) VALUES ('Try the add form below', 0);
INSERT INTO task (text, done) VALUES ('Tick an item to mark it done', 0);
INSERT INTO task (text, done) VALUES ('Every save goes straight to the database', 0);
INSERT INTO task (text, done) VALUES ('The list survives restarts and sleeps', 0);
INSERT INTO task (text, done) VALUES ('This row starts out done', 1);
";

const TASKS_RULES: &str = "\
// Shared rules for the tasks page: the text limit and its inline message.
// Both the server action and the rendered form use this one file.
export const MAX_TEXT = 200;

export function taskError(text: string): string | null {
  const value = text.trim();
  if (!value) {
    return \"Write a task first.\";
  }
  if ([...value].length > MAX_TEXT) {
    return `Keep it under ${MAX_TEXT} characters.`;
  }
  return null;
}
";

const TASKS_SERVER: &str = "\
import { env } from \"cloudflare:workers\";
import { defineHandler, defineHead } from \"void\";
import type { InferProps } from \"void\";

import { taskError } from \"./_tasks.ts\";

export interface Task {
  id: number;
  text: string;
  done: boolean;
}

interface Row {
  id: number;
  text: string;
  done: number;
}

// The table comes from db/migrations/0001_demo.sql. The CREATE below is the
// backstop for preview renders that run no migrations; it matches the
// migrated schema, so the two can never disagree. Nothing is kept in module
// scope: the app can sleep between requests, and only D1 survives.
async function database() {
  await env.DB.prepare(
    \"CREATE TABLE IF NOT EXISTS task (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)\",
  ).run();
  return env.DB;
}

export interface Props {
  tasks: Task[];
}

export const loader = defineHandler(async (): Promise<Props> => {
  const db = await database();
  const { results } = await db
    .prepare(\"SELECT id, text, done FROM task ORDER BY id\")
    .all<Row>();
  return {
    tasks: (results ?? []).map((row) => ({
      id: row.id,
      text: row.text,
      done: row.done === 1,
    })),
  };
});

export const head = defineHead(() => ({
  title: \"Tasks\",
  meta: [{ name: \"description\", content: \"A small task list saved in this app's database.\" }],
}));

// One action; the `intent` field tells the forms apart. Return nothing on
// success so Void re-runs the loader in place.
export const action = defineHandler(async (c) => {
  const body = await c.req.json<{ intent?: string; text?: string; id?: number }>();
  const db = await database();
  if (body.intent === \"add\") {
    const message = taskError(body.text ?? \"\");
    if (message) {
      return c.json({ errors: { text: message } }, 422);
    }
    await db
      .prepare(\"INSERT INTO task (text) VALUES (?)\")
      .bind((body.text ?? \"\").trim())
      .run();
  } else if (body.intent === \"toggle\" && Number.isInteger(body.id)) {
    await db.prepare(\"UPDATE task SET done = 1 - done WHERE id = ?\").bind(body.id).run();
  }
});

export type PageProps = InferProps<typeof loader>;
";

const TASKS_PAGE: &str = "\
import { Form, useForm } from \"@ilha/void\";

import { MAX_TEXT } from \"./_tasks.ts\";
import type { Task } from \"./tasks.server.ts\";
import type { Props } from \"./tasks.server.ts\";

function ItemRow({ task }: { task: Task }) {
  const toggle = useForm(\"/tasks\" as string, { intent: \"toggle\", id: task.id });
  return (
    <li class=\"list-row items-center gap-2\">
      <Form form={toggle}>
        <label class=\"flex min-h-11 min-w-11 cursor-pointer items-center justify-center\">
          <input
            type=\"checkbox\"
            checked={task.done}
            disabled={toggle.pending}
            aria-label={task.done ? `Mark ${task.text} as not done` : `Mark ${task.text} as done`}
            onchange={(event) => event.currentTarget.form?.requestSubmit()}
            class=\"checkbox checkbox-primary\"
          />
        </label>
      </Form>
      <span class={`list-col-grow min-w-0 break-words ${task.done ? \"opacity-60 line-through\" : \"\"}`}>
        {task.text}
      </span>
    </li>
  );
}

export default function Tasks({ tasks }: Props) {
  const add = useForm(\"/tasks\" as string, { intent: \"add\", text: \"\" });
  const done = tasks.filter((task) => task.done).length;
  return (
    <div class=\"flex flex-col gap-5\">
      <section class=\"card bg-base-100 shadow-sm\">
        <div class=\"card-body gap-2\">
          <div class=\"flex items-center justify-between gap-2\">
            <h1 class=\"card-title text-2xl sm:text-3xl\">Tasks</h1>
            {tasks.length ? (
              <span role=\"status\" class=\"badge badge-primary shrink-0\">
                {done} of {tasks.length} done
              </span>
            ) : null}
          </div>
          <p class=\"opacity-70\">A small list saved in this app's database.</p>
        </div>
      </section>
      <section aria-label=\"Tasks\" class=\"card bg-base-100 shadow-sm\">
        <div class=\"card-body gap-4\">
          {tasks.length ? (
            <ul class=\"list rounded-box bg-base-200\">
              {tasks.map((task) => (
                <ItemRow key={task.id} task={task} />
              ))}
            </ul>
          ) : (
            <p role=\"status\" class=\"opacity-70\">Nothing here yet — add the first task below.</p>
          )}
          <Form form={add} class=\"join w-full\">
            <input
              name=\"text\"
              aria-label=\"New task\"
              placeholder=\"Add a task\"
              maxlength={MAX_TEXT}
              value={add.data.text}
              oninput={(event) => add.setData(\"text\", event.currentTarget.value)}
              class=\"input join-item min-w-0 grow\"
            />
            <button type=\"submit\" disabled={add.pending} class=\"btn btn-primary join-item\">
              {add.pending ? \"Adding…\" : \"Add\"}
            </button>
          </Form>
          {add.errors.text ? <p class=\"text-error\">{add.errors.text}</p> : null}
        </div>
      </section>
    </div>
  );
}
";

const TASKS_NAV: &str = "\
// The nav in one place: `layout.tsx` renders these links in the header, the
// desktop sidebar, and the phone tab bar. Adding a page means one new route
// file plus one line here. Icons are Lucide icon nodes — pick any name from
// https://lucide.dev/icons and import it; no new component needed.
import { House, ListChecks, Settings } from \"lucide\"; // outer:nav-icons
import type { IconNode } from \"lucide\";

export interface NavItem {
  href: string;
  label: string;
  icon: IconNode;
}

export const NAV: readonly NavItem[] = [
  { href: \"/\", label: \"Home\", icon: House },
  // outer:nav-items — add_example inserts pages above this line
  { href: \"/tasks\", label: \"Tasks\", icon: ListChecks },
  { href: \"/settings\", label: \"Settings\", icon: Settings },
];
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::Allocation;
    use crate::auth::test_support as auth_ts;
    use crate::mcp::McpState;

    fn login(email: &str) -> ReviewLogin {
        ReviewLogin {
            email: email.to_string(),
            password_hash: auth_ts::REVIEW_PHC.to_string(),
        }
    }

    /// The in-process server the seed talks to: the fake MCP deployer, the
    /// memory store, the vendored test template and the real router on a
    /// loopback port. The returned URL is the base the seed gets as `--url`.
    async fn server() -> (
        McpState,
        SqlitePool,
        String,
        tempfile::TempDir,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&dir.path().join("outer.sqlite"))
            .await
            .unwrap();
        let (auth_cfg, _codes) = auth_ts::test_config();
        let state = McpState::for_tests(
            pool.clone(),
            crate::auth::AuthState {
                pool: pool.clone(),
                config: auth_cfg,
            },
        );
        crate::templates::vendor_test_templates(
            &state.store,
            &[
                ("package.json", "{}\n"),
                ("pages/index.tsx", "<h1>Groceries</h1>\n"),
                (
                    "lib/moderate.ts",
                    "export function moderate(s: string) { return s; }\n",
                ),
            ],
        )
        .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::http::router(crate::http::AppState {
            pool: pool.clone(),
            sync: crate::store::snapshot::StateSync::new("test"),
            auth: state.auth.clone(),
            mcp: state.clone(),
            site: crate::http::site::SiteDir::new(std::path::Path::new("/nonexistent")),
        });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        (state, pool, format!("http://{addr}"), dir, handle)
    }

    #[test]
    fn demo_slugs_are_sanitized_bounded_and_unique_per_account() {
        assert_eq!(
            demo_slug("review", "demo-notes").unwrap(),
            "review-demo-notes"
        );
        assert_eq!(
            demo_slug("Review.Team+one", "demo-tasks").unwrap(),
            "review-team-one-demo-tasks"
        );
        // A long local part shrinks instead of failing the 48-char rule.
        let long = "a".repeat(80);
        let slug = demo_slug(&long, "demo-notes").unwrap();
        assert!(slug.len() <= 48, "{slug}");
        assert!(slug.ends_with("-demo-notes"), "{slug}");
        // Punctuation-only local parts still produce a valid slug.
        assert_eq!(demo_slug("..", "demo-tasks").unwrap(), "review-demo-tasks");
    }

    #[test]
    fn rpc_request_is_one_tools_call() {
        let request = rpc_request("get_app", &json!({"app": "notes"}));
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], "get_app");
        assert_eq!(request["params"]["arguments"]["app"], "notes");
        assert!(request["id"].is_number());
    }

    #[test]
    fn mcp_endpoint_appends_mcp_once() {
        assert_eq!(mcp_endpoint("https://outer.now"), "https://outer.now/mcp");
        assert_eq!(
            mcp_endpoint("http://outer.localhost:8080/"),
            "http://outer.localhost:8080/mcp"
        );
        assert_eq!(
            mcp_endpoint("https://outer.now/mcp"),
            "https://outer.now/mcp"
        );
        assert_eq!(
            mcp_endpoint("https://outer.now/mcp/"),
            "https://outer.now/mcp"
        );
    }

    #[test]
    fn seed_decision_only_skips_a_live_app_with_its_migration() {
        assert_eq!(decide("s", None, None, false).unwrap(), SeedStep::Create);
        assert_eq!(
            decide("s", Some("owner"), Some("success"), true).unwrap(),
            SeedStep::Skip
        );
        // A failed deploy is retried even when the files are present.
        assert_eq!(
            decide("s", Some("owner"), Some("failed"), true).unwrap(),
            SeedStep::Redeploy
        );
        // A missing demo migration is recommitted even after a success.
        assert_eq!(
            decide("s", Some("owner"), Some("success"), false).unwrap(),
            SeedStep::Redeploy
        );
        assert_eq!(
            decide("s", Some("owner"), None, true).unwrap(),
            SeedStep::Redeploy
        );
        let err = decide("s", Some("collaborator"), Some("success"), true).unwrap_err();
        assert!(format!("{err:#}").contains("another account"), "{err:#}");
    }

    #[tokio::test]
    async fn seed_creates_two_pro_demo_apps_over_mcp_and_a_rerun_does_nothing() {
        let (state, pool, url, _dir, _server) = server().await;
        let logins = vec![login(auth_ts::REVIEW_EMAIL)];
        let seeded = seed_demo(&pool, "2026-10-23", auth_ts::REVIEW_EMAIL, &logins, &url)
            .await
            .unwrap();
        assert_eq!(seeded.len(), 2);
        assert_eq!(
            seeded
                .iter()
                .map(|app| app.slug.as_str())
                .collect::<Vec<_>>(),
            ["review-demo-notes", "review-demo-tasks"]
        );
        assert!(seeded.iter().all(|app| app.url.contains(&app.slug)));

        let (plan, status, verified, terms): (String, String, i64, Option<String>) =
            sqlx::query_as(
                "SELECT plan, planStatus, emailVerified, termsVersion FROM user WHERE email = ?1",
            )
            .bind(auth_ts::REVIEW_EMAIL)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(plan, "pro");
        assert_eq!(status, "active");
        assert_eq!(verified, 1);
        assert_eq!(terms.as_deref(), Some("2026-10-23"));

        let owner: String = sqlx::query_scalar("SELECT id FROM user WHERE email = ?1")
            .bind(auth_ts::REVIEW_EMAIL)
            .fetch_one(&pool)
            .await
            .unwrap();
        let apps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app WHERE owner_id = ?1")
            .bind(&owner)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(apps, 2);
        for app in &seeded {
            let app_owner: String = sqlx::query_scalar("SELECT owner_id FROM app WHERE slug = ?1")
                .bind(&app.slug)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(app_owner, owner);
            // The server ran both deploys; the demo commit carries the
            // migration.
            let bare = crate::git::manifest::ensure_synced(
                &state.store,
                &state.git,
                &state.sync,
                &app.slug,
            )
            .await
            .unwrap();
            let (_, bytes) = crate::git::local::read_blob(&bare, None, DEMO_MIGRATION)
                .await
                .unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("CREATE TABLE"), "{text}");
            let last: String = sqlx::query_scalar(
                "SELECT status FROM deploy WHERE app_id = \
                 (SELECT id FROM app WHERE slug = ?1) ORDER BY created_at DESC LIMIT 1",
            )
            .bind(&app.slug)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(last, "success");
        }
        let deploys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(deploys, 4, "two creates and two demo commits");

        let rerun = seed_demo(&pool, "2026-10-23", auth_ts::REVIEW_EMAIL, &logins, &url)
            .await
            .unwrap();
        assert_eq!(
            rerun
                .iter()
                .map(|app| app.slug.as_str())
                .collect::<Vec<_>>(),
            ["review-demo-notes", "review-demo-tasks"]
        );
        let deploys_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(deploys_after, deploys, "a rerun redeploys nothing");
        // The seed mints one key per run and revokes it, both runs included.
        let live_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apikey WHERE enabled != 0")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(live_keys, 0);
        pool.close().await;
    }

    #[tokio::test]
    async fn seed_recommits_an_app_whose_demo_migration_is_gone() {
        let (_state, pool, url, _dir, _server) = server().await;
        let logins = vec![login(auth_ts::REVIEW_EMAIL)];
        seed_demo(&pool, "2026-10-23", auth_ts::REVIEW_EMAIL, &logins, &url)
            .await
            .unwrap();
        let owner: String = sqlx::query_scalar("SELECT id FROM user WHERE email = ?1")
            .bind(auth_ts::REVIEW_EMAIL)
            .fetch_one(&pool)
            .await
            .unwrap();
        let key = auth::create_api_key(&pool, &owner, Some("test delete"))
            .await
            .unwrap();
        let client = SeedMcp::new(&url, key.key.clone());
        client
            .call(
                "commit",
                json!({
                    "app": "review-demo-notes",
                    "message": "Remove the demo migration for the test",
                    "deletes": [DEMO_MIGRATION],
                }),
            )
            .await
            .unwrap();
        assert!(!client
            .has_demo_migration("review-demo-notes")
            .await
            .unwrap());

        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy")
            .fetch_one(&pool)
            .await
            .unwrap();
        let rerun = seed_demo(&pool, "2026-10-23", auth_ts::REVIEW_EMAIL, &logins, &url)
            .await
            .unwrap();
        assert_eq!(rerun.len(), 2);
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, before + 1, "only the incomplete app is redeployed");
        assert!(client
            .has_demo_migration("review-demo-notes")
            .await
            .unwrap());
        auth::revoke_api_key(&pool, &owner, &key.prefix)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn seed_refuses_a_slug_owned_by_someone_else() {
        let (_state, pool, url, _dir, _server) = server().await;
        let logins = vec![login(auth_ts::REVIEW_EMAIL)];
        let other = apps::ensure_account(&pool, "other@example.com")
            .await
            .unwrap();
        apps::create(
            &pool,
            &other,
            "review-demo-notes",
            Some("Foreign"),
            &Allocation {
                uid_range: (220000, 220001),
                port_range: (19000, 19001),
            },
            5,
        )
        .await
        .unwrap();
        let err = seed_demo(&pool, "2026-10-23", auth_ts::REVIEW_EMAIL, &logins, &url)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("another account"), "{err:#}");
        let live_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apikey WHERE enabled != 0")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(live_keys, 0, "the seed key is revoked after a failure");
        pool.close().await;
    }

    #[tokio::test]
    async fn seed_revokes_its_key_when_the_server_is_unreachable() {
        let (_state, pool, _url, _dir, _server) = server().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let logins = vec![login(auth_ts::REVIEW_EMAIL)];
        let err = seed_demo(&pool, "2026-10-23", auth_ts::REVIEW_EMAIL, &logins, &closed)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("MCP request failed"), "{err:#}");
        let live_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apikey WHERE enabled != 0")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(live_keys, 0, "the seed key is revoked after a failed run");
        pool.close().await;
    }

    #[tokio::test]
    async fn seed_refuses_an_owner_outside_the_review_list() {
        let (_state, pool, url, _dir, _server) = server().await;
        let logins = vec![login(auth_ts::REVIEW_EMAIL)];
        let err = seed_demo(&pool, "2026-10-23", "someone@example.com", &logins, &url)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("not in OUTER_REVIEW_LOGINS"),
            "{err:#}"
        );
        let apps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM app")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(apps, 0, "nothing is seeded for an unlisted owner");
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 0, "the unlisted owner gets no account");
        pool.close().await;
    }
}
