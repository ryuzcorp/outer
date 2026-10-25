// Console settings actions: visibility, rename, delete. The UI calls
// `run`; the later ⌘K and WebMCP layer imports every `actions/*.ts`
// registry.
import { ConsoleApiError, request } from "./http.ts";

export interface ConsoleAction<I, O> {
  name: string;
  title: string;
  description: string;
  inputSchema: object;
  readOnly: boolean;
  consequential: boolean;
  untrusted: boolean;
  run(input: I, signal?: AbortSignal): Promise<O>;
}

export interface SetVisibilityInput {
  slug: string;
  visibility: "unlisted" | "public";
}

export interface SetVisibilityOutput {
  visibility: string;
}

export interface RenameAppInput {
  slug: string;
  newSlug: string;
}

export interface RenameAppOutput {
  slug: string;
  url: string;
}

export interface DeleteAppInput {
  slug: string;
  confirm: string;
}

export interface DeleteAppOutput {
  deleted: boolean;
  slug: string;
  quarantined_until: number;
  user_message?: string;
}

const setVisibility: ConsoleAction<SetVisibilityInput, SetVisibilityOutput> = {
  name: "set_visibility",
  title: "Set app visibility",
  description:
    "Set an app unlisted (default, kept out of search engines) or public. Public is a Pro feature: Free owners are refused with payment_required. Returns {visibility}.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      visibility: { type: "string", enum: ["unlisted", "public"] },
    },
    required: ["slug", "visibility"],
  },
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(input, signal) {
    return request<SetVisibilityOutput>(
      "POST",
      `/api/console/apps/${encodeURIComponent(input.slug)}/visibility`,
      { visibility: input.visibility },
      signal,
    );
  },
};

const renameApp: ConsoleAction<RenameAppInput, RenameAppOutput> = {
  name: "rename_app",
  title: "Rename app",
  description:
    "Rename an app's slug. Returns {slug, url}. The endpoint may answer 404 while the rename rollout lands; the UI handles that gracefully.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      newSlug: { type: "string" },
    },
    required: ["slug", "newSlug"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  async run(input, signal) {
    try {
      return await request<RenameAppOutput>(
        "POST",
        `/api/console/apps/${encodeURIComponent(input.slug)}/rename`,
        { slug: input.newSlug },
        signal,
      );
    } catch (err: unknown) {
      // The rename endpoint lands separately: while it 404s, say so
      // plainly instead of leaking the status line.
      if (err instanceof ConsoleApiError && err.status === 404) {
        throw new Error(
          "Rename isn't available in this build yet — ask your agent to rename it over MCP.",
        );
      }
      throw err;
    }
  },
};

const deleteApp: ConsoleAction<DeleteAppInput, DeleteAppOutput> = {
  name: "delete_app",
  title: "Delete app",
  description:
    "Delete an app forever: stops the fleet, wipes its code, data and files, and purges its cached pages. The name stays reserved for 30 days. Pass confirm equal to the slug. Returns {deleted, slug, quarantined_until}.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      confirm: { type: "string" },
    },
    required: ["slug", "confirm"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input, signal) {
    return request<DeleteAppOutput>(
      "POST",
      `/api/console/apps/${encodeURIComponent(input.slug)}/delete`,
      { confirm: input.confirm },
      signal,
    );
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  setVisibility as ConsoleAction<unknown, unknown>,
  renameApp as ConsoleAction<unknown, unknown>,
  deleteApp as ConsoleAction<unknown, unknown>,
];

export const settingsActions = { setVisibility, renameApp, deleteApp };
