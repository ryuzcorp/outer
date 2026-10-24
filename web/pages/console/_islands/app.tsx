// The `/console/app?app=<slug>` page: header with status, URL and
// visibility, metrics (requests, visitors, errors, latency, top paths),
// the agent action log, the last 20 deploys, and recent logs with a refresh
// button. The slug rides the query string (one static shell for every app).
// Metrics and actions load independently from their own endpoints so one
// slow panel never blocks the other.
import { atom } from "ilha";
import { watch } from "ilha";

import {
  api,
  shortSha,
  when,
  type ActionEntry,
  type ActionsPage,
  type AppDetail,
  type LogLine,
  type MetricsSummary,
} from "./api.ts";

type State =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; app: AppDetail; logs: LogLine[]; logsError: string | null };

type MetricsPanel =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; hours: number; summary: MetricsSummary };

type ActionsPanel =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | {
      kind: "ready";
      actions: ActionEntry[];
      next: string | null;
      loadingMore: boolean;
      moreError: string | null;
    };

const METRICS_RANGES = [
  { hours: 24, label: "24 h" },
  { hours: 168, label: "7 d" },
  { hours: 720, label: "30 d" },
] as const;

function badge(status: string): string {
  switch (status) {
    case "success":
    case "running":
      return "badge badge-success";
    case "building":
    case "deploying":
    case "reviewing":
    case "queued":
      return "badge badge-info";
    case "failed":
    case "blocked":
      return "badge badge-error";
    case "sleeping":
      return "badge badge-neutral";
    default:
      return "badge badge-ghost";
  }
}

function outcomeBadge(outcome: string): string {
  switch (outcome) {
    case "success":
    case "ok":
      return "badge badge-success";
    case "failed":
    case "error":
      return "badge badge-error";
    case "blocked":
      return "badge badge-warning";
    case "building":
      return "badge badge-neutral";
    default:
      return "badge badge-ghost";
  }
}

function timeAgo(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 60_000) {
    return "just now";
  }
  if (diff < 3_600_000) {
    return `${Math.floor(diff / 60_000)}m ago`;
  }
  if (diff < 86_400_000) {
    return `${Math.floor(diff / 3_600_000)}h ago`;
  }
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

function axisLabel(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
  });
}

async function loadLogs(slug: string): Promise<LogLine[]> {
  const data = await api<{ lines: LogLine[] }>(
    `/api/console/apps/${encodeURIComponent(slug)}/logs?limit=100`,
  );
  return data.lines;
}

async function loadMetrics(slug: string, hours: number): Promise<MetricsSummary> {
  return api<MetricsSummary>(
    `/api/console/apps/${encodeURIComponent(slug)}/metrics?hours=${hours}`,
  );
}

async function loadActions(slug: string, before: string | null): Promise<ActionsPage> {
  const cursor = before === null ? "" : `&before=${encodeURIComponent(before)}`;
  return api<ActionsPage>(
    `/api/console/apps/${encodeURIComponent(slug)}/actions?limit=20${cursor}`,
  );
}

function MetricsTiles({ summary }: { summary: MetricsSummary }) {
  const errorRate =
    summary.requests > 0
      ? `${((100 * summary.s5xx) / summary.requests).toFixed(1)}%`
      : "—";
  const tiles: Array<{ label: string; value: string; sub: string; hint?: string }> = [
    { label: "Requests", value: summary.requests.toLocaleString(), sub: "in range" },
    {
      label: "Visitors",
      value: summary.visitors.toLocaleString(),
      sub: "daily unique visitors",
      hint: "Summed daily uniques over the range: a visitor active on N days counts N times.",
    },
    {
      label: "Error rate",
      value: errorRate,
      sub: `${summary.s5xx.toLocaleString()} server errors`,
    },
    { label: "p50 latency", value: `${summary.p50_ms} ms`, sub: "response headers" },
    { label: "p95 latency", value: `${summary.p95_ms} ms`, sub: "response headers" },
  ];
  return (
    <div class="grid grid-cols-2 gap-4 lg:grid-cols-5">
      {tiles.map((tile) => (
        <section
          key={tile.label}
          class="card bg-base-100 border-base-300 w-full border shadow-md"
        >
          <div class="card-body gap-1 p-4">
            <p class="m-0 text-xs opacity-70">{tile.label}</p>
            <p class="m-0 text-2xl font-semibold">{tile.value}</p>
            {tile.hint ? (
              <p class="m-0 text-xs opacity-60">
                <span class="tooltip" data-tip={tile.hint}>
                  {tile.sub}
                </span>
              </p>
            ) : (
              <p class="m-0 text-xs opacity-60">{tile.sub}</p>
            )}
          </div>
        </section>
      ))}
    </div>
  );
}

