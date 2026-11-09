//! One tool registry: every tool is defined once (name, description, input
//! type, access, `untrusted` flag). `tools/list`, the Blume site and the M3
//! codemode declarations all render from here.

use serde_json::Value;

use crate::mcp::tools::{
    access as access_inputs, account, add_example as add_example_inputs, apps as app_inputs,
    backups as backup_inputs, code as code_inputs, collaborators as collab_inputs,
    data as data_inputs, domains as domain_inputs, drafts as draft_inputs,
    examples as examples_inputs, export as export_inputs, look as look_inputs,
    open_app as open_app_inputs, preview as preview_inputs, runtime as runtime_inputs,
    secrets as secrets_inputs, NoArgs,
};

/// Who may call the tool. `Public` tools run without a token; anything else
/// answers `401` before it runs when the caller is anonymous (spec §6.1).
/// Collaborators (Pro, spec §7.4) may call the read and change tools; the
/// owner-only tools refuse them with `forbidden`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    Account,
}

/// One row of the registry.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub access: Access,
    /// File contents, code matches, logs, patches and query rows can hold
    /// text an app's visitors wrote: results prepend the §8.4 notice as its
    /// own text block.
    pub untrusted: bool,
    pub input_schema: Value,
    /// MCP tool annotations: clients decide from these whether a call needs
    /// the human's confirmation (read-only tools run without asking).
    pub hints: Hints,
}

/// MCP `ToolAnnotations` (2025-06-18). Clients treat a tool without hints
/// as possibly destructive and ask before every call, which turned a first
/// session in Perplexity into a string of "yes" / "yes to all" prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

impl Hints {
    /// Reads Outer's own state and modifies nothing.
    const READ: Self = Self {
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    };
    /// Strictly additive and confined to the account: creates a row, a
    /// backup or a queued review, losing nothing. Repeating the call can
    /// create a second artifact, so it is not idempotent.
    const ADDITIVE: Self = Self {
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    };
    /// Additive, but the result leaves the private account: a public app
    /// URL, an invite email, a 6-digit code mailed to the caller.
    const ADDITIVE_PUBLIC: Self = Self {
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: true,
    };
    /// Changes a reversible flag and nothing else; the same call leaves the
    /// same state, so a retry is a no-op.
    const SETTING: Self = Self {
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    };
    /// Overwrites a stored value or replaces who can reach the app (a
    /// secret, the visitor PIN). The old value is gone, so a retry can
    /// never recover it: destructive, never a no-op.
    const OVERWRITE: Self = Self {
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: false,
    };
    /// Overwrites a setting whose effect is public: publishing an app can
    /// expose it to search engines and crawlers, and renaming it serves the
    /// same app at a new public hostname. The previous setting or URL is
    /// gone, so clients must confirm.
    const OVERWRITE_PUBLIC: Self = Self {
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: true,
    };
    /// Ends access or data for good (a revoked key, a deleted app, a
    /// discarded draft). Repeating it reaches the same end state.
    const DESTRUCTIVE: Self = Self {
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
    };
    /// Deploys code to a live or preview URL: it replaces what serves,
    /// publishes to the open web, and every run creates a new commit.
    const PUBLISH: Self = Self {
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    };
    /// The fallback for a name this table doesn't know: clients must
    /// confirm, and it may reach the open web. The registry test refuses
    /// any unclassified tool, so this never ships.
    const CAUTIOUS: Self = Self {
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    };

    pub fn to_json(self, title: &str) -> Value {
        serde_json::json!({
            "title": title,
            "readOnlyHint": self.read_only,
            "destructiveHint": self.destructive,
            "idempotentHint": self.idempotent,
            "openWorldHint": self.open_world,
        })
    }
}

