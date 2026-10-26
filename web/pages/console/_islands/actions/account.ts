// Console account actions (spec §13.1, wave 2): every mutation or query
// the account views perform goes through one exported entry here, shaped
// for the later ⌘K palette and WebMCP layer. The UI calls `run`; it never
// fetches the API directly for these.
import {
  api,
  apiPost,
  type AccountSummary,
  type AlertPrefs,
  type ApiKeysSummary,
  type ConnectionsSummary,
  type CreatedKey,
} from "../api.ts";

import type { ActionContext } from "./settings.ts";
export interface ConsoleAction<I, O> {
  name: string;
  title: string;
  description: string;
  inputSchema: object;
  readOnly: boolean;
  consequential: boolean;
  untrusted: boolean;
  run(input: I, ctx?: ActionContext, signal?: AbortSignal): Promise<O>;
}

export interface Revoked {
  revoked: true;
}

export interface SignedOutOthers {
  signed_out: number;
  kept: 1;
}

export interface DeletedAccount {
  deleted: true;
  email: string;
  deleted_apps: string[];
  signed_out_sessions: number;
}

function idSchema(description: string): object {
  return {
    type: "object",
    properties: { id: { type: "string", description } },
    required: ["id"],
  };
}

export const listConnections: ConsoleAction<void, ConnectionsSummary> = {
  name: "list_connections",
  title: "List connections",
  description:
    "List this account's OAuth connections: which clients may act as you, with host, scopes and last use. The console itself uses a web session, so it never appears here and revoking never signs this browser out.",
  inputSchema: { type: "object", properties: {} },
  readOnly: true,
  consequential: false,
  untrusted: true,
  run(_input: void): Promise<ConnectionsSummary> {
    return api<ConnectionsSummary>("/api/console/connections");
  },
};

export const revokeConnection: ConsoleAction<{ id: string }, Revoked> = {
  name: "revoke_connection",
  title: "Revoke connection",
  description:
    "Revoke one OAuth connection by grant id. The client signs in again from scratch. The console's own web session is unaffected.",
  inputSchema: idSchema("The grant id shown by list_connections."),
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input: { id: string }): Promise<Revoked> {
    return apiPost<Revoked>(
      `/api/console/connections/${encodeURIComponent(input.id)}/revoke`,
      {},
    );
  },
};

export const listApiKeys: ConsoleAction<void, ApiKeysSummary> = {
  name: "list_api_keys",
  title: "List API keys",
  description:
    "List this account's API keys: name, prefix, creation and last use. Secrets never leave the server.",
  inputSchema: { type: "object", properties: {} },
  readOnly: true,
  consequential: false,
  untrusted: false,
  run(_input: void): Promise<ApiKeysSummary> {
    return api<ApiKeysSummary>("/api/console/api-keys");
  },
};

export const createApiKey: ConsoleAction<{ name?: string }, CreatedKey> = {
  name: "create_api_key",
  title: "Create API key",
  description:
    "Mint an API key. The key is returned once — show it with copy and a warning that it will not be shown again.",
  inputSchema: {
    type: "object",
    properties: {
      name: {
        type: "string",
        description: "A label for the key (shown in list_api_keys).",
      },
    },
  },
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(input: { name?: string }): Promise<CreatedKey> {
    return apiPost<CreatedKey>("/api/console/api-keys", { name: input.name });
  },
};

