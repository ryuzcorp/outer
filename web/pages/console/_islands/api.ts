// Shared fetch for the console islands: same-origin JSON, and a missing or
// expired web session sends the human through the Rust sign-in flow.
export async function api<T>(path: string): Promise<T> {
  const res = await fetch(path, { headers: { accept: "application/json" } });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new Error("signed out");
  }
  if (!res.ok) {
    throw new Error(`request failed: ${res.status}`);
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

export interface AppDetail extends ConsoleApp {
  deploys: ConsoleDeploy[];
}

export interface LogLine {
  seq: number;
  ts: number;
  stream: string;
  line: string;
}

export interface ErrorIssue {
  fingerprint: string;
  message: string;
  frame: string | null;
  count: number;
  first_seen: number;
  last_seen: number;
  status: string;
}

export interface ErrorsSummary {
  issues: ErrorIssue[];
  s5xx_last_hour: number;
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

export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

export function when(ms: number | null | undefined): string {
  if (ms == null) {
    return "never";
  }
  return new Date(ms).toLocaleString();
}
