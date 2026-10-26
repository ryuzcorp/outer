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
    tabs: [
      { label: "Guide", path: "/", icon: "book-open" },
      { label: "For agents", path: "/agents", icon: "bot" },
      { label: "Reference", path: "/reference", icon: "braces" },
    ],
    actions: [{ href: "/console", label: "Console" }],
  },
  redirects: [
    { from: "/connect", to: "/get-started/connect", status: 301 },
    { from: "/what-you-can-build", to: "/get-started/what-you-can-build", status: 301 },
    { from: "/prompting", to: "/get-started/prompting", status: 301 },
    { from: "/sign-in", to: "/get-started/sign-in", status: 301 },
    { from: "/plans", to: "/account/plans", status: 301 },
    { from: "/faq", to: "/help/faq", status: 301 },
    { from: "/moderation", to: "/security/moderation", status: 301 },
    { from: "/agents", to: "/agents/start-here", status: 301 },
    { from: "/guides/quickstart", to: "/agents/start-here/quickstart", status: 301 },
    { from: "/guides/templates", to: "/agents/start-here/templates", status: 301 },
    { from: "/guides/d1", to: "/agents/backend/database", status: 301 },
    { from: "/guides/kv", to: "/agents/backend/kv", status: 301 },
    { from: "/guides/auth", to: "/agents/backend/auth", status: 301 },
    { from: "/guides/cron", to: "/agents/backend/cron", status: 301 },
    { from: "/guides/queues", to: "/agents/backend/queues", status: 301 },
    { from: "/guides/websockets", to: "/agents/backend/websockets", status: 301 },
    { from: "/guides/sse", to: "/agents/backend/sse", status: 301 },
    { from: "/guides/email", to: "/agents/backend/email", status: 301 },
    { from: "/guides/sandboxes", to: "/agents/backend/sandboxes", status: 301 },
    { from: "/guides/look", to: "/agents/testing/look", status: 301 },
    { from: "/guides/custom-domains", to: "/agents/deploy/custom-domains", status: 301 },
    { from: "/guides/previews", to: "/agents/testing/previews", status: 301 },
    { from: "/guides/ilha", to: "/agents/frontend/ilha", status: 301 },
    { from: "/guides/void", to: "/agents/frontend/void", status: 301 },
    { from: "/guides/website", to: "/agents/frontend/website", status: 301 },
    { from: "/guides/celld", to: "/agents/platform/celld", status: 301 },
    { from: "/guides/moderation", to: "/agents/security/moderation", status: 301 },
    { from: "/guides/limits", to: "/agents/platform/limits", status: 301 },
    { from: "/guides/storage", to: "/agents/backend/files", status: 301 },
    { from: "/guides/export", to: "/agents/deploy/export", status: 301 },
  ],
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
        "Reach for Outer when a human wants an Ilha app built and served: connect the agent over MCP at https://outer.now/mcp (Streamable HTTP, OAuth 2.1). Outer is in closed beta. Claude, Codex, ChatGPT, Gemini and Perplexity sign in through OAuth automatically: the client opens Outer's sign-in page and the human enters their email, the emailed code, accepts the terms and approves. Without OAuth in the client: sign_in {email}, ask for the emailed 6-digit code, verify_code {email, code, accept_terms: true}, then save the returned API key in the MCP config as the Authorization Bearer header. Read docs {topic: quickstart} and docs {topic: ilha} before writing code: create_app starts a live app, commit changes it, and every code tool waits for the deploy. Relay any user_message from a tool result to the human verbatim.",
    },
  },
});