/// Every tool's hints, by name. `None` for a name not classified here: the
/// registry test fails on it, so a new tool can't ship without a decision.
pub fn hints_for(name: &str) -> Option<Hints> {
    Some(match name {
        // Retrieval and computation only: no row, object or deploy changes.
        // `look_at_app` renders the page in a browser but writes nothing;
        // `share_app` formats a URL and a QR code.
        "about" | "docs" | "whoami" | "list_api_keys" | "list_connections" | "list_apps"
        | "get_app" | "share_app" | "look_at_app" | "open_app" | "list_files" | "read_files"
        | "search_code" | "history" | "show_commit" | "deploy_status" | "deploy_log"
        | "app_logs" | "app_errors" | "app_metrics" | "db_query" | "list_secrets"
        | "list_collaborators" | "list_domains" | "list_backups" | "diff_draft" | "examples" => {
            Hints::READ
        }
        // Each of these commits code and deploys it: it overwrites what the
        // live (or preview) URL serves, publishes to the open web, and
        // every call mints a new commit. `commit {preview: true}` deploys
        // the preview host, which is public too.
        "commit" | "ship" | "rollback" | "promote_preview" | "add_example" => Hints::PUBLISH,
        // Additive writes whose result is visible outside the private
        // account: `create_app` publishes a new app URL, `sign_in` mails a
        // code, `add_collaborator` emails an invite, `add_domain` registers
        // a public hostname.
        "create_app" | "sign_in" | "add_collaborator" | "add_domain" => Hints::ADDITIVE_PUBLIC,
        // Additive writes confined to the account: a new key or row, a
        // staged draft op, a build (which consumes build minutes, so not
        // read-only), a fresh backup, a queued review or triage change,
        // and `export_code` (which persists a new zip in the bucket).
        "verify_code" | "create_api_key" | "export_code" | "stage" | "check"
        | "backup_database" | "request_review" | "resolve_error" | "ignore_error"
        | "reopen_error" => Hints::ADDITIVE,
        // One reversible flag; the same call leaves the same state.
        "set_always_on" => Hints::SETTING,
        // Overwrites a value or replaces who can reach the app: the old
        // secret, PIN or tables are gone, and a retry can't restore them.
        // `set_secret` is the audit's example: non-destructive would let
        // injected text swap a Stripe key without a confirmation.
        "set_secret" | "set_access" | "restore_backup" => Hints::OVERWRITE,
        // Replaces the current visibility or name *and* publishes outside
        // the private account: `set_visibility` can expose the app to
        // search engines and `rename_app` serves it at a new public
        // hostname. Open-world because those pages leave the account,
        // destructive because the previous setting or URL is overwritten.
        "set_visibility" | "rename_app" => Hints::OVERWRITE_PUBLIC,
        // Ends access or data for good: the same call reaches the same end
        // state, so repeating it is a no-op.
        "revoke_api_key"
        | "revoke_connection"
        | "delete_app"
        | "delete_account"
        | "unset_secret"
        | "remove_domain"
        | "remove_collaborator"
        | "discard_preview"
        | "discard_draft" => Hints::DESTRUCTIVE,
        _ => return None,
    })
}

/// `create_app` → `Create app`, for the annotation title.
pub fn title_of(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => words,
    }
}

fn schema_of<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

fn tool<T: schemars::JsonSchema>(
    name: &'static str,
    description: &'static str,
    access: Access,
    untrusted: bool,
) -> ToolDef {
    // Compile-time guard: every registered input must actually derive the
    // schema the registry advertises.
    let input_schema = schema_of::<T>();
    ToolDef {
        name,
        description,
        access,
        untrusted,
        input_schema,
        // Unclassified names fall back to the cautious default; the
        // registry test refuses them, so this never ships.
        hints: hints_for(name).unwrap_or(Hints::CAUTIOUS),
    }
}

