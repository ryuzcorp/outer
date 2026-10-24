use maud::{html, Markup};

use super::cimd::ClientDoc;

pub const CSP: &str = "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; style-src 'unsafe-inline'";

/// The consent page's CSP: `form-action` also admits the origin the Allow /
/// Deny redirect goes to (`https://www.perplexity.ai`, `http://127.0.0.1:5555`)
/// or, for an app's own URI scheme, that scheme (`cursor:`). The redirect URI
/// was already matched against the client's registered list, so this only
/// ever names the client's own target. Anything unparsable adds nothing.
pub fn consent_csp(redirect_uri: &str) -> String {
    let target = url::Url::parse(redirect_uri)
        .ok()
        .and_then(|uri| match uri.scheme() {
            "https" | "http" => {
                let origin = uri.origin().ascii_serialization();
                (origin != "null").then_some(origin)
            }
            scheme => Some(format!("{scheme}:")),
        });
    match target {
        Some(target) => CSP.replace(
            "form-action 'self'",
            &format!("form-action 'self' {target}"),
        ),
        None => CSP.to_string(),
    }
}

fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Outer" }
            }
            body style="font-family:system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem;line-height:1.5;" {
                h1 style="font-size:1.25rem;" { "Outer" }
                (body)
            }
        }
    }
}

fn hidden(request_id: &str, csrf: &str, step: &str) -> Markup {
    html! {
        input type="hidden" name="request_id" value=(request_id);
        input type="hidden" name="csrf" value=(csrf);
        input type="hidden" name="step" value=(step);
    }
}

pub fn email_page(client: &ClientDoc, request_id: &str, csrf: &str, error: Option<&str>) -> Markup {
    layout(
        "Sign in",
        html! {
            p { "Sign in to let " strong { (client.name) } " (" (client.host) ") create, change and delete apps on your Outer account." }
            @if let Some(error) = error {
                p role="alert" style="color:#a00;" { (error) }
            }
            form method="post" action="/oauth/authorize" {
                (hidden(request_id, csrf, "email"))
                label for="email" { "Email" }
                br;
                input type="email" name="email" id="email" required autocomplete="email"
                    style="width:100%;font-size:1rem;padding:.5rem;" {}
                br; br;
                button type="submit" style="font-size:1rem;padding:.5rem 1rem;" { "Send code" }
            }
        },
    )
}

pub fn code_page(email: &str, request_id: &str, csrf: &str, error: Option<&str>) -> Markup {
    layout(
        "Check your email",
        html! {
            p { "Outer sent a 6-digit code to " strong { (email) } ". It expires in 10 minutes." }
            @if let Some(error) = error {
                p role="alert" style="color:#a00;" { (error) }
            }
            form method="post" action="/oauth/authorize" {
                (hidden(request_id, csrf, "code"))
                input type="hidden" name="email" value=(email);
                label for="code" { "Code" }
                br;
                input type="text" name="code" id="code" required inputmode="numeric"
                    autocomplete="one-time-code" minlength="6" maxlength="6"
                    style="width:100%;font-size:1.5rem;padding:.5rem;letter-spacing:.25rem;" {}
                br; br;
                button type="submit" style="font-size:1rem;padding:.5rem 1rem;" { "Continue" }
            }
        },
    )
}

