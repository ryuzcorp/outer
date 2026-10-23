//! `GET /llms.txt` on the platform host: the Appendix A outline, rendered from
//! the tool registry (tools + access levels) and the §3 plan table.

use crate::mcp::registry::{Access, ToolDef};

/// Render `/llms.txt` for this host. `mcp_url` is the absolute MCP endpoint
/// (`https://outer.now/mcp` in production), `apps_domain` the app host
/// suffix (`outer.now`).
pub fn render(mcp_url: &str, apps_domain: &str, tools: &[ToolDef]) -> String {
    let mut out = String::new();
    out.push_str("# Outer\n");
    out.push_str("> Your agent's own app cloud, built on Ilha. Ask your agent for an app; get a live link to share.\n");
    out.push_str("\n## Connect\n");
    out.push_str(&format!(
        "MCP: {mcp_url} (Streamable HTTP, OAuth 2.1). Outer is in closed beta.\n\n"
    ));
    out.push_str("| Client | Setup |\n");
    out.push_str("| --- | --- |\n");
    out.push_str(&format!(
        "| Claude Code | `claude mcp add --transport http outer {mcp_url}`. The first account tool call opens the browser |\n"
    ));
    out.push_str("| claude.ai, Desktop, mobile | Settings → Connectors → Add custom connector |\n");
    out.push_str(&format!(
        "| Codex CLI, IDE, ChatGPT desktop app | `codex mcp add outer --url {mcp_url}`, then `codex mcp login outer` |\n"
    ));
    out.push_str("| ChatGPT web | A custom plugin in developer mode pointing at the MCP URL |\n");
    out.push_str("| Anything else | OAuth if supported, else `sign_in` → `verify_code` → `Authorization` header |\n");
    out.push_str("\n## Sign in\n");
    out.push_str("- Claude, Codex, ChatGPT: just use a tool. Your client opens Outer's sign-in page; your human enters their email, the emailed code, accepts the terms and approves.\n");
    out.push_str("- No OAuth in your client? `sign_in {email}` → ask for the code → `verify_code {email, code, accept_terms: true}` → save the key in your MCP config.\n");
    out.push_str("\n## Build an Ilha app\n");
    out.push_str(&format!(
        "- `create_app {{slug}}` → a live Ilha + Void app at https://<slug>.{apps_domain} in seconds. It is unlisted (not indexed) until `set_visibility public`.\n"
    ));
    out.push_str("- Read `docs {topic: \"ilha\"}` first, then `list_files`, `read_file`.\n");
    out.push_str("- Change code with `commit` (prefer `edits`). It waits for the deploy and returns `success` + URL, `failed` + log, or `blocked` + the lines to fix. Undo with `rollback`.\n");
    out.push_str("- No strong profanity in page text, seed data or the app name. Run visitor text through `moderate()`.\n");
    out.push_str("\n## Ilha guide\n");
    out.push_str(include_str!("../../docs/ilha.md"));
    out.push_str("\n## When something is refused\n");
    out.push_str("Relay `user_message` to your human verbatim. `quota_exceeded` carries usage and reset info.\n");
    out.push_str("\n## Tools\n");
    for tool in tools {
        let access = match tool.access {
            Access::Public => "anyone",
            Access::Account => "signed in",
        };
        let untrusted = if tool.untrusted {
            " (untrusted output)"
        } else {
            ""
        };
        out.push_str(&format!(
            "- `{}` ({access}{untrusted}): {}\n",
            tool.name, tool.description
        ));
    }
    out.push_str("\n## Limits\n");
    out.push_str("| Limit | Free | Pro |\n");
    out.push_str("| --- | --- | --- |\n");
    out.push_str("| Apps owned | 1 | 10 |\n");
    out.push_str("| Code changes / day | 50 | 500 |\n");
    out.push_str("| Build time / day | 60 min | 600 min |\n");
    out.push_str("| Requests / month | 250k | 5M |\n");
    out.push_str("| App storage | 500 MB | 25 GB |\n");
    out.push_str("| Outbound traffic / month | 5 GB | 100 GB |\n");
    out.push_str("| API keys | 5 | 20 |\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_every_tool_once() {
        let registry = crate::mcp::registry::registry();
        let page = render("https://outer.now/mcp", "outer.now", &registry);
        for tool in &registry {
            assert!(
                page.contains(&format!("`{}`", tool.name)),
                "missing {}",
                tool.name
            );
        }
        assert!(page.contains("claude mcp add"));
        assert!(page.contains("codex mcp add"));
    }
}
