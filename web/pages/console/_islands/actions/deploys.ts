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

export interface PreviewInput {
  slug: string;
}

export interface PreviewDeploy {
  status: string;
  [key: string]: unknown;
}

export interface PromotePreviewOutput {
  sha: string | null;
  deploy: PreviewDeploy | null;
}

const promotePreview: ConsoleAction<PreviewInput, PromotePreviewOutput> = {
  name: "promote_preview",
  title: "Promote preview to live",
  description:
    "Promote the app's preview to live: fast-forwards main to the preview commit and deploys it normally without waiting. Needs a successful preview deploy. Returns {sha, deploy}; the UI refreshes the app to show the new live version.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
    },
    required: ["slug"],
  },
  readOnly: false,
  consequential: true,
  untrusted: true,
  run(input, _ctx, signal) {
    return request<PromotePreviewOutput>(
      "POST",
      `/api/console/apps/${encodeURIComponent(input.slug)}/preview/promote`,
      {},
      signal,
    );
  },
};

export interface DiscardPreviewOutput {
  app: string | null;
}

const discardPreview: ConsoleAction<PreviewInput, DiscardPreviewOutput> = {
  name: "discard_preview",
  title: "Discard the preview",
  description:
    "Discard the app's preview: stops the preview fleet and removes its trial state. Live is untouched. Returns {app}; the UI refreshes the app to drop the preview card.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
    },
    required: ["slug"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input, _ctx, signal) {
    return request<DiscardPreviewOutput>(
      "POST",
      `/api/console/apps/${encodeURIComponent(input.slug)}/preview/discard`,
      {},
      signal,
    );
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  rollbackApp as ConsoleAction<unknown, unknown>,
  promotePreview as ConsoleAction<unknown, unknown>,
  discardPreview as ConsoleAction<unknown, unknown>,
];

export const deploysActions = { rollbackApp, promotePreview, discardPreview };

