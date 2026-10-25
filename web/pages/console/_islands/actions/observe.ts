// Console observe actions (wave 2): every Logs/Errors query and mutation
// the UI performs goes through this registry, shaped like the other
// `actions/*.ts` modules so the later ⌘K and WebMCP layer can import it
// as-is. Log and error content is app/user text, so every entry that
// returns it sets `untrusted: true`.
import type { ActionContext } from "./settings.ts";
import type {
  ErrorCounts,
  ErrorIssue,
  ErrorsSummary,
  IssueDetail,
  LogFilter,
  LogLine,
  LogsPage,
} from "../api.ts";

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

// The Logs→Errors "View issue" handoff carries no module state: the
// Console island owns the pending fingerprint and passes it to the
// Errors view as props (see console.tsx).

function slugSchema(extra: Record<string, unknown> = {}): object {
  return {
    type: "object",
    properties: {
      slug: { type: "string", description: "The app slug." },
      ...extra,
    },
    required: ["slug"],
    additionalProperties: false,
  };
}

async function request<T>(path: string, init?: RequestInit, signal?: AbortSignal): Promise<T> {
  const res = await fetch(path, { headers: { accept: "application/json" }, ...init, signal });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new Error("signed out");
  }
  if (!res.ok) {
    let message = `request failed: ${res.status}`;
    try {
      const body = (await res.json()) as { message?: string };
      if (body.message) {
        message = body.message;
      }
    } catch {
      // Non-JSON error: keep the status text.
    }
    throw new Error(message);
  }
  return (await res.json()) as T;
}

function logsPath(slug: string, filter: LogFilter & { limit?: number; since?: number; before?: number }): string {
  const q = new URLSearchParams();
  if (filter.q) {
    q.set("q", filter.q);
  }
  if (filter.stream) {
    q.set("stream", filter.stream);
  }
  if (filter.level) {
    q.set("level", filter.level);
  }
  if (filter.from != null) {
    q.set("from", String(filter.from));
  }
  if (filter.to != null) {
    q.set("to", String(filter.to));
  }
  if (filter.limit != null) {
    q.set("limit", String(filter.limit));
  }
  if (filter.since != null) {
    q.set("since", String(filter.since));
  }
  if (filter.before != null) {
    q.set("before", String(filter.before));
  }
  const qs = q.toString();
  return `/api/console/apps/${encodeURIComponent(slug)}/logs${qs ? `?${qs}` : ""}`;
}

export interface QueryLogsInput extends LogFilter {
  slug: string;
  limit?: number;
  since?: number;
  before?: number;
}

