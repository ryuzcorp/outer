// Shared fetch for the console islands: same-origin JSON, and a missing or
// expired web session sends the human through the Rust sign-in flow.
// Failures throw ApiError carrying the parsed `{error, message,
// user_message}` body, so views can show the server's human message.
export class ApiError extends Error {
  status: number;
  body: unknown;
  constructor(status: number, body: unknown) {
    // Older endpoints answer `{error, message}` with no `user_message`;
    // prefer the server's sentence either way over the bare status text.
    let message = `request failed: ${status}`;
    if (body !== null && typeof body === "object") {
      const human =
        (body as Record<string, unknown>).user_message ??
        (body as Record<string, unknown>).message;
      if (typeof human === "string" && human.length > 0) {
        message = human;
      }
    }
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }
}

async function fail(res: Response): Promise<never> {
  let body: unknown = null;
  try {
    body = await res.json();
  } catch {
    body = null;
  }
  throw new ApiError(res.status, body);
}

export async function api<T>(path: string): Promise<T> {
  const res = await fetch(path, { headers: { accept: "application/json" } });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new Error("signed out");
  }
  if (!res.ok) {
    await fail(res);
  }
  return (await res.json()) as T;
}

export async function apiPost<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { accept: "application/json", "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new Error("signed out");
  }
  if (!res.ok) {
    await fail(res);
  }
  return (await res.json()) as T;
}

export interface LastDeploy {
  sha: string;
  status: string;
  at?: number;
  duration_ms?: number;
}

export interface ConsoleApp {
  slug: string;
  url: string;
  role: string;
  visibility: string;
  /** Pro keep-warm flag; stored for every plan, ignored on Free. */
  always_on: boolean;
  status: string;
  last_request_at: number | null;
  last_deploy?: LastDeploy;
}

export interface ConsoleDeploy {
  deploy_id: string;
  sha: string;
  status: string;
  findings_count: number;
  created_at: number;
  duration_ms?: number;
  message?: string;
}

export interface PreviewInfo {
  url: string;
  sha: string;
  status: string;
  updated_at: number;
}

export interface AppDetail extends ConsoleApp {
  deploys: ConsoleDeploy[];
  preview?: PreviewInfo | null;
}

export interface LogLine {
  seq: number;
  /** Unix milliseconds (the console API converts the stored seconds). */
  ts: number;
  stream: string;
  line: string;
  fingerprint?: string;
}

/**
 * The live version: the newest successful deploy (the list is newest
 * first), or null when nothing succeeded yet. Shared by DeploysView and
 * the Export card so both agree on what "live" downloads or rolls back to.
 */
export function liveDeploySha(deploys: ConsoleDeploy[]): string | null {
  return deploys.find((d) => d.status === "success")?.sha ?? null;
}

export interface LogFilter {
  q?: string;
  stream?: string;
  level?: string;
  from?: number;
  to?: number;
}

export interface LogsPage {
  lines: LogLine[];
  next_since: number | null;
}

export interface ErrorIssue {
  fingerprint: string;
  message: string;
  frame: string | null;
  count: number;
  first_seen: number;
  last_seen: number;
  status: string;
  resolved_at: number | null;
  regressed: boolean;
}

export interface ErrorCounts {
  open: number;
  resolved: number;
  ignored: number;
}

export interface ErrorsSummary {
  issues: ErrorIssue[];
  counts: ErrorCounts;
  s5xx_last_hour: number;
}

export interface SparkBucket {
  start: number;
  count: number;
}

export interface IssueDetail {
  issue: ErrorIssue;
  sparkline: SparkBucket[];
  recent: LogLine[];
}

export interface MetricsBucket {
  hour: number;
  requests: number;
  s4xx: number;
  s5xx: number;
}

export interface PathStat {
  path: string;
  requests: number;
  s5xx: number;
}

export interface MetricsSummary {
  requests: number;
  visitors: number;
  s4xx: number;
  s5xx: number;
  p50_ms: number;
  p95_ms: number;
  buckets: MetricsBucket[];
  top_paths: PathStat[];
}

export interface ActionEntry {
  id: string;
  at: number;
  tool: string;
  summary: string;
  outcome: string;
  error_code: string | null;
  duration_ms: number;
  by: string;
}

export interface ActionsPage {
  actions: ActionEntry[];
  next: string | null;
}

export interface SecretEntry {
  name: string;
  updated_at: number;
}

export interface SecretsSummary {
  secrets: SecretEntry[];
}

export interface DomainTxtRecord {
  name: string;
  value: string;
}

export interface DomainEntry {
  hostname: string;
  status: "pending" | "active" | "failed" | string;
  ssl_status: string;
  reason: string | null;
  cname_target: string;
  txt_records: DomainTxtRecord[];
}

export interface DomainsSummary {
  plan: string;
  role: string;
  domains: DomainEntry[];
}

export interface DomainResult {
  domain: DomainEntry | null;
}

export interface CollaboratorEntry {
  email: string;
  status: "joined" | "pending";
  added_at: number;
}

export interface CollaboratorsSummary {
  app: string;
  role: string;
  owner_plan: string;
  collaborators: CollaboratorEntry[];
}

export interface InviteEntry {
  code: string;
  kind: string;
  note: string | null;
  createdAt: number;
  redeemedAt: number | null;
  redeemedByEmail: string | null;
  revokedAt: number | null;
  createdByEmail?: string | null;
}

export interface InvitesSummary {
  codes: InviteEntry[];
}

export interface AdminInvitesPage {
  codes: InviteEntry[];
  next?: string;
}

export interface WaitlistEntry {
  email: string;
  first_seen: number;
  attempts: number;
}

export interface WaitlistSummary {
  entries: WaitlistEntry[];
}

export interface ConnectionEntry {
  id: string;
  client_id: string;
  client_name: string | null;
  host: string | null;
  scope: string;
  created_at: number;
  last_used_at: number | null;
}

export interface ConnectionsSummary {
  console_session: string;
  connections: ConnectionEntry[];
}

export interface ApiKeyEntry {
  id: string;
  name: string | null;
  prefix: string;
  created_at: number;
  last_used_at: number | null;
  client: string | null;
}

export interface ApiKeysSummary {
  console_session: string;
  keys: ApiKeyEntry[];
}

export interface CreatedKey {
  id: string;
  name: string | null;
  prefix: string;
  api_key: string;
}

export interface RollbackUsage {
  slug: string;
  used_today: number;
  limit: number;
}

export interface AccountUsage {
  apps_owned: number;
  apps_limit: number;
  code_changes_today: number;
  code_changes_limit: number;
  build_ms_today: number;
  rollbacks: RollbackUsage[];
  rollback_limit: number;
  requests_month: number;
  storage_peak_bytes: number;
  egress_bytes_month: number;
}

export interface AlertPrefs {
  deploy_failed: boolean;
  error_spike: boolean;
  quota: boolean;
}

export interface AccountSummary {
  email: string;
  plan: string;
  plan_status: string | null;
  member_since: number;
  usage: AccountUsage;
  alerts: AlertPrefs;
  resets_at: string;
}

export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

export function when(ms: number | null | undefined): string {
  if (ms == null) {
    return "never";
  }
  return new Date(ms).toLocaleString();
}
