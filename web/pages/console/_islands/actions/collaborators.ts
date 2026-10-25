// Console action registry for collaborators (Pro, spec §7.4): every
// collaborators mutation or query the Settings card performs goes through
// one exported entry here, so the later ⌘K and WebMCP layer can import
// `actions/*.ts` wholesale. The Settings UI calls `run`.
import { request } from "./http.ts";
import type { ConsoleAction } from "./settings.ts";
import type { CollaboratorEntry, CollaboratorsSummary } from "../api.ts";

function slugPath(slug: string, suffix: string): string {
  return `/api/console/apps/${encodeURIComponent(slug)}${suffix}`;
}

const slugSchema = {
  type: "object",
  properties: { app: { type: "string", description: "The app slug." } },
  required: ["app"],
};

const emailSchema = {
  type: "object",
  properties: {
    app: { type: "string", description: "The app slug." },
    email: { type: "string", description: "The collaborator's email address." },
  },
  required: ["app", "email"],
};

export interface ListCollaboratorsInput {
  app: string;
}

export interface AddCollaboratorInput {
  app: string;
  email: string;
}

export interface AddCollaboratorOutput {
  collaborator: CollaboratorEntry;
}

export interface RemoveCollaboratorOutput {
  removed: boolean;
}

const listCollaborators: ConsoleAction<ListCollaboratorsInput, CollaboratorsSummary> = {
  name: "list_collaborators",
  title: "List collaborators",
  description:
    "List an app's collaborators: email, joined or pending status, and when each was added. Owners and collaborators can read.",
  inputSchema: slugSchema,
  readOnly: true,
  consequential: false,
  untrusted: true,
  run(input, signal?: AbortSignal): Promise<CollaboratorsSummary> {
    return request<CollaboratorsSummary>("GET", slugPath(input.app, "/collaborators"), undefined, signal);
  },
};

const addCollaborator: ConsoleAction<AddCollaboratorInput, AddCollaboratorOutput> = {
  name: "add_collaborator",
  title: "Invite collaborator",
  description:
    "Invite a collaborator to a Pro app by email. The address joins the closed beta and gets an invite email. Owner only; at most 10 collaborators per app.",
  inputSchema: emailSchema,
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input, signal?: AbortSignal): Promise<AddCollaboratorOutput> {
    return request("POST", slugPath(input.app, "/collaborators"), { email: input.email }, signal);
  },
};

const removeCollaborator: ConsoleAction<AddCollaboratorInput, RemoveCollaboratorOutput> = {
  name: "remove_collaborator",
  title: "Remove collaborator",
  description:
    "Remove a collaborator from an app by email. Their access ends immediately. Owner only; confirms in the UI before running.",
  inputSchema: emailSchema,
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input, signal?: AbortSignal): Promise<RemoveCollaboratorOutput> {
    return request("POST", slugPath(input.app, "/collaborators/remove"), { email: input.email }, signal);
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  listCollaborators as ConsoleAction<unknown, unknown>,
  addCollaborator as ConsoleAction<unknown, unknown>,
  removeCollaborator as ConsoleAction<unknown, unknown>,
];

export const collaboratorActions = { listCollaborators, addCollaborator, removeCollaborator };
