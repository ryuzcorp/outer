// Console deploys actions: roll back to an earlier successful deploy.
import { request } from "./http.ts";
import type { ConsoleAction } from "./settings.ts";

export interface RollbackInput {
  slug: string;
  sha: string;
}

export interface RollbackDeploy {
  status: string;
  [key: string]: unknown;
}

export interface RollbackOutput {
  sha: string | null;
  deploy: RollbackDeploy | null;
}

const rollbackApp: ConsoleAction<RollbackInput, RollbackOutput> = {
  name: "rollback",
  title: "Roll back to a deploy",
  description:
    "Roll back to an earlier successful deploy: makes a new commit restoring that tree, then deploys it without waiting. The target sha must be an earlier successful deploy. Counts against the app's daily rollback budget. Returns {sha, deploy}; the UI refreshes the deploy list to show the new build.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      sha: { type: "string" },
    },
    required: ["slug", "sha"],
  },
  readOnly: false,
  consequential: true,
  untrusted: true,
  run(input, _ctx, signal) {
    return request<RollbackOutput>(
      "POST",
      `/api/console/apps/${encodeURIComponent(input.slug)}/rollback`,
      { sha: input.sha },
      signal,
    );
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  rollbackApp as ConsoleAction<unknown, unknown>,
];

export const deploysActions = { rollbackApp };
