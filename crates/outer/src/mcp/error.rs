//! Domain errors as MCP `isError` results (spec §8.3).
//!
//! Transport and auth errors stay HTTP-level (JSON-RPC codes, the lazy-auth
//! `401`); everything a tool refuses becomes `200` with `isError: true`, one
//! text block and `structuredContent` carrying `{error, message,
//! user_message?, …}`. Deploy outcomes are never tool errors: a failed or
//! blocked deploy still returns normally from `commit`.

use serde_json::{json, Value};

/// One refused tool call. `code` is always in the spec §8.3 set; `extra`
/// carries the code's payload (`current_sha`, quota numbers, `retry_after_s`).
#[derive(Debug, Clone)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub user_message: Option<String>,
    pub extra: serde_json::Map<String, Value>,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            user_message: None,
            extra: serde_json::Map::new(),
        }
    }

    pub fn with_user(mut self, message: impl Into<String>) -> Self {
        self.user_message = Some(message.into());
        self
    }

    pub fn with_extra(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.to_string(), value);
        self
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("forbidden", message)
    }

    pub fn payment_required(message: impl Into<String>, user_message: impl Into<String>) -> Self {
        Self::new("payment_required", message).with_user(user_message)
    }
    pub fn conflict(message: impl Into<String>, current_sha: &str) -> Self {
        Self::new("conflict", message).with_extra("current_sha", json!(current_sha))
    }

    pub fn quota_exceeded(
        message: impl Into<String>,
        user_message: impl Into<String>,
        quota: &str,
        limit: u64,
        used: u64,
        resets_at: Option<&str>,
    ) -> Self {
        let mut err = Self::new("quota_exceeded", message)
            .with_user(user_message)
            .with_extra("quota", json!(quota))
            .with_extra("limit", json!(limit))
            .with_extra("used", json!(used));
        if let Some(resets_at) = resets_at {
            err = err.with_extra("resets_at", json!(resets_at));
        }
        err
    }

    pub fn rate_limited(message: impl Into<String>, retry_after_s: u64) -> Self {
        Self::new("rate_limited", message).with_extra("retry_after_s", json!(retry_after_s))
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    /// Fold the auth module's domain errors into the §8.3 code set. Messages
    /// and `user_message`s pass through untouched; only the code is mapped,
    /// and infrastructure failures become a generic `internal` (the detail
    /// goes to the server log, never to the agent).
    pub fn from_auth(err: crate::auth::AuthError) -> Self {
        let message = err.message.clone();
        let user_message = err.user_message.clone();
        let mut out = match err.code {
            "rate_limited" => Self::rate_limited(message, err.retry_after_s.unwrap_or(60)),
            "forbidden" | "closed_beta" => Self::forbidden(message),
            "invalid_email" | "invalid_code" | "too_many_attempts" | "code_expired"
            | "invalid_token" | "terms_required" | "invite_used" => Self::invalid_argument(message),
            _ => {
                tracing::warn!(code = err.code, message = %err.message, "auth module error");
                Self::internal("sign-in failed on our side; try again in a minute")
            }
        };
        if out.user_message.is_none() {
            out.user_message = user_message;
        }
        out
    }

    /// Map commit failures to agent-actionable errors. Edit mismatches name
    /// the path and the match count (spec §9.2); a moved head carries
    /// `current_sha` (spec §8.3).
    pub fn from_commit(err: crate::git::CommitError) -> Self {
        use crate::git::CommitError as E;
        match err {
            E::Conflict { current_sha } => Self::conflict(
                format!("tip moved under this commit: main is {current_sha}"),
                &current_sha,
            )
            .with_user(format!(
                "Someone changed the code since you read it. Read the latest files and retry with base {current_sha}."
            )),
            E::UnknownBase { base, current } => Self::conflict(
                format!("unknown base {base:?}; main is {current}"),
                &current,
            ),
            E::PreviewConflict { current_sha } => Self::conflict(
                format!("tip moved under this preview commit: preview is {current_sha}"),
                &current_sha,
            )
            .with_user(format!(
                "Someone changed the preview since you read it. Read the latest files and retry with base {current_sha}."
            )),
            E::UnknownPreviewBase { base, current } => Self::conflict(
                format!("unknown preview base {base:?}; preview is {current}"),
                &current,
            ),
            E::Internal(e) => {
                tracing::warn!("commit internal: {e:#}");
                Self::internal("the commit failed on our side; try again in a minute")
            }
            other => Self::invalid_argument(other.to_string()),
        }
    }

    /// Map app-creation failures. A taken slug is the agent's to fix, so it
    /// is `invalid_argument`, not `conflict` (no `current_sha` exists).
    pub fn from_create(err: crate::apps::CreateError, plan: &str) -> Self {
        use crate::apps::CreateError as E;
        match err {
            E::InvalidSlug(e) => Self::invalid_argument(e.to_string()),
            E::Profanity(err) => Self::from_profanity(err),
            E::Taken => Self::invalid_argument("slug is taken or quarantined; choose another slug"),
            E::Exhausted(what) => {
                Self::internal(format!("no free {what} left; ask the operator to widen it"))
            }
            E::Quota { limit, used } => Self::quota_exceeded(
                format!("app quota: {limit} allowed, {used} owned"),
                crate::mcp::tools::app_slots_message(plan, limit),
                "apps_owned",
                limit,
                used,
                None,
            ),
            E::Internal(e) => {
                tracing::warn!("create_app internal: {e:#}");
                Self::internal("creating the app failed on our side; try again in a minute")
            }
        }
    }

    /// Map rename failures. A taken or quarantined slug is the agent's to
    /// fix, so it is `invalid_argument` exactly like creation's.
    pub fn from_rename(err: crate::apps::rename::RenameError) -> Self {
        use crate::apps::rename::RenameError as E;
        match err {
            E::NotFound(slug) => Self::not_found(format!("no app named {slug:?}")),
            E::SameSlug(slug) => {
                Self::invalid_argument(format!("the app is already named {slug:?}"))
            }
            E::InvalidSlug(e) => Self::invalid_argument(e.to_string()),
            E::Profanity(err) => Self::from_profanity(err),
            E::Taken => Self::invalid_argument("slug is taken or quarantined; choose another slug"),
            E::PreviewActive(slug) => Self::invalid_argument(format!(
                "discard the preview of {slug:?} before renaming it: trial state does not move"
            )),
            E::Internal(e) => {
                tracing::warn!("rename_app internal: {e:#}");
                Self::internal("renaming the app failed on our side; try again in a minute")
            }
        }
    }

    /// Map a slug/name check failure. A flagged name is the agent's to fix
    /// (`invalid_argument`); an unreachable Jev fails closed with a retryable
    /// `internal`, exactly like the gate's `review_unavailable`.
    pub fn from_profanity(err: crate::apps::ProfanityError) -> Self {
        match err {
            crate::apps::ProfanityError::Flagged(text) => Self::invalid_argument(format!(
                "the name {text:?} was flagged by the content check; choose another name"
            )),
            crate::apps::ProfanityError::Unavailable => {
                Self::internal("Couldn't check this name right now; try again in a minute")
            }
        }
    }

    /// The `isError` result object.
    pub fn into_result(self) -> Value {
        let mut structured = serde_json::Map::new();
        structured.insert("error".to_string(), json!(self.code));
        structured.insert("message".to_string(), json!(self.message));
        if let Some(user_message) = self.user_message {
            structured.insert("user_message".to_string(), json!(user_message));
        }
        structured.extend(self.extra);
        let text = match structured.get("user_message").and_then(Value::as_str) {
            Some(user) => format!("{} {}", self.message, user),
            None => self.message.clone(),
        };
        json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": structured,
            "isError": true,
        })
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}