/// Every M1 tool, in `tools/list` order.
pub fn registry() -> Vec<ToolDef> {
    use Access::{Account, Public};
    vec![
        tool::<NoArgs>(
            "about",
            "What Outer is, how to connect, and the ground rules. Call it when you have no token.",
            Public, false,
        ),
        tool::<account::DocsInput>(
            "docs",
"Read an Outer agent guide: quickstart, templates, ilha, void, website, celld, d1, storage, backups, kv, auth, cron, queues, websockets, sse, email, sandboxes, limits, moderation, custom-domains, export, previews or look. Read the ilha guide before writing any code.",
            Public, false,
        ),
        tool::<account::SignInInput>(
            "sign_in",
            "Start email sign-in (clients without OAuth): emails a 6-digit code. Then call verify_code. During the closed beta a new email also needs invite_code (OUTER-XXXX-XXXX). Disappears after sign-in.",
            Public, false,
        ),
        tool::<account::VerifyCodeInput>(
            "verify_code",
            "Finish email sign-in: verify the emailed code and get an API key. accept_terms is required on first use. Disappears after sign-in.",
            Public, false,
        ),
        tool::<NoArgs>(
            "whoami",
            "Your account: email, plan, usage, limits and when they reset. deletes_at is your soonest Free-app deletion date for inactivity (null on Pro or with no apps).",
            Account, false,
        ),
        tool::<account::CreateApiKeyInput>(
            "create_api_key",
            "Mint an API key. It is shown once — store it in your MCP config.",
            Account, false,
        ),
        tool::<NoArgs>(
            "list_api_keys",
            "List your API keys: id, name, prefix, use. Secrets are never shown again.",
            Account, false,
        ),
        tool::<account::RevokeApiKeyInput>(
            "revoke_api_key",
            "Revoke an API key by its prefix.",
            Account, false,
        ),
        tool::<NoArgs>(
            "list_connections",
            "List your OAuth grants: which clients (Claude, Codex, ChatGPT) may act as you.",
            Account, false,
        ),
        tool::<account::RevokeConnectionInput>(
            "revoke_connection",
            "Revoke an OAuth grant by id. The client signs in again from scratch.",
            Account, false,
        ),
        tool::<account::DeleteAccountInput>(
            "delete_account",
            "Delete your account: pass confirm equal to your email. Deletes your apps, signs every session out.",
            Account, false,
        ),
        tool::<app_inputs::CreateAppInput>(
            "create_app",
            "Create an app and deploy it: {slug, name?, description?, template?, examples?, wait?}. Examples like [\"passkeys\"] seed working features in the first commit. Name (1-60 chars) and description (1-160 chars) brand the app; both default from the slug. Template \"app\" is an interactive Ilha + Void app, \"website\" an Astro content site (default \"app\"). Waits for the first deploy by default.",
            Account, false,
        ),
        tool::<NoArgs>(
            "list_apps",
            "Your apps: slug, url, role, visibility, status, and deletes_at when a Free app faces deletion for inactivity (absent on Pro).",
            Account, false,
        ),
        tool::<app_inputs::GetAppInput>(
            "get_app",
            "One app: url, role, visibility, status, its last deploy, and deletes_at when a Free app faces deletion for inactivity (absent on Pro).",
            Account, false,
        ),
        tool::<app_inputs::RequestReviewInput>(
            "request_review",
            "Re-crawl a held app and lift the hold when it is clean, or put a blocked deploy or suspended app in the admin review queue.",
            Account, false,
        ),
        tool::<app_inputs::SetVisibilityInput>(
            "set_visibility",
            "Set an app unlisted (default, kept out of search engines) or public (public is a Pro feature).",
            Account, false,
        ),
        tool::<app_inputs::SetAlwaysOnInput>(
            "set_always_on",
            "Keep a Pro app warm {app, on}: on skips the idle sleep and restarts the fleet after restarts (up to the host cap); off lets it sleep on idle. On is a Pro feature; turning it off is always allowed.",
            Account, false,
        ),
        tool::<access_inputs::SetAccessInput>(
            "set_access",
            "Put a visitor gate on your app {app, mode, secret?}: open (default, everyone walks in), pin (4-8 digits) or password (8-128 chars). PIN and password are a Pro feature; opening is always allowed. The gate locks every host and path until the visitor unlocks; changing the secret signs everyone out. Never returns the secret. Owner only.",
            Account, false,
        ),
        tool::<app_inputs::ShareAppInput>(
            "share_app",
            "Share an app: its url, a one-line share text and a QR code.",
            Account, false,
        ),
        tool::<app_inputs::DeleteAppInput>(
            "delete_app",
            "Delete an app forever: stops the fleet, wipes its code, data and files, and purges its cached pages. Pass confirm equal to the slug. The freed name is claimable again immediately.",
            Account, false,
        ),
        tool::<app_inputs::RenameAppInput>(
            "rename_app",
            "Rename an app: {app, slug}. Moves its slug, hostname, code, data and files to the new name (D1 and Durable Object data survive); the old name redirects to the new one until someone claims it. Counts one code change; a running app redeploys.",
            Account, false,
        ),
        tool::<collab_inputs::AddCollaboratorInput>(
            "add_collaborator",
            "Invite a collaborator to your app by email: {app, email}. The address joins the closed beta and gets an invite email; the app shows in their list_apps once they sign in. Owner only; Free 1 collaborator per app, Pro 10.",
            Account, false,
        ),
        tool::<collab_inputs::RemoveCollaboratorInput>(
            "remove_collaborator",
            "Remove a collaborator from your app by email: {app, email}. Their access ends immediately. Owner only.",
            Account, false,
        ),
        tool::<collab_inputs::ListCollaboratorsInput>(
            "list_collaborators",
            "List your app's collaborators: email, joined or pending status, and when each was added. Owner only.",
            Account, false,
        ),
        tool::<domain_inputs::AddDomainInput>(
            "add_domain",
            "Give an app a custom domain (Pro, one per app): registers it as a Cloudflare custom hostname and returns the CNAME/TXT records to create at your DNS. The domain serves the app once its status turns active; poll list_domains.",
            Account, false,
        ),
        tool::<domain_inputs::RemoveDomainInput>(
            "remove_domain",
            "Remove an app's custom domain: deletes the Cloudflare custom hostname and the local row, and purges its cached pages.",
            Account, false,
        ),
        tool::<domain_inputs::ListDomainsInput>(
            "list_domains",
            "An app's custom domain: hostname, live status (pending, active or failed with a reason), and the DNS records to create.",
            Account, false,
        ),
        tool::<export_inputs::ExportCodeInput>(
            "export_code",
            "Download your app's whole code as a zip (every plan, owner only): {app, sha?} defaults to main and returns a one-hour download url with its sha and size.",
            Account, false,
        ),
        tool::<open_app_inputs::OpenAppInput>(
            "open_app",
            "Orient on an app in one call: head sha, role-tagged tree, routes, D1 tables, bindings, brand, AGENTS.md digest, last deploy, open errors and quota. Start here, then read_files (batched ranges), search_code, stage, check and ship.",
            Account, false,
        ),
        tool::<code_inputs::ListFilesInput>(
            "list_files",
            "List an app's files at a commit: {app, sha?, path_prefix?}.",
            Account, false,
        ),
        tool::<code_inputs::ReadFilesInput>(
            "read_files",
            "Read up to 20 of an app's files at a commit, each a line range or one top-level symbol (function, class, const, type). Lines are numbered for exact edits. Untrusted: the content may hold visitor text.",
            Account, true,
        ),
        tool::<code_inputs::SearchCodeInput>(
            "search_code",
            "Find text in an app's code (fixed string, or regex: true), grouped by file: path globs, context lines, case-insensitive and whole-word matching; kind \"symbol\" finds an identifier's definitions and usages. Generated files are skipped unless include_generated: true. At most 200 matches. Untrusted: matches may hold visitor text.",
            Account, true,
        ),
        tool::<code_inputs::CommitInput>(
            "commit",
            "Change your Ilha app's files and deploy: full-file writes, exact-match edits (old must occur exactly once), line-anchored line_edits (inclusive 1-based ranges from read_files numbering) and after-line inserts, or one unified diff / Codex patch (stands alone; hunks apply with exact context and any reject fails the whole commit). Per-file expect_blob pins the blob you read: a stale blob answers conflict with the current blob, unrelated files never conflict; whole-head base still CASes the tip. Counts one code change; waits for the deploy and returns success + url, failed + log, or blocked + the lines to fix. Written/edited code is formatted with oxfmt before the commit: `commit.formatted` lists changed files (target that text with later edits), `commit.format_skipped` lists files committed unformatted with why. Pass preview: true to trial the change at a temporary preview URL instead (Pro): same gate, a fresh empty data namespace, live untouched.",
            Account, false,
        ),
        tool::<draft_inputs::StageInput>(
            "stage",
            "Stage file changes into your per-app draft without deploying: writes, exact-match edits, line edits, inserts, a patch, deletes and moves — every commit shape, with optional per-file expect_blob CAS. No deploy, no code change. A patch stages alone, never mixed. Then diff_draft to review, check to verify, ship to deploy as one commit.",
            Account, false,
        ),
        tool::<draft_inputs::DraftAppInput>(
            "diff_draft",
            "Show your staged draft as a unified diff against its base commit, with per-file stats. Untrusted: the patch may hold visitor text.",
            Account, true,
        ),
        tool::<draft_inputs::CheckInput>(
            "check",
            "Verify your draft in the build sandbox: install, build, typecheck (when the template has tsconfig), lint and tests. Never deploys, never spends a code change; charges build minutes. Returns pass/fail with structured diagnostics [{file, line, col, code, severity, message}].",
            Account, false,
        ),
        tool::<draft_inputs::ShipInput>(
            "ship",
            "Ship your draft as one commit through the normal path (gate, deploy, one code change) and clear it. Rebases onto the current head when it moved, or answers conflict with current_sha when the draft no longer applies.",
            Account, false,
        ),
        tool::<draft_inputs::DiscardDraftInput>(
            "discard_draft",
            "Drop your staged draft. The deployed app is untouched.",
            Account, false,
        ),
        tool::<code_inputs::HistoryInput>(
            "history",
            "An app's commits, newest first, with the next_skip cursor.",
            Account, false,
        ),
        tool::<code_inputs::ShowCommitInput>(
            "show_commit",
            "One commit: metadata, changed files and the unified patch. Untrusted: the patch may hold visitor text.",
            Account, true,
        ),
        tool::<code_inputs::RollbackInput>(
            "rollback",
            "Redeploy an earlier successfully deployed commit: a new commit copying its tree, then the deploy. Never counts as a code change.",
            Account, false,
        ),
        tool::<preview_inputs::PromotePreviewInput>(
            "promote_preview",
            "Promote your app's preview to live: fast-forwards main to the preview commit and deploys it normally. Needs a successful preview deploy; Pro. The preview goes away and the URL serves the promoted version.",
            Account, false,
        ),
        tool::<preview_inputs::DiscardPreviewInput>(
            "discard_preview",
            "Discard your app's preview: stops the preview fleet and removes its trial state. Live is untouched, and cleanup is always allowed, even off Pro.",
            Account, false,
        ),
        tool::<runtime_inputs::DeployStatusInput>(
            "deploy_status",
            "A deploy's status: success + url, failed + step and error, blocked + findings, or building + next. wait: true blocks until it finishes. Pass preview: true for preview runs.",
            Account, false,
        ),
        tool::<runtime_inputs::DeployLogInput>(
            "deploy_log",
            "A deploy's build log. Untrusted: the log may hold visitor text. Pass preview: true for preview runs.",
            Account, true,
        ),
        tool::<runtime_inputs::AppLogsInput>(
            "app_logs",
            "An app's recent log lines from the fleet ring, oldest first; pass back next_since to poll. Untrusted: lines may hold visitor text.",
            Account, true,
        ),
        tool::<runtime_inputs::AppErrorsInput>(
            "app_errors",
            "An app's grouped runtime errors, newest first: ERROR lines and uncaught-exception stacks from the log stream, grouped by fingerprint, plus proxy 5xx over the last hour. Pass status to filter open, resolved or ignored issues. Untrusted: messages and frames may hold visitor text.",
            Account, true,
        ),
        tool::<runtime_inputs::ResolveErrorInput>(
            "resolve_error",
            "Mark one grouped runtime error {app, fingerprint} resolved: it leaves the open list, and a new event for the same fingerprint reopens it as a regression.",
            Account, false,
        ),
        tool::<runtime_inputs::IgnoreErrorInput>(
            "ignore_error",
            "Ignore one grouped runtime error {app, fingerprint}: it keeps counting but stays out of the open list and never reopens automatically.",
            Account, false,
        ),
        tool::<runtime_inputs::ReopenErrorInput>(
            "reopen_error",
            "Reopen one resolved or ignored error issue {app, fingerprint}: it returns to the open list as a fresh triage.",
            Account, false,
        ),
        tool::<data_inputs::AppMetricsInput>(
            "app_metrics",
            "An app's traffic over 24, 168 or 720 hours (default 24): requests, unique visitors, errors, p50/p95 latency, top paths and the 4xx/5xx split. Same source as the Console metrics tab. Untrusted: paths may hold visitor text.",
            Account, true,
        ),
        tool::<data_inputs::DbQueryInput>(
            "db_query",
            "Read an app's live D1 with one SELECT, WITH … SELECT or PRAGMA table_info (? placeholders come from params); at most 200 rows. Read-only: writes ship as migrations in your next commit. Untrusted: rows may hold visitor data.",
            Account, true,
        ),
        tool::<look_inputs::LookAtAppInput>(
            "look_at_app",
            "Look at your app's live page: renders it in a real browser and returns a screenshot plus its HTTP status and title, console errors and failed requests. Call after each change to verify your work. Untrusted: the screenshot and console entries render app content. When the console recorder cannot run (blocking CSP, oversize page), the note says why: use app_logs and app_errors for those cases.",
            Account, true,
        ),
 tool::<secrets_inputs::ListSecretsInput>(
 "list_secrets",
 "An app's secret names and when each was set. Values are never shown: they reach the app as Worker env vars on the next deploy.",
 Account, false,
 ),
 tool::<secrets_inputs::SetSecretInput>(
 "set_secret",
 "Set an app secret {app, key, value}: encrypted at rest, injected as a Worker env var on the next deploy. Names use uppercase letters, digits and _, at most 64 characters, and at most 50 secrets fit per app.",
 Account, false,
 ),
        tool::<secrets_inputs::UnsetSecretInput>(
            "unset_secret",
            "Remove an app secret {app, key}. Applies on the next deploy.",
            Account, false,
        ),
        tool::<backup_inputs::ListBackupsInput>(
            "list_backups",
            "An app's D1 backups: id, binding, when each dump was taken and its size. Daily dumps are automatic for active apps (a request or successful deploy in the prior 24 h; newest 3 kept, Pro 30); restore one with restore_backup, download one from Console → D1 → Backups. Owner only.",
            Account, false,
        ),
        tool::<backup_inputs::BackupDatabaseInput>(
            "backup_database",
            "Back up an app's D1 database now: {app, binding?} (binding defaults to the single declared binding). Returns the stored backup id, time and size. Owner only.",
            Account, false,
        ),
        tool::<backup_inputs::RestoreBackupInput>(
            "restore_backup",
            "Restore an app's D1 database from a backup: {app, binding?, backup_id, confirm} with confirm equal to backup_id. Takes a fresh backup first, then replaces the live tables with the dump. Owner only.",
            Account, false,
        ),
        tool::<examples_inputs::ExamplesInput>(
            "examples",
            "Ready features for apps and sites: `examples` lists them, `examples {name}` shows the README and file list (contents with `files: true`). Apply one with `add_example`.",
            Public, false,
        ),
        tool::<add_example_inputs::AddExampleInput>(
            "add_example",
            "Add a ready feature to your app: {app, name} (+ optional wait). Commits the example's pages, nav, bindings and migrations in one change and deploys. Customise with commit after.",
            Account, false,
        ),
    ]
}