export const revokeApiKey: ConsoleAction<{ prefix: string }, Revoked> = {
  name: "revoke_api_key",
  title: "Revoke API key",
  description:
    "Revoke an API key by its prefix. The console's own web session is unaffected.",
  inputSchema: {
    type: "object",
    properties: {
      prefix: {
        type: "string",
        description: "The key prefix shown by list_api_keys.",
      },
    },
    required: ["prefix"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input: { prefix: string }): Promise<Revoked> {
    return apiPost<Revoked>("/api/console/api-keys/revoke", {
      prefix: input.prefix,
    });
  },
};

export const getAccount: ConsoleAction<void, AccountSummary> = {
  name: "get_account",
  title: "Get account",
  description:
    "This account: email, plan and member-since, plus usage measured the same way the quota checks measure it.",
  inputSchema: { type: "object", properties: {} },
  readOnly: true,
  consequential: false,
  untrusted: false,
  run(_input: void): Promise<AccountSummary> {
    return api<AccountSummary>("/api/console/account");
  },
};

export const signOutOthers: ConsoleAction<void, SignedOutOthers> = {
  name: "sign_out_others",
  title: "Sign out other sessions",
  description:
    "Sign out every web session on this account except the current one. Returns how many sessions were deleted.",
  inputSchema: { type: "object", properties: {} },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(_input: void): Promise<SignedOutOthers> {
    return apiPost<SignedOutOthers>(
      "/api/console/sessions/sign-out-others",
      {},
    );
  },
};

export const deleteAccount: ConsoleAction<{ confirm: string }, DeletedAccount> = {
  name: "delete_account",
  title: "Delete account",
  description:
    "Delete this account: pass confirm equal to the account email. Deletes owned apps, leaves apps the account only collaborates on, and signs every session out. Irreversible.",
  inputSchema: {
    type: "object",
    properties: {
      confirm: {
        type: "string",
        description: "The account email, exactly.",
      },
    },
    required: ["confirm"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input: { confirm: string }): Promise<DeletedAccount> {
    return apiPost<DeletedAccount>("/api/console/account/delete", {
      confirm: input.confirm,
    });
  },
};

export const updateAlertPrefs: ConsoleAction<Partial<AlertPrefs>, AlertPrefs> = {
  name: "update_alert_prefs",
  title: "Update email alert prefs",
  description:
    "Flip this account's email alert toggles (deploy failures, error spikes, quota thresholds, inactivity reminders). Pass only the toggles to change; the rest stay as they are. Returns the effective toggles.",
  inputSchema: {
    type: "object",
    properties: {
      deploy_failed: {
        type: "boolean",
        description: "Email the app owner when a deploy fails.",
      },
      error_spike: {
        type: "boolean",
        description: "Email the app owner when errors spike.",
      },
      quota: {
        type: "boolean",
        description: "Email the account when code-change quota hits 80% and 100%.",
      },
      inactivity: {
        type: "boolean",
        description: "Email the account 30, 7, and 1 day before a Free app is deleted for inactivity.",
      },
    },
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(input: Partial<AlertPrefs>): Promise<AlertPrefs> {
    return apiPost<AlertPrefs>("/api/console/alerts/prefs", input);
  },
};

export interface CheckoutSession {
  checkout_url: string;
}

export interface PortalSession {
  portal_url: string;
}

export const createCheckout: ConsoleAction<void, CheckoutSession> = {
  name: "create_checkout",
  title: "Upgrade to Pro",
  description:
    "Mint a Stripe Checkout link to upgrade this account to Outer Pro. Open the returned URL in a browser to pay; the plan lifts on the next call after payment.",
  inputSchema: { type: "object", properties: {} },
  readOnly: false,
  consequential: true,
  untrusted: false,
  run(_input: void): Promise<CheckoutSession> {
    return apiPost<CheckoutSession>("/api/console/billing/checkout", {});
  },
};

export const createPortal: ConsoleAction<void, PortalSession> = {
  name: "create_portal",
  title: "Manage billing",
  description:
    "Mint a Stripe Customer Portal link to update the card or cancel Outer Pro. Needs an existing subscription — upgrade first.",
  inputSchema: { type: "object", properties: {} },
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(_input: void): Promise<PortalSession> {
    return apiPost<PortalSession>("/api/console/billing/portal", {});
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  listConnections as ConsoleAction<unknown, unknown>,
  revokeConnection as ConsoleAction<unknown, unknown>,
  listApiKeys as ConsoleAction<unknown, unknown>,
  createApiKey as ConsoleAction<unknown, unknown>,
  revokeApiKey as ConsoleAction<unknown, unknown>,
  getAccount as ConsoleAction<unknown, unknown>,
  signOutOthers as ConsoleAction<unknown, unknown>,
  deleteAccount as ConsoleAction<unknown, unknown>,
  updateAlertPrefs as ConsoleAction<unknown, unknown>,
  createCheckout as ConsoleAction<unknown, unknown>,
  createPortal as ConsoleAction<unknown, unknown>,
];