function RequestsChart({ summary }: { summary: MetricsSummary }) {
  const buckets = summary.buckets;
  // Fixed viewBox: bars share the width whatever the range (24 h–30 d),
  // so axis type keeps one size and dense ranges merge into an area.
  const width = 720;
  const height = 140;
  const plot = 110;
  const step = width / buckets.length;
  const bar = Math.max(step - 0.5, 0.5);
  const max = Math.max(1, ...buckets.map((bucket) => bucket.requests));
  const first = buckets[0];
  const last = buckets[buckets.length - 1];
  return (
    <figure class="flex flex-col gap-1">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        class="w-full"
        role="img"
        aria-label="Hourly requests with 4xx and 5xx stacked"
      >
        {buckets.map((bucket, i) => {
          if (bucket.requests === 0) {
            return null;
          }
          const ok = Math.max(0, bucket.requests - bucket.s4xx - bucket.s5xx);
          const hOk = (ok / max) * plot;
          const h4 = (bucket.s4xx / max) * plot;
          const h5 = (bucket.s5xx / max) * plot;
          const x = i * step;
          return (
            <g key={bucket.hour}>
              <title>
                {`${when(bucket.hour)}: ${bucket.requests} requests, ${bucket.s4xx} 4xx, ${bucket.s5xx} 5xx`}
              </title>
              <rect
                x={x}
                y={plot - hOk}
                width={bar}
                height={Math.max(hOk, 0.5)}
                class="fill-base-300"
              />
              <rect
                x={x}
                y={plot - hOk - h4}
                width={bar}
                height={h4}
                class="fill-warning"
              />
              <rect
                x={x}
                y={plot - hOk - h4 - h5}
                width={bar}
                height={Math.max(h5, bucket.s5xx > 0 ? 0.5 : 0)}
                class="fill-error"
              />
            </g>
          );
        })}
        {first && last && (
          <g>
            <text x={0} y={height - 6} font-size={12} fill="currentColor" opacity={0.6}>
              {axisLabel(first.hour)}
            </text>
            <text
              x={width}
              y={height - 6}
              font-size={12}
              fill="currentColor"
              opacity={0.6}
              text-anchor="end"
            >
              {axisLabel(last.hour)}
            </text>
          </g>
        )}
      </svg>
      <figcaption class="flex gap-4 text-xs opacity-70">
        <span class="flex items-center gap-1">
          <span class="bg-base-300 inline-block h-3 w-3 rounded-sm" /> requests
        </span>
        <span class="flex items-center gap-1">
          <span class="bg-warning inline-block h-3 w-3 rounded-sm" /> 4xx
        </span>
        <span class="flex items-center gap-1">
          <span class="bg-error inline-block h-3 w-3 rounded-sm" /> 5xx
        </span>
      </figcaption>
    </figure>
  );
}