/// The tools `tools/list` shows this caller: anonymous callers see every
/// non-admin tool (so the model can pick `create_app` first), signed-in
/// callers no longer see `sign_in`/`verify_code`, admins see the admin
/// additions (none in M1).
pub fn visible_tools(authenticated: bool) -> Vec<ToolDef> {
    registry()
        .into_iter()
        .filter(|t| {
            if !authenticated {
                return true;
            }
            !matches!(t.name, "sign_in" | "verify_code")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_shapes() {
        let tools = registry();
        assert_eq!(tools.len(), 61);
    }

    /// Commerce guidelines (owner decision D2): no tool description
    /// advertises a price, a discount, a promotion, a checkout or a coupon.
    /// "Pro feature" statements stay allowed.
    #[test]
    fn no_description_promotes_commerce() {
        for tool in registry() {
            let text = tool.description.to_lowercase();
            for banned in ["$", "checkout", "discount", "launch price", "coupon"] {
                assert!(
                    !text.contains(banned),
                    "{} description mentions {banned:?}: {}",
                    tool.name,
                    tool.description
                );
            }
        }
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    /// Every tool has a decision, no tool is both read-only and
    /// destructive, untrusted tools never change state, and the pinned
    /// destructive set matches the handlers' actual effects.
    #[test]
    fn every_tool_is_classified_and_the_destructive_set_is_pinned() {
        for tool in registry() {
            let hints = hints_for(tool.name)
                .unwrap_or_else(|| panic!("{} has no annotation hints", tool.name));
            assert!(
                !(hints.read_only && hints.destructive),
                "{} can't be both read-only and destructive",
                tool.name
            );
            let destructive = matches!(
                tool.name,
                "revoke_api_key"
                    | "revoke_connection"
                    | "delete_app"
                    | "delete_account"
                    | "unset_secret"
                    | "remove_collaborator"
                    | "remove_domain"
                    | "discard_preview"
                    | "discard_draft"
                    | "restore_backup"
                    | "set_secret"
                    | "set_access"
                    | "rename_app"
                    | "set_visibility"
                    | "commit"
                    | "ship"
                    | "rollback"
                    | "promote_preview"
                    | "add_example"
            );
            assert_eq!(hints.destructive, destructive, "{}", tool.name);
            // Untrusted tools return app content; they must never change state.
            if tool.untrusted {
                assert!(
                    hints.read_only,
                    "{} is untrusted but not read-only",
                    tool.name
                );
            }
        }
        assert!(hints_for("list_apps").is_some_and(|h| h.read_only));
        // Every restore takes a fresh backup first, so a retry is never a
        // no-op: destructive but not idempotent.
        assert!(hints_for("restore_backup").is_some_and(|h| h.destructive && !h.idempotent));
        assert_eq!(title_of("create_app"), "Create app");
    }

    /// The classes pinned by what a caller would notice: a live deploy is
    /// open-world and destructive (except `create_app`, which adds an app
    /// rather than replacing one), every `delete_`/`remove_`/`revoke_`/
    /// `unset_`/`discard_` tool is destructive, and a read-only tool never
    /// claims an effect.
    #[test]
    fn hints_follow_the_handlers_effects() {
        // These can deploy live code; the owner's site changes.
        for name in [
            "commit",
            "ship",
            "rollback",
            "promote_preview",
            "add_example",
            "create_app",
        ] {
            let hints = hints_for(name).unwrap_or_else(|| panic!("{name} is unclassified"));
            assert!(!hints.read_only, "{name} deploys but reads as read-only");
            assert!(hints.open_world, "{name} deploys but reads as closed-world");
            if name == "create_app" {
                // A new app overwrites nothing.
                assert!(!hints.destructive, "create_app adds an app");
            } else {
                assert!(hints.destructive, "{name} replaces what is live");
            }
        }
        for tool in registry() {
            let hints =
                hints_for(tool.name).unwrap_or_else(|| panic!("{} unclassified", tool.name));
            if tool.name.starts_with("delete_")
                || tool.name.starts_with("remove_")
                || tool.name.starts_with("revoke_")
                || tool.name.starts_with("unset_")
                || tool.name.starts_with("discard_")
            {
                assert!(hints.destructive, "{} removes something", tool.name);
            }
            if hints.read_only {
                assert!(
                    !hints.destructive,
                    "{} is read-only and destructive",
                    tool.name
                );
                assert!(
                    !hints.open_world,
                    "{} is read-only and open-world",
                    tool.name
                );
            }
        }
        // The audit's example: a silent `set_secret` would let injected text
        // swap a Stripe key without the human ever seeing a prompt.
        assert!(hints_for("set_secret").is_some_and(|h| h.destructive && !h.read_only));
        // `rename_app` retires the old URL and serves the app at a new
        // public hostname, so it is open-world as well as destructive.
        assert!(hints_for("rename_app").is_some_and(|h| h.destructive && h.open_world));
    }
}
