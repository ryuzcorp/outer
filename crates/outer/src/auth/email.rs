//! Sign-in codes by email through Cloudflare Email Service.
//!
//! The sender reuses the `CLOUDFLARE_ACCOUNT_ID` + `CLOUDFLARE_API_TOKEN`
//! pair the config already reads: no extra secret. On localhost without the
//! pair the code is logged at INFO instead of emailed. Tests capture codes
//! in memory behind `#[cfg(test)]`.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::{Arc, Mutex};

/// `Outer <hello@outer.now>`: the display name plus the sending address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailFrom {
    pub name: String,
    pub address: String,
}

impl EmailFrom {
    /// Parse `Name <addr>` or a bare address. A bare address takes the
    /// local part as the display name.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if let Some((name, rest)) = raw.split_once('<') {
            let name = name.trim().trim_matches('"').to_string();
            let address = rest.trim_end_matches('>').trim().to_string();
            if address.is_empty() {
                return Self::bare(raw);
            }
            let name = if name.is_empty() {
                address.clone()
            } else {
                name
            };
            Self { name, address }
        } else {
            Self::bare(raw)
        }
    }

    fn bare(raw: &str) -> Self {
        let address = raw.to_string();
        let name = raw
            .split('@')
            .next()
            .filter(|local| !local.is_empty())
            .unwrap_or(raw)
            .to_string();
        Self { name, address }
    }
}

/// How one send failed. 429 (Cloudflare code 10004) is retryable; anything
/// else, including a permanent bounce, is not. The API token never appears
/// in either variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailError {
    Retryable(String),
    Permanent(String),
}

impl std::fmt::Display for EmailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(msg) | Self::Permanent(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for EmailError {}

/// The code sender. `Cloudflare` hits the Email Sending REST API;
/// `Log` prints the code for localhost dev; `Capture` keeps codes in
/// memory for deterministic tests.
#[derive(Clone)]
pub enum EmailSender {
    Cloudflare {
        client: reqwest::Client,
        api_base: String,
        account_id: String,
        api_token: String,
        from: EmailFrom,
    },
    Log,
    #[cfg(test)]
    Capture {
        codes: Arc<Mutex<HashMap<String, String>>>,
    },
}

impl std::fmt::Debug for EmailSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cloudflare { from, .. } => {
                f.debug_struct("Cloudflare").field("from", from).finish()
            }
            Self::Log => write!(f, "Log"),
            #[cfg(test)]
            Self::Capture { .. } => write!(f, "Capture"),
        }
    }
}

impl EmailSender {
    /// The production sender: Cloudflare Email Service on
    /// `https://api.cloudflare.com`.
    pub fn cloudflare(account_id: String, api_token: String, from: EmailFrom) -> Self {
        Self::Cloudflare {
            client: reqwest::Client::new(),
            api_base: "https://api.cloudflare.com".to_string(),
            account_id,
            api_token,
            from,
        }
    }

