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

export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

export function when(ms: number | null | undefined): string {
  if (ms == null) {
    return "never";
  }
  return new Date(ms).toLocaleString();
}
