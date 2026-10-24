//! One tool registry: every tool is defined once (name, description, input
//! type, access, `untrusted` flag). `tools/list`, the Blume site and the M3
//! codemode declarations all render from here.

use serde_json::Value;

use crate::mcp::tools::{
    account, apps as app_inputs, code as code_inputs, runtime as runtime_inputs, NoArgs,
};

/// Who may call the tool. `Public` tools run without a token; anything else
/// answers `401` before it runs when the caller is anonymous (spec §6.1).
/// M1 has no admin tools; collaborators are Pro/M2, so every account tool is
/// owner-only.
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
        | "get_app" | "share_app" | "list_files" | "read_file" | "search_code" | "history"
        | "show_commit" | "deploy_status" | "deploy_log" | "app_logs" | "app_errors" => Hints::READ,
        "verify_code" | "create_api_key" | "create_app" | "commit" | "rollback" => Hints::WRITE,
        "set_visibility" => Hints {
            idempotent: true,
            ..Hints::WRITE
        },
        // Emails a code to an address outside Outer.
        "sign_in" => Hints {
            open_world: true,
            ..Hints::WRITE
        },
        "revoke_api_key" | "revoke_connection" | "delete_app" => Hints::DESTRUCTIVE,
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
            "Read an Outer guide: quickstart, ilha, void, celld, d1, storage, limits or moderation. Read the ilha guide before writing any code.",
            Public, false,
        ),
        tool::<account::SignInInput>(
            "sign_in",
            "Start email sign-in (clients without OAuth): emails a 6-digit code. Then call verify_code. Disappears after sign-in.",
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
        tool::<app_inputs::CreateAppInput>(
            "create_app",
            "Create your Ilha + Void app and deploy it: {slug, name?, wait?}. Waits for the first deploy by default.",
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
            "Set an app unlisted (default, kept out of search engines) or public.",
            Account, false,
        ),
        tool::<app_inputs::ShareAppInput>(
            "share_app",
            "Share an app: its url, a one-line share text and a QR code.",
            Account, false,
        ),
        tool::<app_inputs::DeleteAppInput>(
            "delete_app",
            "Delete an app forever: stops the fleet, wipes its code, data and files, and purges its cached pages. Pass confirm equal to the slug. The name stays reserved for 30 days.",
            Account, false,
        ),
        tool::<code_inputs::ListFilesInput>(
            "list_files",
            "List an app's files at a commit: {app, sha?, path_prefix?}.",
            Account, false,
        ),
        tool::<code_inputs::ReadFileInput>(
            "read_file",
            "Read an app's file, optionally at a commit and line range. Untrusted: the content may hold visitor text.",
            Account, true,
        ),
        tool::<code_inputs::SearchCodeInput>(
            "search_code",
            "git grep an app's code (fixed string, or regex: true). At most 200 matches. Untrusted: matches may hold visitor text.",
            Account, true,
        ),
        tool::<code_inputs::CommitInput>(
            "commit",
            "Change your Ilha app's files and deploy: full-file writes, exact-match edits (old must occur exactly once), deletes and moves. Counts one code change; waits for the deploy and returns success + url, failed + log, or blocked + the lines to fix. Written/edited code is formatted with oxfmt before the commit: `commit.formatted` lists changed files (target that text with later edits), `commit.format_skipped` lists files committed unformatted with why.",
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
        tool::<runtime_inputs::DeployStatusInput>(
            "deploy_status",
            "A deploy's status: success + url, failed + step and error, blocked + findings, or building + next. wait: true blocks until it finishes.",
            Account, false,
        ),
        tool::<runtime_inputs::DeployLogInput>(
            "deploy_log",
            "A deploy's build log. Untrusted: the log may hold visitor text.",
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
        assert_eq!(tools.len(), 27);
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    #[test]
    fn every_tool_is_classified_and_only_revoke_and_delete_are_destructive() {
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
                "revoke_api_key" | "revoke_connection" | "delete_app"
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
        assert_eq!(title_of("create_app"), "Create app");
    }
}