    /// Point the sender at a fake API base (tests only).
    #[cfg(test)]
    pub fn with_api_base(self, api_base: String) -> Self {
        match self {
            Self::Cloudflare {
                client,
                account_id,
                api_token,
                from,
                ..
            } => Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
            },
            other => other,
        }
    }

    /// Send one sign-in code. The code itself goes in the body; failures
    /// name the reason, never the token.
    pub async fn send_code(&self, to: &str, code: &str) -> Result<(), EmailError> {
        match self {
            Self::Cloudflare {
                client,
                api_base,
                account_id,
                api_token,
                from,
            } => {
                let url = format!("{api_base}/client/v4/accounts/{account_id}/email/sending/send");
                let subject = format!("Your Outer sign-in code is {code}");
                let text = format!(
                    "Your Outer sign-in code is {code}. It expires in 10 minutes.\n\nIf you didn't ask for this, ignore this email."
                );
                let response = client
                    .post(&url)
                    .bearer_auth(api_token)
                    .json(&serde_json::json!({
                        "to": to,
                        "from": { "address": from.address, "name": from.name },
                        "subject": subject,
                        "text": text,
                    }))
                    .send()
                    .await
                    .map_err(|err| EmailError::Retryable(scrubbed(&err.to_string())))?;
                let status = response.status();
                let body: serde_json::Value = response.json().await.map_err(|err| {
                    EmailError::Retryable(format!("email API returned an unreadable body: {err}"))
                })?;
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || error_code(&body) == Some(10004)
                {
                    return Err(EmailError::Retryable(
                        "email provider is rate limited, try again shortly".to_string(),
                    ));
                }
                if !status.is_success()
                    || body.get("success").and_then(|v| v.as_bool()) != Some(true)
                {
                    let detail = first_error(&body).unwrap_or_else(|| status.to_string());
                    return Err(EmailError::Permanent(format!(
                        "email provider refused the message ({detail})"
                    )));
                }
                if permanent_bounce(&body, to) {
                    return Err(EmailError::Permanent(format!(
                        "email to {to} permanently bounced"
                    )));
                }
                Ok(())
            }
            Self::Log => {
                tracing::info!(email = %to, code = %code, "sign-in code (localhost log sender)");
                Ok(())
            }
            #[cfg(test)]
            Self::Capture { codes } => {
                codes
                    .lock()
                    .unwrap()
                    .insert(to.to_string(), code.to_string());
                Ok(())
            }
        }
    }
}

/// The recipient counts as permanently bounced when it appears in
/// `result.permanent_bounces` or `result.suppressed_recipients`.
fn permanent_bounce(body: &serde_json::Value, to: &str) -> bool {
    let result = body.get("result");
    ["permanent_bounces", "suppressed_recipients"]
        .iter()
        .any(|key| {
            result
                .and_then(|result| result.get(*key))
                .and_then(|list| list.as_array())
                .is_some_and(|list| {
                    list.iter().any(|entry| {
                        entry.as_str() == Some(to)
                            || entry.get("address").and_then(|a| a.as_str()) == Some(to)
                            || entry.get("email").and_then(|a| a.as_str()) == Some(to)
                    })
                })
        })
}

/// First `errors[0]` as `code:message`, if the API answered `success:false`.
fn first_error(body: &serde_json::Value) -> Option<String> {
    body.get("errors")
        .and_then(|errors| errors.as_array())
        .and_then(|errors| errors.first())
        .map(|err| match err.get("code").and_then(|code| code.as_u64()) {
            Some(code) => format!(
                "{code}:{}",
                err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
            ),
            None => err.to_string(),
        })
}

fn error_code(body: &serde_json::Value) -> Option<u64> {
    body.get("errors")
        .and_then(|errors| errors.as_array())
        .and_then(|errors| errors.first())
        .and_then(|err| err.get("code"))
        .and_then(|code| code.as_u64())
}

/// A transport error must never carry the bearer token back to the caller.
fn scrubbed(message: &str) -> String {
    if message.len() > 200 {
        format!("email send failed ({})", &message[..200])
    } else {
        format!("email send failed ({message})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parses_named_and_bare_forms() {
        let named = EmailFrom::parse("Outer <hello@outer.now>");
        assert_eq!(
            named,
            EmailFrom {
                name: "Outer".to_string(),
                address: "hello@outer.now".to_string(),
            }
        );
        let bare = EmailFrom::parse("hello@outer.now");
        assert_eq!(bare.address, "hello@outer.now");
        assert_eq!(bare.name, "hello");
    }

    #[test]
    fn permanent_bounce_matches_string_and_object_entries() {
        let body = serde_json::json!({ "result": { "permanent_bounces": ["ana@example.com"] } });
        assert!(permanent_bounce(&body, "ana@example.com"));
        assert!(!permanent_bounce(&body, "bob@example.com"));
        let body = serde_json::json!({ "result": { "suppressed_recipients": [{"address": "ana@example.com"}] } });
        assert!(permanent_bounce(&body, "ana@example.com"));
        let body = serde_json::json!({ "result": { "delivered": ["ana@example.com"] } });
        assert!(!permanent_bounce(&body, "ana@example.com"));
    }
}
