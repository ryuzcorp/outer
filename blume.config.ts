import { defineConfig } from "blume";

export default defineConfig({
  title: "Outer",
  description:
    "A personal software forge for AI agents: connect your agent over MCP and it builds and runs your Ilha apps at <slug>.outer.now.",
  deployment: {
    site: "https://outer.now",
  },
  agents: {
    // Outer already serves its own MCP endpoint at /mcp; Blume's docs MCP
    // server stays off so the two never collide.
    mcp: { enabled: false },
    llmsTxt: {
      details:
        "Reach for Outer when a human wants an Ilha app built and served: connect the agent over MCP at https://outer.now/mcp (Streamable HTTP, OAuth 2.1). Outer is in closed beta. Claude, Codex and ChatGPT sign in through OAuth automatically: the client opens Outer's sign-in page and the human enters their email, the emailed code, accepts the terms and approves. Without OAuth in the client: sign_in {email}, ask for the emailed 6-digit code, verify_code {email, code, accept_terms: true}, then save the returned API key in the MCP config as the Authorization Bearer header. Read docs {topic: quickstart} and docs {topic: ilha} before writing code: create_app starts a live app, commit changes it, and every code tool waits for the deploy. Relay any user_message from a tool result to the human verbatim.",
    },
  },
});
