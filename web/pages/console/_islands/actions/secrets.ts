// Console secrets actions: list, set (add/replace), unset. Values are
// write-only: they travel to the server but never come back in any answer.
import { request } from "./http.ts";
import type { ConsoleAction } from "./settings.ts";

export interface ListSecretsInput {
  slug: string;
}

export interface SecretName {
  name: string;
  updated_at: number;
}

export interface ListSecretsOutput {
  secrets: SecretName[];
}

export interface SetSecretInput {
  slug: string;
  name: string;
  value: string;
}

export interface SetSecretOutput {
  name: string;
  updated_at: number;
  applies: string;
}

export interface UnsetSecretInput {
  slug: string;
  name: string;
}

export interface UnsetSecretOutput {
  name: string;
  applies: string;
}

const listSecrets: ConsoleAction<ListSecretsInput, ListSecretsOutput> = {
  name: "list_secrets",
  title: "List secret names",
  description:
    "List an app's secret names with updated_at, ordered by name. Values never leave the server. Returns {secrets}.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
    },
    required: ["slug"],
  },
  readOnly: true,
  consequential: false,
  untrusted: false,
  run(input, signal) {
    return request<ListSecretsOutput>(
      "GET",
      `/api/console/apps/${encodeURIComponent(input.slug)}/secrets`,
      undefined,
      signal,
    );
  },
};

const setSecret: ConsoleAction<SetSecretInput, SetSecretOutput> = {
  name: "set_secret",
  title: "Set secret",
  description:
    "Add or replace one app secret. Names match ^[A-Z_][A-Z0-9_]{0,63}$, values are at most 4 KiB. Takes effect on the next deploy. Returns {name, updated_at, applies}; the value never echoes.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      name: { type: "string" },
      value: { type: "string" },
    },
    required: ["slug", "name", "value"],
  },
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(input, signal) {
    return request<SetSecretOutput>(
      "PUT",
      `/api/console/apps/${encodeURIComponent(input.slug)}/secrets/${encodeURIComponent(input.name)}`,
      { value: input.value },
      signal,
    );
  },
};

const unsetSecret: ConsoleAction<UnsetSecretInput, UnsetSecretOutput> = {
  name: "unset_secret",
  title: "Remove secret",
  description:
    "Remove one app secret. Takes effect on the next deploy. Returns {name, applies}.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string" },
      name: { type: "string" },
    },
    required: ["slug", "name"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input, signal) {
    return request<UnsetSecretOutput>(
      "DELETE",
      `/api/console/apps/${encodeURIComponent(input.slug)}/secrets/${encodeURIComponent(input.name)}`,
      undefined,
      signal,
    );
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  listSecrets as ConsoleAction<unknown, unknown>,
  setSecret as ConsoleAction<unknown, unknown>,
  unsetSecret as ConsoleAction<unknown, unknown>,
];

export const secretsActions = { listSecrets, setSecret, unsetSecret };
