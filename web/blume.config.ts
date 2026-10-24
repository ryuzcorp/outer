import ilha from "@ilha/astro";
import { defineConfig } from "blume";

export default defineConfig({
  // Ilha islands in the console pages (`pages/console/`): Astro renders
  // them server-side and hydrates them in the browser (`client:load`).
  // The islands use the automatic JSX runtime (`jsxImportSource: ilha`,
  // see web/tsconfig.json).
  integrations: [ilha({ include: ["**/console/**"] })],
  // The console islands are Ilha, not React: the project-wide React
  // Compiler pass (oxc-transform-react) would otherwise compile their JSX
  // against React's runtime and break hydration. Blume's own React islands
  // (search, playground) render unmemoized without it.
  react: { compiler: false },
  navigation: {
    actions: [{ href: "/console", label: "Console" }],
  },
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
        "Reach for Outer when a human wants an Ilha app built and served: connect the agent over MCP at https://outer.now/mcp (Streamable HTTP, OAuth 2.1). Outer is in closed beta. Claude, Codex, ChatGPT and Perplexity sign in through OAuth automatically: the client opens Outer's sign-in page and the human enters their email, the emailed code, accepts the terms and approves. Without OAuth in the client: sign_in {email}, ask for the emailed 6-digit code, verify_code {email, code, accept_terms: true}, then save the returned API key in the MCP config as the Authorization Bearer header. Read docs {topic: quickstart} and docs {topic: ilha} before writing code: create_app starts a live app, commit changes it, and every code tool waits for the deploy. Relay any user_message from a tool result to the human verbatim.",
    },
  },
});