function MetricsSection({
  slug,
  panel,
  onRange,
}: {
  slug: string;
  panel: MetricsPanel;
  onRange: (hours: number) => void;
}) {
  const active = panel.kind === "ready" ? panel.hours : 24;
  return (
    <section class="flex flex-col gap-3">
      <div class="flex items-center justify-between">
        <h3 class="text-lg font-semibold">Metrics</h3>
        <div class="join" role="group" aria-label="Metrics range">
          {METRICS_RANGES.map((range) => (
            <button
              key={range.hours}
              type="button"
              class={`btn btn-sm join-item ${range.hours === active ? "btn-active" : ""}`}
              onclick={() => onRange(range.hours)}
            >
              {range.label}
            </button>
          ))}
        </div>
      </div>
      {panel.kind === "loading" && (
        <div class="flex justify-center py-6">
          <span class="loading loading-spinner" aria-label="Loading metrics" />
        </div>
      )}
      {panel.kind === "error" && (
        <div role="alert" class="alert alert-warning text-sm">
          Couldn't load metrics: {panel.message}
        </div>
      )}
      {panel.kind === "ready" &&
        (panel.summary.requests === 0 ? (
          <div class="flex flex-col gap-3">
            <MetricsTiles summary={panel.summary} />
            <div role="status" class="alert">
              No traffic yet in this range.
            </div>
          </div>
        ) : (
          <div class="flex flex-col gap-3">
            <MetricsTiles summary={panel.summary} />
            <RequestsChart summary={panel.summary} />
            <div class="overflow-x-auto">
              <table class="table table-sm">
                <thead>
                  <tr>
                    <th>Path</th>
                    <th>Requests</th>
                    <th>5xx</th>
                  </tr>
                </thead>
                <tbody>
                  {panel.summary.top_paths.map((row) => (
                    <tr key={row.path}>
                      <td class="font-mono break-all">{row.path}</td>
                      <td>{row.requests.toLocaleString()}</td>
                      <td>{row.s5xx.toLocaleString()}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        ))}
    </section>
  );
}

function ActionsSection({
  panel,
  onMore,
}: {
  panel: ActionsPanel;
  onMore: () => void;
}) {
  return (
    <section class="flex flex-col gap-2">
      <h3 class="text-lg font-semibold">Agent actions</h3>
      {panel.kind === "loading" && (
        <div class="flex justify-center py-6">
          <span class="loading loading-spinner" aria-label="Loading agent actions" />
        </div>
      )}
      {panel.kind === "error" && (
        <div role="alert" class="alert alert-warning text-sm">
          Couldn't load agent actions: {panel.message}
        </div>
      )}
      {panel.kind === "ready" &&
        (panel.actions.length === 0 ? (
          <div role="status" class="alert">
            No agent actions yet.
          </div>
        ) : (
          <div class="flex flex-col gap-2">
            <div class="overflow-x-auto">
              <table class="table table-sm">
                <thead>
                  <tr>
                    <th>Time</th>
                    <th>Client</th>
                    <th>Tool</th>
                    <th>Summary</th>
                    <th>Outcome</th>
                    <th>Duration</th>
                  </tr>
                </thead>
                <tbody>
                  {panel.actions.map((action) => (
                    <tr key={action.id}>
                      <td title={when(action.at)}>{timeAgo(action.at)}</td>
                      <td>{action.by}</td>
                      <td class="font-mono">{action.tool}</td>
                      <td class="font-mono text-xs whitespace-pre-wrap break-all">
                        {action.summary}
                      </td>
                      <td>
                        <span class={outcomeBadge(action.outcome)}>{action.outcome}</span>
                        {action.error_code && (
                          <span class="font-mono ml-1 text-xs opacity-70">
                            {action.error_code}
                          </span>
                        )}
                      </td>
                      <td>{`${(action.duration_ms / 1000).toFixed(1)}s`}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {panel.moreError && (
              <div role="alert" class="alert alert-warning text-sm">
                Couldn't load more actions: {panel.moreError}
              </div>
            )}
            {panel.next && (
              <button
                type="button"
                class="btn btn-sm self-start"
                disabled={panel.loadingMore}
                onclick={onMore}
              >
                {panel.loadingMore ? "Loading…" : "Load more"}
              </button>
            )}
          </div>
        ))}
    </section>
  );
}

export function AppView() {
  const state = atom<State>({ kind: "loading" });
  const metrics = atom<MetricsPanel>({ kind: "loading" });
  const actions = atom<ActionsPanel>({ kind: "loading" });
  const slug =
    typeof window === "undefined"
      ? null
      : new URLSearchParams(window.location.search).get("app");

  async function refresh(slug: string): Promise<void> {
    try {
      const logs = await loadLogs(slug);
      const current = state();
      if (current.kind === "ready") {
        state.set({ ...current, logs, logsError: null });
      }
    } catch (err: unknown) {
      const current = state();
      if (current.kind === "ready") {
        state.set({ ...current, logsError: String(err) });
      }
    }
  }

  function pickRange(slug: string, hours: number): void {
    const current = metrics();
    if (current.kind === "ready" && current.hours === hours) {
      return;
    }
    metrics.set({ kind: "loading" });
    loadMetrics(slug, hours)
      .then((summary) => {
        metrics.set({ kind: "ready", hours, summary });
      })
      .catch((err: unknown) => {
        metrics.set({ kind: "error", message: String(err) });
      });
  }

  function loadMore(slug: string): void {
    const current = actions();
    if (current.kind !== "ready" || current.next === null || current.loadingMore) {
      return;
    }
    const before = current.next;
    actions.set({ ...current, loadingMore: true, moreError: null });
    loadActions(slug, before)
      .then((page) => {
        const latest = actions();
        if (latest.kind !== "ready") {
          return;
        }
        actions.set({
          ...latest,
          actions: [...latest.actions, ...page.actions],
          next: page.next,
          loadingMore: false,
        });
      })
      .catch((err: unknown) => {
        const latest = actions();
        if (latest.kind !== "ready") {
          return;
        }
        actions.set({ ...latest, loadingMore: false, moreError: String(err) });
      });
  }

  watch.once(() => {
    if (typeof window === "undefined" || !slug) {
      return;
    }
    const name = slug;
    api<AppDetail>(`/api/console/apps/${encodeURIComponent(name)}`)
      .then(async (app) => {
        const logs = await loadLogs(app.slug).catch(() => null);
        state.set({
          kind: "ready",
          app,
          logs: logs ?? [],
          logsError: logs === null ? "Couldn't load logs." : null,
        });
        loadMetrics(app.slug, 24)
          .then((summary) => {
            metrics.set({ kind: "ready", hours: 24, summary });
          })
          .catch((err: unknown) => {
            metrics.set({ kind: "error", message: String(err) });
          });
        loadActions(app.slug, null)
          .then((page) => {
            actions.set({
              kind: "ready",
              actions: page.actions,
              next: page.next,
              loadingMore: false,
              moreError: null,
            });
          })
          .catch((err: unknown) => {
            actions.set({ kind: "error", message: String(err) });
          });
      })
      .catch((err: unknown) => {
        state.set({ kind: "error", message: String(err) });
        metrics.set({ kind: "error", message: String(err) });
        actions.set({ kind: "error", message: String(err) });
      });
  });

  const current = state();
  if (current.kind === "loading") {
    if (typeof window !== "undefined" && !slug) {
      return (
        <div role="alert" class="alert alert-warning">
          No app selected. <a class="link" href="/console">Back to your apps</a>.
        </div>
      );
    }
    return (
      <div class="flex justify-center py-10">
        <span class="loading loading-spinner" aria-label="Loading app" />
      </div>
    );
  }
  if (current.kind === "error") {
    return (
      <div class="flex flex-col gap-4">
        <div role="alert" class="alert alert-error">
          Couldn't load this app (unknown or not yours): {current.message}
        </div>
        <a class="link" href="/console">
          Back to your apps
        </a>
      </div>
    );
  }
  const { app, logs, logsError } = current;
  return (
    <div class="flex flex-col gap-6">
      <a class="link text-sm" href="/console">
        ← Your apps
      </a>
      <header class="flex flex-wrap items-center justify-between gap-2">
        <h2 class="text-2xl font-bold">{app.slug}</h2>
        <span class={badge(app.status)}>{app.status}</span>
      </header>
      <dl class="flex flex-col gap-1 text-sm">
        <div class="flex gap-2">
          <dt class="opacity-70">URL</dt>
          <dd>
            <a class="link break-all" href={app.url}>
              {app.url}
            </a>
          </dd>
        </div>
        <div class="flex gap-2">
          <dt class="opacity-70">Visibility</dt>
          <dd>{app.visibility}</dd>
        </div>
        <div class="flex gap-2">
          <dt class="opacity-70">Last request</dt>
          <dd>{when(app.last_request_at)}</dd>
        </div>
      </dl>

      <MetricsSection
        slug={app.slug}
        panel={metrics()}
        onRange={(hours) => pickRange(app.slug, hours)}
      />

      <ActionsSection panel={actions()} onMore={() => loadMore(app.slug)} />

      <section class="flex flex-col gap-2">
        <h3 class="text-lg font-semibold">Deploys</h3>
        {app.deploys.length === 0 ? (
          <div role="status" class="alert">
            No deploys yet.
          </div>
        ) : (
          <div class="overflow-x-auto">
            <table class="table table-sm">
              <thead>
                <tr>
                  <th>Commit</th>
                  <th>Status</th>
                  <th>Duration</th>
                  <th>Findings</th>
                  <th>Created</th>
                </tr>
              </thead>
              <tbody>
                {app.deploys.map((deploy) => (
                  <tr key={deploy.deploy_id}>
                    <td class="font-mono">{shortSha(deploy.sha)}</td>
                    <td>
                      <span class={badge(deploy.status)}>{deploy.status}</span>
                    </td>
                    <td>
                      {deploy.duration_ms == null
                        ? "—"
                        : `${(deploy.duration_ms / 1000).toFixed(1)}s`}
                    </td>
                    <td>{deploy.findings_count}</td>
                    <td>{when(deploy.created_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {app.deploys.some((deploy) => deploy.message) && (
          <ul class="flex flex-col gap-2">
            {app.deploys
              .filter((deploy) => deploy.message)
              .map((deploy) => (
                <li key={deploy.deploy_id} role="alert" class="alert alert-error text-sm">
                  <span class="font-mono">{shortSha(deploy.sha)}</span> {deploy.message}
                </li>
              ))}
          </ul>
        )}
      </section>

      <section class="flex flex-col gap-2">
        <div class="flex items-center justify-between">
          <h3 class="text-lg font-semibold">Recent logs</h3>
          <button
            type="button"
            class="btn btn-sm"
            onclick={() => void refresh(app.slug)}
          >
            Refresh
          </button>
        </div>
        {logsError && (
          <div role="alert" class="alert alert-warning text-sm">
            {logsError}
          </div>
        )}
        {logs.length === 0 ? (
          <div role="status" class="alert">
            No log lines yet.
          </div>
        ) : (
          <pre class="mockup-code text-xs">
            {logs.map((entry) => (
              <code key={entry.seq} data-prefix={entry.stream === "stderr" ? "!" : ">"}>
                {entry.line}
              </code>
            ))}
          </pre>
        )}
      </section>
    </div>
  );
}
