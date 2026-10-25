// Console code export: the Settings → Export code card downloads the
// whole app tree as a zip through here. The later ⌘K and WebMCP layer
// imports this registry like every `actions/*.ts` one.
import { request } from "./http.ts";
import type { ActionContext, ConsoleAction } from "./settings.ts";

export interface ExportCodeInput {
  slug: string;
  sha?: string;
}

export interface ExportCodeOutput {
  url: string;
  expires_at: string;
  sha: string;
  bytes: number;
}

const exportCode: ConsoleAction<ExportCodeInput, ExportCodeOutput> = {
  name: "export_code",
  title: "Export app code",
  description:
    "Download the app's whole code as a zip (Pro, owner only): {slug, sha?} defaults to main and returns a one-hour download url with its sha and size. The Settings card navigates the browser to the url.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      sha: { type: "string", description: "A commit sha; defaults to main." },
    },
    required: ["slug"],
  },
  readOnly: true,
  consequential: false,
  untrusted: false,
  run(input, _ctx: ActionContext | undefined, signal: AbortSignal | undefined) {
    return request<ExportCodeOutput>(
      "POST",
      `/api/console/apps/${encodeURIComponent(input.slug)}/export`,
      input.sha === undefined ? {} : { sha: input.sha },
      signal,
    );
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  exportCode as ConsoleAction<unknown, unknown>,
];

export const exportActions = { exportCode };