const query_logs: ConsoleAction<QueryLogsInput, LogsPage> = {
  name: "query_logs",
  title: "Query logs",
  description:
    "Search an app's recent log lines: substring match (case-insensitive), stream (stdout/stderr) and level (error/warn/info) filters, a millisecond time range, and backwards pagination via before (Load older) or forward polling via since.",
  inputSchema: slugSchema({
    q: { type: "string", description: "Substring match, case-insensitive, at most 200 characters." },
    stream: { type: "string", enum: ["stdout", "stderr"] },
    level: { type: "string", enum: ["error", "warn", "info"] },
    from: { type: "number", description: "Unix milliseconds, inclusive." },
    to: { type: "number", description: "Unix milliseconds, inclusive." },
    limit: { type: "number", description: "Lines per page, 1–500 (default 100)." },
    since: { type: "number", description: "Only lines after this seq (forward poll)." },
    before: { type: "number", description: "Newest lines strictly older than this seq (Load older)." },
  }),
  readOnly: true,
  consequential: false,
  untrusted: true,
  run({ slug, ...filter }: QueryLogsInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<LogsPage> {
    return request<LogsPage>(logsPath(slug, filter), undefined, signal);
  },
};

export interface ListErrorsInput {
  slug: string;
  status?: string;
}

const list_errors: ConsoleAction<ListErrorsInput, ErrorsSummary> = {
  name: "list_errors",
  title: "List errors",
  description:
    "An app's grouped runtime errors, newest first, with per-status counts for the Open/Resolved/Ignored tabs and the proxy 5xx count over the trailing hour.",
  inputSchema: slugSchema({
    status: { type: "string", enum: ["open", "resolved", "ignored"], description: "Unset returns every status." },
  }),
  readOnly: true,
  consequential: false,
  untrusted: true,
  run(input: ListErrorsInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<ErrorsSummary> {
    const qs = input.status ? `?status=${encodeURIComponent(input.status)}` : "";
    return request<ErrorsSummary>(`/api/console/apps/${encodeURIComponent(input.slug)}/errors${qs}`, undefined, signal);
  },
};

export interface ErrorDetailInput {
  slug: string;
  fingerprint: string;
}

const error_detail: ConsoleAction<ErrorDetailInput, IssueDetail> = {
  name: "error_detail",
  title: "Error detail",
  description:
    "One grouped error issue: message, top frame, first/last seen, count, a 24-bucket hourly sparkline over the trailing day, and the most recent matching log lines.",
  inputSchema: slugSchema({
    fingerprint: { type: "string", description: "The issue fingerprint from list_errors." },
  }),
  readOnly: true,
  consequential: false,
  untrusted: true,
  run(input: ErrorDetailInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<IssueDetail> {
    return request<IssueDetail>(
      `/api/console/apps/${encodeURIComponent(input.slug)}/errors/${encodeURIComponent(input.fingerprint)}`,
      undefined,
      signal,
    );
  },
};

export interface TriageInput {
  slug: string;
  fingerprint: string;
}

export interface TriageResult {
  fingerprint: string;
  status: string;
  updated: number;
}

function triageAction(
  name: string,
  segment: string,
  title: string,
  description: string,
): ConsoleAction<TriageInput, TriageResult> {
  return {
    name,
    title,
    description,
    inputSchema: slugSchema({
      fingerprint: { type: "string", description: "The issue fingerprint from list_errors." },
    }),
    readOnly: false,
    // Reversible through reopen; nothing is deleted.
    consequential: false,
    // The answer echoes only the fingerprint and status, never app text.
    untrusted: false,
    run(input: TriageInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<TriageResult> {
      return request<TriageResult>(
        `/api/console/apps/${encodeURIComponent(input.slug)}/errors/${encodeURIComponent(input.fingerprint)}/${segment}`,
        { method: "POST", headers: { "content-type": "application/json" }, body: "{}" },
        signal,
      );
    },
  };
}

export interface BulkTriageInput {
  slug: string;
  action: string;
  fingerprints: string[];
}

export interface BulkTriageResult {
  action: string;
  status: string;
  updated: number;
}

const bulk_triage: ConsoleAction<BulkTriageInput, BulkTriageResult> = {
  name: "bulk_triage",
  title: "Bulk triage errors",
  description: "Resolve, ignore or reopen up to 100 selected error issues at once.",
  inputSchema: slugSchema({
    action: { type: "string", enum: ["resolve", "ignore", "reopen"] },
    fingerprints: { type: "array", items: { type: "string" }, maxItems: 100 },
  }),
  readOnly: false,
  consequential: false,
  untrusted: false,
  run(input: BulkTriageInput, _ctx?: ActionContext, signal?: AbortSignal): Promise<BulkTriageResult> {
    return request<BulkTriageResult>(`/api/console/apps/${encodeURIComponent(input.slug)}/errors/bulk`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ action: input.action, fingerprints: input.fingerprints }),
    }, signal);
  },
};

export const actions: ConsoleAction<never, never>[] = [
  query_logs as ConsoleAction<never, never>,
  list_errors as ConsoleAction<never, never>,
  error_detail as ConsoleAction<never, never>,
  triageAction(
    "resolve_error",
    "resolve",
    "Resolve error",
    "Mark one grouped runtime error resolved: it leaves the open list, and a new event for the same fingerprint reopens it automatically as a regression.",
  ) as ConsoleAction<never, never>,
  triageAction(
    "ignore_error",
    "ignore",
    "Ignore error",
    "Ignore one grouped runtime error: it keeps counting but stays out of the open list and never reopens automatically.",
  ) as ConsoleAction<never, never>,
  triageAction(
    "reopen_error",
    "reopen",
    "Reopen error",
    "Reopen one resolved or ignored error issue as a fresh triage, not a regression.",
  ) as ConsoleAction<never, never>,
  bulk_triage as ConsoleAction<never, never>,
];

export function action<I, O>(name: string): ConsoleAction<I, O> {
  const found = actions.find((a) => a.name === name);
  if (!found) {
    throw new Error(`unknown console action ${name}`);
  }
  return found as ConsoleAction<I, O>;
}

export type { ErrorCounts, ErrorIssue, ErrorsSummary, IssueDetail, LogFilter, LogLine, LogsPage };

// --- Follow (live tail) ----------------------------------------------------
// Server-sent events, not polling: one connection streams `{lines}` pages.
// The view owns visibility (close on hide, reconnect on show); this helper
// owns reconnect backoff: 1 s, 2 s, 5 s, 10 s, then 30 s, reset on message.

export function followLogs(
  slug: string,
  filter: LogFilter,
  since: number | null,
  onPage: (lines: LogLine[]) => void,
  onError: (message: string) => void,
  signal: AbortSignal,
): void {
  let delay = 1000;
  let source: EventSource | null = null;
  let closed = false;
  signal.addEventListener("abort", () => {
    closed = true;
    source?.close();
  }, { once: true });
  function connect(): void {
    if (closed || signal.aborted) {
      return;
    }
    const q = new URLSearchParams();
    if (filter.q) {
      q.set("q", filter.q);
    }
    if (filter.stream) {
      q.set("stream", filter.stream);
    }
    if (filter.level) {
      q.set("level", filter.level);
    }
    if (filter.from != null) {
      q.set("from", String(filter.from));
    }
    if (filter.to != null) {
      q.set("to", String(filter.to));
    }
    if (since != null) {
      q.set("since", String(since));
    }
    const qs = q.toString();
    const url = `/api/console/apps/${encodeURIComponent(slug)}/logs/stream${qs ? `?${qs}` : ""}`;
    const next = new EventSource(url);
    source = next;
    next.onmessage = (event: MessageEvent) => {
      delay = 1000;
      try {
        const page = JSON.parse(event.data) as { lines: LogLine[] };
        const lines = page.lines ?? [];
        if (lines.length > 0) {
          const last = lines[lines.length - 1];
          since = last.seq;
        }
        onPage(lines);
      } catch {
        onError("Couldn't read the live tail. Trying again.");
      }
    };
    next.onerror = () => {
      next.close();
      if (closed || signal.aborted) {
        return;
      }
      source = null;
      onError("Live tail disconnected. Reconnecting.");
      window.setTimeout(connect, delay);
      delay = Math.min(delay * 2, 30000);
    };
  }
  connect();
}
