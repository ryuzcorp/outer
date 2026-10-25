// Admin moderation actions (spec §15.3, §13 `admin_stats`): the console
// endpoints in `http/console_admin.rs` as registered actions, so ⌘K and
// WebMCP pick them up. Every one is `adminOnly`: the palette and the
// WebMCP layer only register them for admins (the server 403s the rest
// anyway). Reads are plain fetches; the four writes are consequential, so
// they confirm in the console modal like every other destructive action.
import { request } from "./http.ts";
import type { ConsoleAction } from "./settings.ts";

export interface ModerationQueueItem {
  kind: "blocked_deploy" | "held_app" | "suspended_app" | "flagged_check";
  id: string;
  app_id: string;
  app_slug: string;
  owner_email: string | null;
  sha: string | null;
  reason: string | null;
  findings: unknown;
  scores: unknown;
  reviewed_by: string | null;
  reviewed_at: number | null;
  created_at: number;
}

export interface ModerationQueue {
  items: ModerationQueueItem[];
}

export interface AdminStats {
  accounts_total: number;
  accounts_by_plan: { plan: string; accounts: number }[];
  waitlist_size: number;
  apps_total: number;
  apps_awake: number;
  apps_asleep: number;
  apps_held: number;
  apps_suspended: number;
  blocked_deploys: number;
  unreviewed_checks: number;
  deploys_7d: number;
  deploy_failure_rate_7d: number;
  deploy_p50_ms_7d: number | null;
  deploy_p95_ms_7d: number | null;
}

export interface ReviewInput {
  kind: "deploy" | "app";
  id: string;
  verdict: "allow" | "deny";
}

export interface ReviewOutput {
  reviewed: boolean;
  redeployed: boolean;
  deploy?: unknown;
}

export interface SuspendOutput {
  suspended: boolean;
  slug: string;
}

export interface RestoreOutput {
  restored: boolean;
  slug: string;
}

export interface BanInput {
  email: string;
  reason: string;
}

export interface BanOutput {
  banned: boolean;
  email: string;
  apps_stopped: number;
}

const moderationQueue: ConsoleAction<Record<string, never>, ModerationQueue> = {
  name: "moderation_queue",
  title: "Show moderation queue",
  description:
    "The admin moderation queue: latest blocked deploy per app with the gate's flagged chunks and scores, held/suspended apps, and unreviewed after-live checks. Returns {items}. Admins only.",
  inputSchema: { type: "object", properties: {} },
  readOnly: true,
  consequential: false,
  untrusted: true,
  adminOnly: true,
  run(_input, _ctx, signal) {
    return request<ModerationQueue>("GET", "/api/console/admin/moderation/queue", undefined, signal);
  },
};

const adminStats: ConsoleAction<Record<string, never>, AdminStats> = {
  name: "admin_stats",
  title: "Show admin stats",
  description:
    "The admin overview numbers: accounts by plan, waitlist size, apps awake/asleep/held/suspended, recent deploy health (count, failure rate, p50/p95 durations) and moderation queue depth. Returns the stats object. Admins only.",
  inputSchema: { type: "object", properties: {} },
  readOnly: true,
  consequential: false,
  untrusted: false,
  adminOnly: true,
  run(_input, _ctx, signal) {
    return request<AdminStats>("GET", "/api/console/admin/stats", undefined, signal);
  },
};

const adminReview: ConsoleAction<ReviewInput, ReviewOutput> = {
  name: "admin_review",
  title: "Review a blocked deploy or held app",
  description:
    "Decide a moderation queue item {kind: deploy|app, id, verdict: allow|deny}. Allow on a blocked deploy clears that app's content-check hashes and redeploys the blocked commit; allow on a held/suspended app lifts the hold. Deny records the decision and keeps the block. Returns {reviewed, redeployed}. Admins only.",
  inputSchema: {
    type: "object",
    properties: {
      kind: { type: "string", enum: ["deploy", "app"] },
      id: { type: "string" },
      verdict: { type: "string", enum: ["allow", "deny"] },
    },
    required: ["kind", "id", "verdict"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  adminOnly: true,
  run(input, _ctx, signal) {
    return request<ReviewOutput>("POST", "/api/console/admin/review", input, signal);
  },
};

const adminSuspendApp: ConsoleAction<{ slug: string }, SuspendOutput> = {
  name: "admin_suspend_app",
  title: "Suspend an app",
  description:
    "Suspend an app {slug}: stops its fleet and purges its cached pages. The wake sweep skips suspended apps until restored. Returns {suspended, slug}. Admins only.",
  inputSchema: {
    type: "object",
    properties: { slug: { type: "string" } },
    required: ["slug"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  adminOnly: true,
  run(input, _ctx, signal) {
    return request<SuspendOutput>(
      "POST",
      `/api/console/admin/apps/${encodeURIComponent(input.slug)}/suspend`,
      {},
      signal,
    );
  },
};

const adminRestoreApp: ConsoleAction<{ slug: string }, RestoreOutput> = {
  name: "admin_restore_app",
  title: "Restore a suspended app",
  description:
    "Restore a suspended app {slug} back to normal: the wake sweep picks it up again on demand. Returns {restored, slug}. Admins only.",
  inputSchema: {
    type: "object",
    properties: { slug: { type: "string" } },
    required: ["slug"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  adminOnly: true,
  run(input, _ctx, signal) {
    return request<RestoreOutput>(
      "POST",
      `/api/console/admin/apps/${encodeURIComponent(input.slug)}/restore`,
      {},
      signal,
    );
  },
};

const adminBanAccount: ConsoleAction<BanInput, BanOutput> = {
  name: "admin_ban_account",
  title: "Ban an account",
  description:
    "Ban an account {email, reason}: sign-in is refused from then on and every owned app is parked as stopped. A reason is required. Returns {banned, email, apps_stopped}. Admins only.",
  inputSchema: {
    type: "object",
    properties: {
      email: { type: "string" },
      reason: { type: "string" },
    },
    required: ["email", "reason"],
  },
  readOnly: false,
  consequential: true,
  untrusted: false,
  adminOnly: true,
  run(input, _ctx, signal) {
    return request<BanOutput>("POST", "/api/console/admin/accounts/ban", input, signal);
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  moderationQueue as ConsoleAction<unknown, unknown>,
  adminStats as ConsoleAction<unknown, unknown>,
  adminReview as ConsoleAction<unknown, unknown>,
  adminSuspendApp as ConsoleAction<unknown, unknown>,
  adminRestoreApp as ConsoleAction<unknown, unknown>,
  adminBanAccount as ConsoleAction<unknown, unknown>,
];

export const adminModerationActions = {
  moderationQueue,
  adminStats,
  adminReview,
  adminSuspendApp,
  adminRestoreApp,
  adminBanAccount,
};
