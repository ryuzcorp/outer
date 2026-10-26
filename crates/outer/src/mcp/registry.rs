//! One tool registry: every tool is defined once (name, description, input
//! type, access, `untrusted` flag). `tools/list`, the Blume site and the M3
//! codemode declarations all render from here.

use serde_json::Value;

use crate::mcp::tools::{
    account, apps as app_inputs, backups as backup_inputs, code as code_inputs,
    collaborators as collab_inputs, domains as domain_inputs, export as export_inputs,
    look as look_inputs, preview as preview_inputs, runtime as runtime_inputs,
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
    /// Reads Outer's own state only.
    const READ: Self = Self {
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    };
    /// Adds or changes state that the next call (or `rollback`) can undo:
    /// commits append history, nothing is lost.
    const WRITE: Self = Self {
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    };
    /// Ends access for good (a revoked key or connection can't come back).
    const DESTRUCTIVE: Self = Self {
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
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
        "about" | "docs" | "whoami" | "list_api_keys" | "list_connections" | "list_apps"
        | "get_app" | "share_app" | "look_at_app" | "list_files" | "read_files" | "search_code"
        | "history" | "show_commit" | "deploy_status" | "deploy_log" | "app_logs"
        | "app_errors" | "list_secrets" | "list_collaborators" | "list_domains" | "export_code"
        | "list_backups" => Hints::READ,
        "verify_code" | "create_api_key" | "create_app" | "commit" | "rollback" | "rename_app"
        | "set_secret" | "resolve_error" | "ignore_error" | "reopen_error" | "add_domain"
        | "backup_database" | "promote_preview" | "upgrade" | "manage_billing" => Hints::WRITE,
        // Emails an invite to an address outside Outer.
        "add_collaborator" => Hints {
            open_world: true,
            ..Hints::WRITE
        },
        "remove_collaborator" => Hints::DESTRUCTIVE,
        "set_visibility" | "set_always_on" => Hints {
            idempotent: true,
            ..Hints::WRITE
        },
        // Emails a code to an address outside Outer.
        "sign_in" => Hints {
            open_world: true,
            ..Hints::WRITE
        },
        "revoke_api_key" | "revoke_connection" | "delete_app" | "delete_account"
        | "unset_secret" | "remove_domain" | "discard_preview" => Hints::DESTRUCTIVE,
        // Repeating a restore takes another fresh backup, so one call is
        // never a no-op repeat of the last: not idempotent.
        "restore_backup" => Hints {
            idempotent: false,
            ..Hints::DESTRUCTIVE
        },
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
        hints: hints_for(name).unwrap_or(Hints::DESTRUCTIVE),
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
            "Your account: email, plan, usage, limits and when they reset.",
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
        tool::<NoArgs>(
            "upgrade",
            "Upgrade to Outer Pro: returns a Checkout link for the human to open (Pro is $8/month, with a $4/month launch price while the promotion runs; tax calculated). The plan lifts on the next call after payment.",
            Account, false,
        ),
        tool::<NoArgs>(
            "manage_billing",
            "Manage Outer Pro billing: returns the Customer Portal link to update the card or cancel. Needs an existing subscription — call upgrade first.",
            Account, false,
        ),
        tool::<app_inputs::CreateAppInput>(
            "create_app",
            "Create an app and deploy it: {slug, name?, description?, template?, wait?}. Name (1-60 chars) and description (1-160 chars) brand the app's brand.json; both default from the slug. Template \"app\" is an interactive Ilha + Void app, \"website\" an Astro content site (default \"app\"). Waits for the first deploy by default.",
            Account, false,
        ),
        tool::<NoArgs>(
            "list_apps",
            "Your apps: slug, url, role, visibility and status.",
            Account, false,
        ),
        tool::<app_inputs::GetAppInput>(
            "get_app",
            "One app: url, role, visibility, status and its last deploy.",
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
            "Invite a collaborator to your Pro app by email: {app, email}. The address joins the closed beta and gets an invite email; the app shows in their list_apps once they sign in. Owner only; at most 10 collaborators per app.",
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
            "Download your app's whole code as a zip (Pro, owner only): {app, sha?} defaults to main and returns a one-hour download url with its sha and size.",
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
            "Change your Ilha app's files and deploy: full-file writes, exact-match edits (old must occur exactly once), deletes and moves. Counts one code change; waits for the deploy and returns success + url, failed + log, or blocked + the lines to fix. Written/edited code is formatted with oxfmt before the commit: `commit.formatted` lists changed files (target that text with later edits), `commit.format_skipped` lists files committed unformatted with why. Pass preview: true to trial the change at a temporary preview URL instead (Pro): same gate, a fresh empty data namespace, live untouched.",
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
 "Set an app secret {app, key, value}: encrypted at rest, injected as a Worker env var on the next deploy. Names match ^[A-Z_][A-Z0-9_]{0,63}$, at most 50 per app.",
 Account, false,
 ),
        tool::<secrets_inputs::UnsetSecretInput>(
            "unset_secret",
            "Remove an app secret {app, key}. Applies on the next deploy.",
            Account, false,
        ),
        tool::<backup_inputs::ListBackupsInput>(
            "list_backups",
            "An app's D1 backups: id, binding, when each dump was taken and its size. Daily dumps are automatic for active apps (a request or successful deploy in the prior 24 h; 7 days kept, Pro 30); restore one with restore_backup, download one from Console → D1 → Backups. Owner only.",
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
        assert_eq!(tools.len(), 51);
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    #[test]
    fn every_tool_is_classified_and_only_revoke_delete_unset_remove_discard_and_restore_are_destructive(
    ) {
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
                    | "restore_backup"
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
        assert!(hints_for("commit").is_some_and(|h| !h.read_only && !h.destructive));
        assert!(hints_for("list_apps").is_some_and(|h| h.read_only));
        // Every restore takes a fresh backup first, so a retry is never a
        // no-op: destructive but not idempotent.
        assert!(hints_for("restore_backup").is_some_and(|h| h.destructive && !h.idempotent));
        assert_eq!(title_of("create_app"), "Create app");
    }
}