pub fn terms_page(version: &str, request_id: &str, csrf: &str, error: Option<&str>) -> Markup {
    layout(
        "Terms",
        html! {
            p { "First sign-in: accept the Terms (" (version) ") and Privacy Policy to continue." }
            p {
                a href="/terms" { "Terms" } " · " a href="/privacy" { "Privacy Policy" }
            }
            @if let Some(error) = error {
                p role="alert" style="color:#a00;" { (error) }
            }
            form method="post" action="/oauth/authorize" {
                (hidden(request_id, csrf, "terms"))
                label {
                    input type="checkbox" name="accept" value="yes" required;
                    " I accept the Terms and Privacy Policy"
                }
                br; br;
                button type="submit" style="font-size:1rem;padding:.5rem 1rem;" { "Accept and continue" }
            }
        },
    )
}
pub fn consent_page(client: &ClientDoc, scope: &str, request_id: &str, csrf: &str) -> Markup {
    let scopes = if scope.split(' ').any(|part| part == "offline_access") {
        "create, change and delete apps on your Outer account, and stay signed in"
    } else {
        "create, change and delete apps on your Outer account"
    };
    layout(
        "Allow access",
        html! {
            p {
                "Allow " strong { (client.name) } " (" (client.host) ") to " (scopes) "?"
            }
            @if !client.verified {
                p role="note" style="color:#555;" {
                    "This app registered itself with Outer, so its name comes from the app. After you allow it, you return to " (client.host) "."
                }
            }
            @if client.loopback_only {
                p role="note" { "This app runs on your own computer (a local address)." }
            }
            form method="post" action="/oauth/authorize" {
                (hidden(request_id, csrf, "decision"))
                button type="submit" name="allow" value="yes" style="font-size:1rem;padding:.5rem 1rem;" { "Allow" }
                " "
                button type="submit" name="allow" value="no" style="font-size:1rem;padding:.5rem 1rem;" { "Deny" }
            }
        },
    )
}

/// After Allow/Deny: hand the browser back to the client with a page that
/// navigates by itself (`<meta refresh>`) instead of a 302 answer to the form
/// POST. Browsers apply CSP `form-action` to every hop of a form
/// submission's redirect chain, and some clients bounce through more than one
/// origin before landing (Gemini: oauth-redirect.googleusercontent.com, then
/// Google's own pages), so a 302 gets the user stuck on the consent page. A
/// refresh is a fresh navigation that `form-action` doesn't govern. The link
/// is the fallback when refresh is disabled.
pub fn return_page(url: &str, host: &str) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta http-equiv="refresh" content=(format!("0;url={url}"));
                title { "Returning · Outer" }
            }
            body style="font-family:system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem;line-height:1.5;" {
                h1 style="font-size:1.25rem;" { "Outer" }
                p { "Returning to " strong { (host) } "…" }
                p { a href=(url) { "Continue to " (host) } }
            }
        }
    }
}

pub fn error_page(title: &str, message: &str) -> Markup {
    layout(
        title,
        html! {
            p role="alert" style="color:#a00;" { (message) }
            p { a href="/" { "Back to Outer" } }
        },
    )
}

/// Human-readable OAuth failures become closed-beta and rate-limit text on
/// the email page; anything else becomes a 400 error page.
pub fn friendly_error(code: &str) -> &str {
    match code {
        "closed_beta" => "Outer is in closed beta. You're on the waitlist.",
        "rate_limited" => "Too many attempts. Try again shortly.",
        "invalid_email" => "That doesn't look like an email address.",
        "email_retry" => "Outer couldn't send the code. Try again shortly.",
        "email_failed" => "Outer couldn't send the code to that address.",
        _ => "Something went wrong signing in. Start over from the app.",
    }
}

pub fn render(page: Markup) -> String {
    page.into_string()
}

#[cfg(test)]
mod csp_tests {
    use super::consent_csp;

    #[test]
    fn consent_form_may_redirect_to_the_client_only() {
        let csp = consent_csp("https://www.perplexity.ai/rest/connections/oauth_callback");
        assert!(
            csp.contains("form-action 'self' https://www.perplexity.ai;"),
            "{csp}"
        );
        let csp = consent_csp("http://127.0.0.1:5555/callback");
        assert!(
            csp.contains("form-action 'self' http://127.0.0.1:5555;"),
            "{csp}"
        );
        let csp = consent_csp("cursor://anysphere.cursor-retrieval/oauth/callback");
        assert!(csp.contains("form-action 'self' cursor:;"), "{csp}");
        let csp = consent_csp("not a url");
        assert!(csp.contains("form-action 'self';"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    }
}
