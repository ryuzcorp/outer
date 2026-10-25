// Console views: one panel per sidebar item.
// Data shapes ported from Noite's $lib/app-detail/{metrics,deploys,errors,
// events,logs}.tsx; interaction details follow Noite (loading skeletons,
// empty states, relative times with absolute titles on hover, copy buttons
// for shas/URLs). Charts are monochrome inline SVG: near-black series,
// light-gray comparison series, dashed vertical gridlines, minimal axes,
// muted tick labels. No `style=` attributes — the console CSP only hashes
// inline scripts/styles, so all sizing rides on classes and SVG attrs.
//
// Overflow armor: every flex/grid child that holds text carries `min-w-0`,
// every label, cell, slug, URL, sha, path, summary and message truncates
// with an ellipsis (`truncate`) and exposes the full text via `title`.
// Tables are `table-fixed` with column widths. `break-words` is reserved
// for text that should wrap: log lines and the expanded deploy message.
import { atom } from "ilha";
import {
  shortSha,
  when,
  type AdminInvitesPage,
  type AppDetail,
  type ConsoleDeploy,
  type ErrorsSummary,
  type InviteEntry,
  type InvitesSummary,
  type LogLine,
  type MetricsBucket,
  type MetricsSummary,
  type SecretsSummary,
  type WaitlistSummary,
} from "./api.ts";
import {
  CopyButton,
  timeAgo,
  type ActionsPanel,
  type Panel,
  type ViewId,
} from "./common.tsx";
import {
  ActionsIcon,
  DeploysIcon,
  ErrorsIcon,
  LogsIcon,
  MetricsIcon,
  OverviewIcon,
  SecretsIcon,
  TicketIcon,
} from "./icons.tsx";

// --- Small pieces ------------------------------------------------------------

function Loading({ label }: { label: string }) {
  return (
    <div class="flex min-w-0 flex-col gap-3" role="status" aria-label={label}>
      <div class="grid min-w-0 grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4" aria-hidden="true">
        <div class="skeleton rounded-box h-28 w-full" />
        <div class="skeleton rounded-box h-28 w-full" />
        <div class="skeleton rounded-box h-28 w-full" />
        <div class="skeleton rounded-box h-28 w-full" />
        <div class="skeleton rounded-box h-64 w-full md:col-span-2 xl:col-span-4" />
      </div>
      <span class="sr-only">Loading…</span>
    </div>
  );
}

function Err({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="alert alert-error alert-soft min-w-0" role="alert">
        <span class="min-w-0 break-words">Couldn't load this view: {message}</span>
      </div>
      {onRetry && (
        <div class="min-w-0">
          <button type="button" class="btn btn-sm" onclick={onRetry}>
            Retry
          </button>
        </div>
      )}
    </div>
  );
}

// One shared empty state: an icon in a soft circle, a title, a muted line,
// and an optional action. `plain` skips the card chrome for empties that
// already sit inside a card (Overview rows, log lines).
function EmptyState({
  icon,
  title,
  children,
  action,
  plain,
}: {
  icon?: unknown;
  title: string;
  children?: unknown;
  action?: unknown;
  plain?: boolean;
}) {
  const inner = (
    <div class="contents">
      {icon && (
        <span class="bg-base-200 flex h-11 w-11 items-center justify-center rounded-full opacity-70" aria-hidden="true">
          {icon}
        </span>
      )}
      <h3 class="m-0 min-w-0 text-base font-semibold">{title}</h3>
      {children && <p class="m-0 min-w-0 text-sm opacity-60">{children}</p>}
      {action}
    </div>
  );
  if (plain) {
    return (
      <div class="flex min-w-0 flex-col items-center gap-2 py-6 text-center" role="status" aria-label={title}>
        {inner}
      </div>
    );
  }
  return (
    <section class="card card-border bg-base-100 min-w-0" role="status" aria-label={title}>
      <div class="card-body items-center gap-2 py-8 text-center">
        {inner}
      </div>
    </section>
  );
}

function compact(n: number): string {
  if (n >= 1000) {
    const v = (n / 1000).toFixed(1).replace(/\.0$/, "");
    return `${v}k`;
  }
  return `${n}`;
}

// daisyUI badge variants for app/deploy status and action outcomes.
function statusBadge(status: string): string {
  if (status === "success" || status === "running" || status === "ok") {
    return "badge-success";
  }
  if (
    status === "building" ||
    status === "deploying" ||
    status === "reviewing" ||
    status === "queued"
  ) {
    return "badge-info";
  }
  if (status === "failed" || status === "error" || status === "blocked") {
    return "badge-error";
  }
  if (status === "sleeping") {
    return "badge-ghost";
  }
  return "badge-ghost";
}

function outcomeBadge(outcome: string): string {
  if (outcome === "success" || outcome === "ok") {
    return "badge-success";
  }
  if (outcome === "failed" || outcome === "error") {
    return "badge-error";
  }
  if (outcome === "blocked") {
    return "badge-warning";
  }
  return "badge-ghost";
}

function axisLabel(ms: number, hours: number): string {
  const d = new Date(ms);
  if (hours <= 24) {
    return d.toLocaleString(undefined, { hour: "numeric" });
  }
  return d.toLocaleString(undefined, { month: "short", day: "numeric" });
}

function bucketTitle(bucket: MetricsBucket, hours: number): string {
  const label = new Date(bucket.hour).toLocaleString(undefined,
    hours <= 24
      ? { hour: "numeric" }
      : { month: "short", day: "numeric", hour: "numeric" },
  );
  return `${label}: ${bucket.requests} requests, ${bucket.s4xx} 4xx, ${bucket.s5xx} 5xx`;
}

// --- Stat cards --------------------------------------------------------------

function StatCard({
  icon,
  label,
  value,
  pill,
  pillTitle,
  sub,
}: {
  icon?: unknown;
  label: string;
  value: string;
  pill?: string | null;
  /** What the pill compares, shown on hover (e.g. the half-range delta). */
  pillTitle?: string;
  sub?: string;
}) {
  return (
    <div class="stat min-w-0">
      <div class="stat-title flex min-w-0 items-center gap-2">
        {icon}
        <span class="min-w-0 flex-1 truncate" title={label}>{label}</span>
      </div>
      <div class="stat-value min-w-0 truncate text-3xl font-medium" title={pill ? `${value} ${pill}` : value}>
        {value}
        {pill != null && pill !== "" && (
          <span class="badge badge-soft badge-sm ml-2 shrink-0 align-middle" title={pillTitle}>{pill}</span>
        )}
      </div>
      {sub && <div class="stat-desc min-w-0 truncate" title={sub}>{sub}</div>}
    </div>
  );
}

// --- Charts ------------------------------------------------------------------
// RequestsLine: dark requests line over a light-gray errors line, dashed
// vertical gridlines, muted end ticks. Bars: Noite's metrics.tsx BarChart
// (fixed viewBox, per-bar <title>, axis end labels).

function RequestsLine({
  buckets,
  hours,
}: {
  buckets: MetricsBucket[];
  hours: number;
}) {
  const width = 720;
  const height = 240;
  const padL = 44;
  const padB = 26;
  const padT = 14;
  const plotW = width - padL - 8;
  const plotH = height - padT - padB;
  const max = Math.max(1, ...buckets.map((b) => b.requests));
  const yTicks = niceTicks(max);
  const top = Math.max(1, yTicks[yTicks.length - 1]);
  const n = buckets.length;
  const x = (i: number) =>
    n === 1 ? padL + plotW / 2 : padL + (i * plotW) / (n - 1);
  const y = (v: number) => padT + plotH * (1 - v / top);
  const line = (pick: (b: MetricsBucket) => number) =>
    buckets.map((b, i) => `${x(i).toFixed(1)},${y(pick(b)).toFixed(1)}`).join(" ");
  const area =
    `${padL},${(padT + plotH).toFixed(1)} ` +
    line((b) => b.requests) +
    ` ${x(n - 1).toFixed(1)},${(padT + plotH).toFixed(1)}`;
  const guides = [0, 1, 2, 3, 4, 5];
  const first = buckets[0];
  const last = buckets[n - 1];
  return (
    <figure class="chart-block">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        width="100%"
        role="img"
        aria-label="Requests over time with errors for comparison"
      >
        {yTicks.map((t) => (
          <g key={t}>
            <line
              x1={padL}
              y1={y(t)}
              x2={padL + plotW}
              y2={y(t)}
              stroke="currentColor"
              stroke-width="1"
              stroke-dasharray="3 4"
              opacity="0.25"
            />
            <text
              x={padL - 6}
              y={y(t) + 4}
              font-size="11"
              fill="currentColor"
              opacity="0.55"
              text-anchor="end"
            >
              {compact(t)}
            </text>
          </g>
        ))}
        {guides.map((t) => {
          const gx = padL + (t * plotW) / 5;
          return (
            <line
              key={t}
              x1={gx}
              y1={padT}
              x2={gx}
              y2={padT + plotH}
              stroke="currentColor"
              stroke-width="1"
              stroke-dasharray="3 4"
              opacity="0.25"
            />
          );
        })}
        <polygon points={area} fill="currentColor" opacity="0.05" />
        <polyline
          points={line((b) => b.s4xx + b.s5xx)}
          fill="none"
          stroke="currentColor"
          stroke-width="1.5"
          opacity="0.3"
        />
        <polyline
          points={line((b) => b.requests)}
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        />
        {first && last && (
          <g font-size="11" fill="currentColor" opacity="0.55">
            <text x={padL} y={height - 8}>
              {axisLabel(first.hour, hours)}
            </text>
            <text x={padL + plotW} y={height - 8} text-anchor="end">
              {axisLabel(last.hour, hours)}
            </text>
          </g>
        )}
      </svg>
    </figure>
  );
}

function Bars({
  buckets,
  hours,
}: {
  buckets: MetricsBucket[];
  hours: number;
}) {
  // 30 d of hourly buckets would render sub-pixel: roll up to days so
  // bars stay readable. Fewer than ~200 buckets keep hourly bars.
  let bars = buckets;
  let grouped = false;
  if (buckets.length > 200) {
    const days: MetricsBucket[] = [];
    for (let i = 0; i < buckets.length; i += 24) {
      const slice = buckets.slice(i, i + 24);
      days.push({
        hour: slice[0].hour,
        requests: slice.reduce((a, b) => a + b.requests, 0),
        s4xx: slice.reduce((a, b) => a + b.s4xx, 0),
        s5xx: slice.reduce((a, b) => a + b.s5xx, 0),
      });
    }
    bars = days;
    grouped = true;
  }
  const width = 720;
  const height = 200;
  const padL = 44;
  const padB = 26;
  const padT = 14;
  const plotW = width - padL - 8;
  const plotH = height - padT - padB;
  const max = Math.max(1, ...bars.map((b) => b.requests));
  const yTicks = niceTicks(max);
  const top = Math.max(1, yTicks[yTicks.length - 1]);
  const y = (v: number) => padT + plotH * (1 - v / top);
  const step = plotW / Math.max(1, bars.length);
  // Thin near-black bars like the reference (2–3 px at card width).
  const bar = Math.min(3, Math.max(2, step * 0.55));
  const barX = (i: number) => padL + i * step + (step - bar) / 2;
  const titleFor = (b: MetricsBucket): string =>
    grouped
      ? `${axisLabel(b.hour, 168)}: ${b.requests} requests, ${b.s4xx} 4xx, ${b.s5xx} 5xx`
      : bucketTitle(b, hours);
  const first = bars[0];
  const last = bars[bars.length - 1];
  return (
    <figure class="chart-block">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        width="100%"
        role="img"
        aria-label={grouped ? "Daily requests as bars" : "Hourly requests as bars"}
      >
        {yTicks.map((t) => (
          <g key={t}>
            <line
              x1={padL}
              y1={y(t)}
              x2={padL + plotW}
              y2={y(t)}
              stroke="currentColor"
              stroke-width="1"
              stroke-dasharray="3 4"
              opacity="0.25"
            />
            <text
              x={padL - 6}
              y={y(t) + 4}
              font-size="11"
              fill="currentColor"
              opacity="0.55"
              text-anchor="end"
            >
              {compact(t)}
            </text>
          </g>
        ))}
        {bars.map((b, i) => {
          const h = Math.max(1.5, (b.requests / top) * (plotH - 2));
          return (
            <g key={b.hour}>
              <rect
                x={barX(i)}
                y={padT + plotH - h}
                width={bar}
                height={h}
                fill="currentColor"
              />
              <rect
                x={padL + i * step}
                y={padT}
                width={step}
                height={plotH}
                fill="transparent"
              >
                <title>{titleFor(b)}</title>
              </rect>
            </g>
          );
        })}
      </svg>
      <figcaption class="mt-1 flex min-w-0 justify-between gap-3 text-xs opacity-60" aria-hidden="true">
        <span class="min-w-0 truncate">{first ? axisLabel(first.hour, grouped ? 168 : hours) : ""}</span>
        <span class="min-w-0 truncate">{last ? axisLabel(last.hour, grouped ? 168 : hours) : ""}</span>
      </figcaption>
    </figure>
  );
}

// Second-half vs first-half percentage change (the stat-card delta pills).
function halfChange(buckets: MetricsBucket[]): string | null {
  if (buckets.length < 2) {
    return null;
  }
  const mid = Math.floor(buckets.length / 2);
  const first = buckets
    .slice(0, mid)
    .reduce((a, b) => a + b.requests, 0);
  const second = buckets.slice(mid).reduce((a, b) => a + b.requests, 0);
  if (first === 0) {
    return second > 0 ? "new" : null;
  }
  const pct = Math.round(((second - first) / first) * 100);
  return `${pct >= 0 ? "+" : ""}${pct}%`;
}

// Error rate: exact 0% only when there are no 5xx; sub-1% rates keep two
// decimals so e.g. 6 / 28,845 reads 0.02% instead of 0.0%.
function fmtRate(s5xx: number, requests: number): string {
  if (requests <= 0) {
    return "—";
  }
  if (s5xx <= 0) {
    return "0%";
  }
  const pct = (100 * s5xx) / requests;
  if (pct >= 1) {
    return `${pct.toFixed(1)}%`;
  }
  if (pct >= 0.005) {
    return `${pct.toFixed(2)}%`;
  }
  return "<0.01%";
}

// Y-axis ticks with a round ceiling (0, 10, 20, … like the reference).
function niceTicks(max: number, count = 5): number[] {
  if (max <= 0) {
    return [0];
  }
  const raw = max / (count - 1);
  const mag = 10 ** Math.floor(Math.log10(raw));
  const norm = raw / mag;
  const step = (norm >= 5 ? 10 : norm >= 2 ? 5 : norm >= 1 ? 2 : 1) * mag;
  const top = Math.ceil(max / step) * step;
  const ticks: number[] = [];
  for (let v = 0; v <= top + step / 2; v += step) {
    ticks.push(Math.round((v / step)) * step);
  }
  return ticks;
}

// --- Overview ------------------------------------------------------------------

export function OverviewView({
  app,
  metrics,
  actions,
  errors,
  onView,
  onRetry,
}: {
  app: AppDetail;
  metrics: Panel<{ hours: number; summary: MetricsSummary }>;
  actions: ActionsPanel;
  errors: Panel<ErrorsSummary>;
  onView: (view: ViewId) => void;
  onRetry: () => void;
}) {
  return (
    <div class="flex min-w-0 flex-col gap-3">
      {metrics.kind === "loading" && <Loading label="Loading overview" />}
      {metrics.kind === "error" && <Err message={metrics.message} onRetry={onRetry} />}
      {metrics.kind === "ready" && (
        <OverviewReady
          summary={metrics.value.summary}
          hours={metrics.value.hours}
        />
      )}
      <div class="grid min-w-0 grid-cols-1 gap-3 xl:grid-cols-2">
        <section class="card card-border bg-base-100 min-w-0" aria-label="Latest deploy">
          <div class="card-body min-w-0">
            <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
              <h2 class="card-title min-w-0 truncate" title="Latest deploy">Latest deploy</h2>
              <button
                type="button"
                class="link shrink-0 text-sm"
                onclick={() => onView("deploys")}
              >
                View all
              </button>
            </div>
            {app.deploys.length === 0 ? (
              <EmptyState plain icon={<DeploysIcon />} title="No deploys yet">
                Deploys your agent ships will show up here.
              </EmptyState>
            ) : (
              <ul class="list min-w-0">
                <DeployRow deploy={app.deploys[0]} />
              </ul>
            )}
          </div>
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="Latest agent actions">
          <div class="card-body min-w-0">
            <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
              <h2 class="card-title min-w-0 truncate" title="Latest agent actions">Latest agent actions</h2>
              <button
                type="button"
                class="link shrink-0 text-sm"
                onclick={() => onView("actions")}
              >
                View all
              </button>
            </div>
            {actions.kind === "loading" && (
              <div class="flex min-w-0 flex-col gap-2" role="status" aria-label="Loading actions">
                <div class="skeleton h-8 w-full" />
                <div class="skeleton h-8 w-full" />
                <div class="skeleton h-8 w-full" />
              </div>
            )}
            {actions.kind === "error" && (
              <p class="m-0 min-w-0 break-words opacity-60" role="alert">
                Couldn't load actions: {actions.message}
              </p>
            )}
            {actions.kind === "ready" &&
              (actions.state.actions.length === 0 ? (
                <EmptyState plain icon={<ActionsIcon />} title="No agent actions yet">
                  Your agent's tool calls will show up here.
                </EmptyState>
              ) : (
                <ul class="list min-w-0">
                  {actions.state.actions.slice(0, 5).map((a) => (
                    <li key={a.id} class="list-row min-w-0 text-[13px]">
                      <span class={`badge badge-soft ${outcomeBadge(a.outcome)} shrink-0`} title={a.outcome}>
                        {a.outcome}
                      </span>
                      <span class="min-w-0 flex-1 truncate font-mono text-xs" title={a.summary}>
                        {a.summary || a.tool}
                      </span>
                      <span class="shrink-0 whitespace-nowrap text-xs opacity-60" title={when(a.at)}>
                        {timeAgo(a.at)}
                      </span>
                    </li>
                  ))}
                </ul>
              ))}
          </div>
        </section>
      </div>
      {errors.kind === "ready" && errors.value.issues.length > 0 && (
        <section class="card card-border bg-base-100 min-w-0" aria-label="Open errors">
          <div class="card-body min-w-0">
            <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
              <h2 class="card-title min-w-0 truncate" title="Open errors">Open errors</h2>
              <button
                type="button"
                class="link shrink-0 text-sm"
                onclick={() => onView("errors")}
              >
                View all
              </button>
            </div>
            <ul class="list min-w-0">
              {errors.value.issues
                .filter((i) => i.status === "open")
                .slice(0, 3)
                .map((i) => (
                  <li key={i.fingerprint} class="list-row min-w-0 text-[13px]">
                    <span class="min-w-0 flex-1 truncate font-mono text-xs" title={i.message}>
                      {i.message}
                    </span>
                    <span class="shrink-0 whitespace-nowrap text-xs opacity-60">
                      {i.count.toLocaleString()}× ·{" "}
                      <span title={when(i.last_seen)}>{timeAgo(i.last_seen)}</span>
                    </span>
                  </li>
                ))}
            </ul>
          </div>
        </section>
      )}
    </div>
  );
}

function OverviewReady({
  summary,
  hours,
}: {
  summary: MetricsSummary;
  hours: number;
}) {
  const errorRate = fmtRate(summary.s5xx, summary.requests);
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="grid min-w-0 grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4">
        <section class="card card-border bg-base-100 min-w-0" aria-label="Requests">
          <StatCard
            icon={<MetricsIcon size={16} />}
            label="Requests"
            value={summary.requests.toLocaleString()}
            pill={halfChange(summary.buckets)}
            pillTitle="Requests in the second half of the range vs the first half"
            sub="in range"
          />
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="Visitors">
          <StatCard
            icon={<OverviewIcon size={16} />}
            label="Visitors"
            value={summary.visitors.toLocaleString()}
            sub="daily uniques, summed"
          />
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="Error rate">
          <StatCard
            icon={<ErrorsIcon size={16} />}
            label="Error rate"
            value={errorRate}
            pill={summary.s5xx > 0 ? `${summary.s5xx.toLocaleString()} 5xx` : null}
            sub="server errors share"
          />
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="p95 latency">
          <StatCard
            icon={<ActionsIcon size={16} />}
            label="p95 latency"
            value={`${summary.p95_ms} ms`}
            pill={`p50 ${summary.p50_ms} ms`}
            sub="response headers"
          />
        </section>
      </div>
      <section class="min-w-0" aria-label="Requests chart">
        <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
          <h2 class="card-title min-w-0 truncate" title="Requests">Requests</h2>
          <div class="flex shrink-0 items-center gap-4 text-xs opacity-60">
            <span class="flex min-w-0 items-center gap-1.5">
              <span class="status status-neutral" aria-hidden="true" />
              This period
            </span>
            <span class="flex min-w-0 items-center gap-1.5">
              <span class="status" aria-hidden="true" />
              Errors (4xx + 5xx)
            </span>
          </div>
        </div>
        {summary.buckets.length === 0 ? (
          <EmptyState plain title="No traffic in this range">
            Requests your app serves will show up here.
          </EmptyState>
        ) : (
          <RequestsLine buckets={summary.buckets} hours={hours} />
        )}
      </section>
    </div>
  );
}

// --- Metrics -------------------------------------------------------------------
export function MetricsView({
  metrics,
  onRetry,
}: {
  metrics: Panel<{ hours: number; summary: MetricsSummary }>;
  onRetry: () => void;
}) {
  if (metrics.kind === "loading") {
    return <Loading label="Loading metrics" />;
  }
  if (metrics.kind === "error") {
    return <Err message={metrics.message} onRetry={onRetry} />;
  }
  const { summary, hours } = metrics.value;
  const errorRate = fmtRate(summary.s5xx, summary.requests);
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <div class="grid min-w-0 grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4">
        <section class="card card-border bg-base-100 min-w-0" aria-label="Requests">
          <StatCard
            icon={<MetricsIcon size={16} />}
            label="Requests"
            value={summary.requests.toLocaleString()}
            pill={halfChange(summary.buckets)}
            pillTitle="Requests in the second half of the range vs the first half"
            sub="in range"
          />
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="Visitors">
          <StatCard
            icon={<OverviewIcon size={16} />}
            label="Visitors"
            value={summary.visitors.toLocaleString()}
            sub="daily uniques, summed"
          />
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="Error rate">
          <StatCard
            icon={<ErrorsIcon size={16} />}
            label="Error rate"
            value={errorRate}
            pill={summary.s5xx > 0 ? `${summary.s5xx.toLocaleString()} 5xx` : null}
            sub={`${summary.s4xx.toLocaleString()} client errors (4xx)`}
          />
        </section>
        <section class="card card-border bg-base-100 min-w-0" aria-label="p95 latency">
          <StatCard
            icon={<ActionsIcon size={16} />}
            label="p95 latency"
            value={`${summary.p95_ms} ms`}
            pill={`p50 ${summary.p50_ms} ms`}
            sub="response headers"
          />
        </section>
      </div>
      <section class="card card-border bg-base-100 min-w-0" aria-label={summary.buckets.length > 200 ? "Daily requests" : "Hourly requests"}>
        <div class="card-body min-w-0">
          <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
            <h2 class="card-title min-w-0 truncate" title={summary.buckets.length > 200 ? "Daily requests" : "Hourly requests"}>{summary.buckets.length > 200 ? "Daily requests" : "Hourly requests"}</h2>
          </div>
          {summary.buckets.length === 0 ? (
            <EmptyState plain title="No traffic in this range">
              Requests your app serves will show up here.
            </EmptyState>
          ) : (
            <Bars buckets={summary.buckets} hours={hours} />
          )}
        </div>
      </section>
      <section class="card card-border bg-base-100 min-w-0" aria-label="Top paths">
        <div class="card-body min-w-0">
          <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
            <h2 class="card-title min-w-0 truncate" title="Top paths">Top paths</h2>
          </div>
          {summary.top_paths.length === 0 ? (
            <EmptyState plain title="No paths recorded in this range">
              Paths your app serves will show up here.
            </EmptyState>
          ) : (
            <div class="min-w-0 overflow-x-auto">
              <table class="table table-fixed w-full">
                <colgroup>
                  <col />
                  <col class="w-24" />
                  <col class="w-20" />
                </colgroup>
                <thead>
                  <tr>
                    <th scope="col">Path</th>
                    <th scope="col">Requests</th>
                    <th scope="col">5xx</th>
                  </tr>
                </thead>
                <tbody>
                  {summary.top_paths.map((p) => (
                    <tr key={p.path}>
                      <td class="truncate font-mono" title={p.path}>
                        {p.path}
                      </td>
                      <td class="whitespace-nowrap">{p.requests.toLocaleString()}</td>
                      <td class="whitespace-nowrap">{p.s5xx.toLocaleString()}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

// --- Actions -------------------------------------------------------------------
// Ported from Noite's events.tsx: newest-first feed, cursor pagination.

export function ActionsView({
  panel,
  onMore,
  onRetry,
}: {
  panel: ActionsPanel;
  onMore: () => void;
  onRetry: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading agent actions" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} onRetry={onRetry} />;
  }
  const { actions, next, loadingMore, moreError } = panel.state;
  if (actions.length === 0) {
    return (
      <EmptyState icon={<ActionsIcon />} title="No agent actions yet">
        Your agent's tool calls will show up here.
      </EmptyState>
    );
  }
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="Agent actions">
      <div class="card-body min-w-0">
        <div class="min-w-0 overflow-x-auto">
          <table class="table table-fixed w-full">
            <colgroup>
              <col class="w-20" />
              <col class="w-24" />
              <col class="w-28" />
              <col />
              <col class="w-28" />
              <col class="w-20" />
            </colgroup>
            <thead>
              <tr>
                <th scope="col">Time</th>
                <th scope="col">Client</th>
                <th scope="col">Tool</th>
                <th scope="col">Summary</th>
                <th scope="col">Outcome</th>
                <th scope="col">Duration</th>
              </tr>
            </thead>
            <tbody>
              {actions.map((a) => (
                <tr key={a.id}>
                  <td class="whitespace-nowrap" title={when(a.at)}>
                    {timeAgo(a.at)}
                  </td>
                  <td class="truncate" title={a.by}>{a.by}</td>
                  <td class="truncate font-mono" title={a.tool}>{a.tool}</td>
                  <td class="truncate font-mono" title={a.summary}>
                    {a.summary}
                  </td>
                  <td class="min-w-0">
                    <span class={`badge badge-soft ${outcomeBadge(a.outcome)}`} title={a.outcome}>
                      {a.outcome}
                    </span>
                    {a.error_code && (
                      <span class="block truncate font-mono opacity-60" title={a.error_code}>
                        {a.error_code}
                      </span>
                    )}
                  </td>
                  <td class="whitespace-nowrap">{`${(a.duration_ms / 1000).toFixed(1)}s`}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {moreError && (
          <p class="m-0 min-w-0 break-words opacity-60" role="alert">
            Couldn't load more actions: {moreError}
          </p>
        )}
        {next && (
          <button
            type="button"
            class="btn mt-3"
            disabled={loadingMore}
            onclick={onMore}
          >
            {loadingMore && <span class="loading loading-spinner loading-sm" aria-hidden="true" />}
            {loadingMore ? "Loading…" : "Load more"}
          </button>
        )}
      </div>
    </section>
  );
}

// --- Deploys -------------------------------------------------------------------
// Ported from Noite's deploys.tsx: status badge, sha + copy, expandable row.

function DeployRow({ deploy }: { deploy: ConsoleDeploy }) {
  return (
    <li class="list-row min-w-0 text-[13px]">
      <span class={`badge badge-soft ${statusBadge(deploy.status)} shrink-0`} title={deploy.status}>{deploy.status}</span>
      <span class="shrink-0 truncate font-mono" title={deploy.sha}>
        {shortSha(deploy.sha)}
      </span>
      <CopyButton value={deploy.sha} label="Copy full SHA" />
      <span class="shrink-0 whitespace-nowrap text-xs opacity-60" title={when(deploy.created_at)}>
        {timeAgo(deploy.created_at)}
      </span>
      <span class="shrink-0 whitespace-nowrap text-xs opacity-60">
        {deploy.duration_ms == null
          ? ""
          : `${(deploy.duration_ms / 1000).toFixed(1)}s`}
      </span>
    </li>
  );
}

export function DeploysView({
  app,
  expanded,
  onToggle,
}: {
  app: AppDetail;
  expanded: string[];
  onToggle: (id: string) => void;
}) {
  if (app.deploys.length === 0) {
    return (
      <EmptyState icon={<DeploysIcon />} title="No deploys yet">
        Deploys your agent ships will show up here.
      </EmptyState>
    );
  }
  return (
    <div class="flex min-w-0 flex-col gap-3">
      {app.deploys.map((d) => {
        const open = expanded.includes(d.deploy_id);
        return (
          <section class={`collapse collapse-arrow border border-base-300 bg-base-100 min-w-0 ${open ? "collapse-open" : "collapse-close"}`} key={d.deploy_id} aria-label={`Deploy ${shortSha(d.sha)}`}>
            <button
              type="button"
              class="collapse-title flex min-w-0 items-center gap-2.5 text-left text-sm"
              aria-expanded={open}
              onclick={() => onToggle(d.deploy_id)}
            >
              <span class={`badge badge-soft ${statusBadge(d.status)} shrink-0`} title={d.status}>{d.status}</span>
              <span class="min-w-0 flex-1 truncate font-mono" title={d.sha}>
                {shortSha(d.sha)}
              </span>
              <span class="shrink-0 whitespace-nowrap text-xs opacity-60" title={when(d.created_at)}>
                {timeAgo(d.created_at)}
              </span>
              <span class="shrink-0 whitespace-nowrap text-xs opacity-60">
                {d.duration_ms == null
                  ? ""
                  : `${(d.duration_ms / 1000).toFixed(1)}s`}
              </span>
            </button>
            {open && (
              <div class="collapse-content min-w-0">
                <div class="flex min-w-0 flex-col gap-2 text-[13px]">
                  <div class="flex min-w-0 gap-3">
                    <span class="w-[90px] shrink-0 opacity-60">Commit</span>
                    <span class="flex min-w-0 items-center gap-2">
                      <span class="min-w-0 flex-1 truncate font-mono" title={d.sha}>{d.sha}</span>
                      <CopyButton value={d.sha} label="Copy full SHA" />
                    </span>
                  </div>
                  <div class="flex min-w-0 gap-3">
                    <span class="w-[90px] shrink-0 opacity-60">Created</span>
                    <span class="min-w-0 flex-1 truncate" title={when(d.created_at)}>{when(d.created_at)}</span>
                  </div>
                  <div class="flex min-w-0 gap-3">
                    <span class="w-[90px] shrink-0 opacity-60">Findings</span>
                    <span class="min-w-0 flex-1 truncate">{d.findings_count}</span>
                  </div>
                  <div class="flex min-w-0 gap-3">
                    <span class="w-[90px] shrink-0 opacity-60">URL</span>
                    <span class="flex min-w-0 items-center gap-2">
                      <a class="link min-w-0 flex-1 truncate" href={app.url} title={app.url}>
                        {app.url}
                      </a>
                      <CopyButton value={app.url} label="Copy app URL" />
                    </span>
                  </div>
                  {d.message && (
                    <p class="alert alert-error alert-soft m-0 min-w-0" role="alert">
                      <span class="min-w-0 break-all" title={d.message}>{d.message}</span>
                    </p>
                  )}
                </div>
              </div>
            )}
          </section>
        );
      })}
    </div>
  );
}

// --- Logs ----------------------------------------------------------------------

export function LogsView({
  panel,
  onRefresh,
  onRetry,
}: {
  panel: Panel<LogLine[]>;
  onRefresh: () => void;
  onRetry: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading logs" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} onRetry={onRetry} />;
  }
  const lines = panel.value;
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="Logs">
      <div class="card-body min-w-0">
        <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
          <h2 class="card-title min-w-0 truncate" title={`Recent logs (${lines.length.toLocaleString()} lines)`}>
            Recent logs{" "}
            <span class="opacity-60">
              ({lines.length.toLocaleString()} lines)
            </span>
          </h2>
          <button type="button" class="btn btn-sm shrink-0" onclick={onRefresh}>
            Refresh
          </button>
        </div>
        {lines.length === 0 ? (
          <EmptyState plain icon={<LogsIcon />} title="No log lines yet">
            Lines your app writes will show up here.
          </EmptyState>
        ) : (
          <ol class="log-list">
            {lines.map((entry) => (
              <li
                key={entry.seq}
                class={`log-line is-${entry.stream}`}
                title={when(entry.ts)}
              >
                <span class="log-stream" aria-hidden="true">
                  {entry.stream === "stderr" ? "!" : ">"}
                </span>
                <code>{entry.line}</code>
              </li>
            ))}
          </ol>
        )}
      </div>
    </section>
  );
}

// --- Errors --------------------------------------------------------------------
// Ported from Noite's errors.tsx: grouped issues with counts + first/last
// seen, plus the trailing-hour proxy 5xx count.

export function ErrorsView({ panel, onRetry }: { panel: Panel<ErrorsSummary>; onRetry: () => void }) {
  if (panel.kind === "loading") {
    return <Loading label="Loading errors" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} onRetry={onRetry} />;
  }
  const { issues, s5xx_last_hour } = panel.value;
  if (issues.length === 0) {
    return (
      <EmptyState icon={<ErrorsIcon />} title="No errors recorded">
        {`Grouped runtime errors will show up here.${s5xx_last_hour > 0 ? ` ${s5xx_last_hour} proxy 5xx in the last hour, with no grouped issue.` : ""}`}
      </EmptyState>
    );
  }
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="Errors">
      <div class="card-body min-w-0">
        <div class="mb-3 flex min-w-0 items-center justify-between gap-3">
          <h2 class="card-title min-w-0 truncate" title="Errors">Errors</h2>
          <span class="min-w-0 shrink-0 truncate text-xs opacity-60" title={`${s5xx_last_hour.toLocaleString()} proxy 5xx in the last hour`}>
            {s5xx_last_hour.toLocaleString()} proxy 5xx in the last hour
          </span>
        </div>
        <div class="min-w-0 overflow-x-auto">
          <table class="table table-fixed w-full">
            <colgroup>
              <col class="w-24" />
              <col />
              <col class="w-20" />
              <col class="w-24" />
              <col class="w-24" />
            </colgroup>
            <thead>
              <tr>
                <th scope="col">Last seen</th>
                <th scope="col">Error</th>
                <th scope="col">Seen</th>
                <th scope="col">First seen</th>
                <th scope="col">Status</th>
              </tr>
            </thead>
            <tbody>
              {issues.map((issue) => (
                <tr key={issue.fingerprint}>
                  <td class="whitespace-nowrap" title={when(issue.last_seen)}>
                    {timeAgo(issue.last_seen)}
                  </td>
                  <td class="min-w-0 truncate font-mono" title={issue.frame ? `${issue.message}\n${issue.frame}` : issue.message}>
                    {issue.message}
                    {issue.frame && (
                      <span class="block truncate opacity-60" title={issue.frame}>
                        {issue.frame}
                      </span>
                    )}
                  </td>
                  <td class="whitespace-nowrap">{issue.count.toLocaleString()}×</td>
                  <td class="whitespace-nowrap" title={when(issue.first_seen)}>
                    {timeAgo(issue.first_seen)}
                  </td>
                  <td class="min-w-0">
                    <span class={`badge badge-soft ${outcomeBadge(issue.status)}`} title={issue.status}>
                      {issue.status}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </section>
  );
}

// --- Secrets -------------------------------------------------------------------

export function SecretsView({ panel, onRetry }: { panel: Panel<SecretsSummary>; onRetry: () => void }) {
  if (panel.kind === "loading") {
    return <Loading label="Loading secrets" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} onRetry={onRetry} />;
  }
  const secrets = panel.value.secrets;
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <section class="card card-border bg-base-100 min-w-0" aria-label="About secrets">
        <div class="card-body min-w-0">
          <p class="m-0 min-w-0 break-words opacity-60">
            Names only — values never leave the server. Your agent manages them
            over MCP (`list_secrets`, `set_secret`, `unset_secret`); they reach
            the app as Worker env vars on the next deploy.
          </p>
        </div>
      </section>
      {secrets.length === 0 ? (
        <EmptyState icon={<SecretsIcon />} title="No secrets set">
          Your agent manages secret names over MCP; values never leave the server.
        </EmptyState>
      ) : (
        <section class="card card-border bg-base-100 min-w-0" aria-label="Secrets">
          <div class="card-body min-w-0">
            <div class="min-w-0 overflow-x-auto">
              <table class="table table-fixed w-full">
                <colgroup>
                  <col />
                  <col class="w-28" />
                </colgroup>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Updated</th>
                  </tr>
                </thead>
                <tbody>
                  {secrets.map((s) => (
                    <tr key={s.name}>
                      <td class="min-w-0">
                        <span class="flex min-w-0 items-center gap-2">
                          <span class="min-w-0 flex-1 truncate font-mono" title={s.name}>{s.name}</span>
                          <CopyButton value={s.name} label={`Copy ${s.name}`} />
                        </span>
                      </td>
                      <td class="whitespace-nowrap" title={when(s.updated_at)}>
                        {timeAgo(s.updated_at)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        </section>
      )}
    </div>
  );
}

// --- Settings --------------------------------------------------------------------

export function SettingsView({ app }: { app: AppDetail }) {
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <section class="card card-border bg-base-100 min-w-0" aria-label="Settings">
        <div class="card-body min-w-0">
          <p class="m-0 min-w-0 opacity-60">
            Read-only for now — your agent manages these over MCP.
          </p>
          <dl class="m-0 mt-3 flex min-w-0 flex-col gap-2">
            <div class="flex min-w-0 gap-3 text-[13px]">
              <dt class="w-[90px] shrink-0 opacity-60">App</dt>
              <dd class="m-0 flex min-w-0 items-center gap-2">
                <span class="min-w-0 flex-1 truncate font-mono" title={app.slug}>{app.slug}</span>
                <CopyButton value={app.slug} label="Copy app slug" />
              </dd>
            </div>
            <div class="flex min-w-0 gap-3 text-[13px]">
              <dt class="w-[90px] shrink-0 opacity-60">URL</dt>
              <dd class="m-0 flex min-w-0 items-center gap-2">
                <a class="link min-w-0 flex-1 truncate" href={app.url} title={app.url}>
                  {app.url}
                </a>
                <CopyButton value={app.url} label="Copy app URL" />
              </dd>
            </div>
            <div class="flex min-w-0 gap-3 text-[13px]">
              <dt class="w-[90px] shrink-0 opacity-60">Visibility</dt>
              <dd class="m-0 min-w-0 flex-1 truncate" title={app.visibility}>{app.visibility}</dd>
            </div>
            <div class="flex min-w-0 gap-3 text-[13px]">
              <dt class="w-[90px] shrink-0 opacity-60">Status</dt>
              <dd class="m-0 min-w-0 flex-1">
                <span class={`badge badge-soft ${statusBadge(app.status)}`} title={app.status}>
                  {app.status}
                </span>
              </dd>
            </div>
            <div class="flex min-w-0 gap-3 text-[13px]">
              <dt class="w-[90px] shrink-0 opacity-60">Your role</dt>
              <dd class="m-0 min-w-0 flex-1 truncate" title={app.role}>{app.role}</dd>
            </div>
            <div class="flex min-w-0 gap-3 text-[13px]">
              <dt class="w-[90px] shrink-0 opacity-60">Last request</dt>
              <dd class="m-0 min-w-0 flex-1 truncate" title={app.last_request_at ? when(app.last_request_at) : ""}>
                {app.last_request_at ? timeAgo(app.last_request_at) : "never"}
              </dd>
            </div>
          </dl>
        </div>
      </section>
    </div>
  );
}

// --- Invites (account) ---------------------------------------------------------

function inviteStatus(entry: InviteEntry): { label: string; badge: string } {
  if (entry.revokedAt != null) {
    return { label: "Revoked", badge: "badge-ghost" };
  }
  if (entry.redeemedAt != null) {
    return { label: "Joined", badge: "badge-success" };
  }
  return { label: "Available", badge: "badge-info" };
}

export function InvitesView({
  panel,
  isAdmin,
  onRetry,
}: {
  panel: Panel<InvitesSummary>;
  isAdmin: boolean;
  onRetry: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading invite codes" />;
  }
  if (panel.kind === "error") {
    return (
      <div class="flex min-w-0 flex-col gap-3">
        <Err message={panel.message} />
        <div class="min-w-0">
          <button type="button" class="btn btn-sm" onclick={onRetry}>
            Retry
          </button>
        </div>
      </div>
    );
  }
  const codes = panel.value.codes;
  const available = codes.filter(
    (c) => c.revokedAt == null && c.redeemedAt == null,
  ).length;
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <section class="card card-border bg-base-100 min-w-0" aria-label="About invite codes">
        <div class="card-body min-w-0">
          <p class="m-0 min-w-0 break-words opacity-60">
            Outer is in closed beta: one code admits one new account. Share a
            code with a friend and it is spent when they sign in
            {available === 1 ? " — you have 1 unused code left." : ` — you have ${available} unused codes left.`}
          </p>
        </div>
      </section>
      {codes.length === 0 ? (
        <EmptyState icon={<TicketIcon />} title="No invite codes">
          {isAdmin
            ? "Admins get no automatic codes — generate some on the Admin page."
            : "Codes appear here when you earn or receive them."}
        </EmptyState>
      ) : (
        <section class="card card-border bg-base-100 min-w-0" aria-label="Your invite codes">
          <div class="card-body min-w-0">
            <div class="min-w-0 overflow-x-auto">
              <table class="table table-fixed w-full max-sm:[&_td]:px-2 max-sm:[&_th]:px-2">
                <colgroup>
                  <col class="sm:w-48" />
                  <col class="w-20 max-sm:hidden" />
                  <col class="w-28" />
                  <col class="max-sm:hidden" />
                  <col class="w-28 max-sm:hidden" />
                </colgroup>
                <thead>
                  <tr>
                    <th scope="col">Code</th>
                    <th scope="col" class="max-sm:hidden">Kind</th>
                    <th scope="col">Status</th>
                    <th scope="col" class="max-sm:hidden">Joined by</th>
                    <th scope="col" class="max-sm:hidden">Created</th>
                  </tr>
                </thead>
                <tbody>
                  {codes.map((entry) => {
                    const status = inviteStatus(entry);
                    return (
                      <tr key={entry.code}>
                        <td class="min-w-0">
                          <span class="flex min-w-0 items-center gap-2">
                            <span class="min-w-0 flex-1 truncate font-mono max-sm:text-xs" title={entry.code}>
                              {entry.code}
                            </span>
                            <CopyButton value={entry.code} label={`Copy ${entry.code}`} />
                          </span>
                        </td>
                        <td class="truncate max-sm:hidden" title={entry.kind}>
                          {entry.kind}
                        </td>
                        <td class="min-w-0">
                          <span class={`badge badge-soft ${status.badge}`} title={status.label}>
                            {status.label}
                          </span>
                        </td>
                        <td
                          class="min-w-0 truncate max-sm:hidden"
                          title={entry.redeemedByEmail ?? entry.note ?? ""}
                        >
                          {entry.redeemedByEmail ?? entry.note ?? "—"}
                        </td>
                        <td class="whitespace-nowrap max-sm:hidden" title={when(entry.createdAt)}>
                          {timeAgo(entry.createdAt)}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </div>
        </section>
      )}
    </div>
  );
}

// --- Admin (account) -----------------------------------------------------------

export function AdminView({
  codes,
  waitlist,
  isAdmin,
  onGenerate,
  onRevoke,
  onMoreCodes,
  onRetry,
}: {
  codes: Panel<AdminInvitesPage>;
  waitlist: Panel<WaitlistSummary>;
  isAdmin: boolean;
  onGenerate: (count: number, note: string) => Promise<void>;
  onRevoke: (code: string) => Promise<void>;
  onMoreCodes: () => void;
  onRetry: () => void;
}) {
  if (!isAdmin) {
    return (
      <div class="flex min-w-0 flex-col gap-3">
        <div class="alert alert-warning alert-soft min-w-0" role="alert">
          <span class="min-w-0">Admins only — this page lists every invite code and the waitlist.</span>
        </div>
      </div>
    );
  }
  return (
    <div class="flex min-w-0 flex-col gap-3">
      <GenerateCard onGenerate={onGenerate} />
      <AdminCodesCard
        panel={codes}
        onRevoke={onRevoke}
        onMore={onMoreCodes}
        onRetry={onRetry}
      />
      <WaitlistCard panel={waitlist} onRetry={onRetry} />
    </div>
  );
}

function GenerateCard({
  onGenerate,
}: {
  onGenerate: (count: number, note: string) => Promise<void>;
}) {
  const busy = atom(false);
  const error = atom<string | null>(null);
  async function submit(): Promise<void> {
    const countInput = document.getElementById("invite-count") as HTMLInputElement | null;
    const noteInput = document.getElementById("invite-note") as HTMLInputElement | null;
    const count = Number(countInput?.value ?? "");
    if (!Number.isInteger(count) || count < 1 || count > 50) {
      error.set("Count must be between 1 and 50.");
      return;
    }
    busy.set(true);
    error.set(null);
    try {
      await onGenerate(count, (noteInput?.value ?? "").trim());
    } catch {
      error.set("Couldn't generate codes. Try again.");
    } finally {
      busy.set(false);
    }
  }
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="Generate invite codes">
      <div class="card-body min-w-0">
        <h2 class="card-title m-0 min-w-0 text-base">Generate codes</h2>
        <div class="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-end">
          <label class="form-control min-w-0 flex-1" for="invite-count">
            <span class="label min-w-0">
              <span class="label-text truncate" title="How many codes to mint (1–50)">
                Count (1–50)
              </span>
            </span>
            <input
              type="number"
              id="invite-count"
              class="input input-bordered w-full min-w-0"
              min="1"
              max="50"
              value="5"
            />
          </label>
          <label class="form-control min-w-0 flex-[2]" for="invite-note">
            <span class="label min-w-0">
              <span class="label-text truncate" title="An optional note stored with each code">
                Note (optional)
              </span>
            </span>
            <input
              type="text"
              id="invite-note"
              class="input input-bordered w-full min-w-0"
              maxlength="200"
              placeholder="e.g. friends of Ana"
            />
          </label>
          <button
            type="button"
            class="btn btn-neutral min-w-0 shrink-0"
            disabled={busy()}
            onclick={() => void submit()}
          >
            {busy() ? "Generating…" : "Generate"}
          </button>
        </div>
        {error() && (
          <p class="m-0 min-w-0 break-words text-sm text-error" role="alert">
            {error()}
          </p>
        )}
      </div>
    </section>
  );
}

function AdminCodesCard({
  panel,
  onRevoke,
  onMore,
  onRetry,
}: {
  panel: Panel<AdminInvitesPage>;
  onRevoke: (code: string) => Promise<void>;
  onMore: () => void;
  onRetry: () => void;
}) {
  const revoking = atom<string | null>(null);
  const error = atom<string | null>(null);
  if (panel.kind === "loading") {
    return <Loading label="Loading invite codes" />;
  }
  if (panel.kind === "error") {
    return (
      <div class="flex min-w-0 flex-col gap-3">
        <Err message={panel.message} />
        <div class="min-w-0">
          <button type="button" class="btn btn-sm" onclick={onRetry}>
            Retry
          </button>
        </div>
      </div>
    );
  }
  const page = panel.value;
  async function revoke(code: string): Promise<void> {
    revoking.set(code);
    error.set(null);
    try {
      await onRevoke(code);
    } catch {
      error.set("Couldn't revoke that code. Try again.");
    } finally {
      revoking.set(null);
    }
  }
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="All invite codes">
      <div class="card-body min-w-0">
        <h2 class="card-title m-0 min-w-0 text-base">All codes</h2>
        {error() && (
          <p class="m-0 min-w-0 break-words text-sm text-error" role="alert">
            {error()}
          </p>
        )}
        {page.codes.length === 0 ? (
          <EmptyState plain icon={<TicketIcon />} title="No codes yet">
            Generate some above to invite new accounts.
          </EmptyState>
        ) : (
          <div class="min-w-0 overflow-x-auto">
            <table class="table table-fixed w-full max-sm:[&_td]:px-2 max-sm:[&_th]:px-2">
              <colgroup>
                <col class="sm:w-48" />
                <col class="w-20 max-md:hidden" />
                <col class="max-md:hidden" />
                <col class="max-sm:hidden" />
                <col class="w-28 max-md:hidden" />
                <col class="w-20 sm:w-28" />
              </colgroup>
              <thead>
                <tr>
                  <th scope="col">Code</th>
                  <th scope="col" class="max-md:hidden">Kind</th>
                  <th scope="col" class="max-md:hidden">Creator</th>
                  <th scope="col" class="max-sm:hidden">Note / joined by</th>
                  <th scope="col" class="max-md:hidden">Created</th>
                  <th scope="col">
                    <span class="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {page.codes.map((entry) => {
                  const status = inviteStatus(entry);
                  const usable = entry.revokedAt == null && entry.redeemedAt == null;
                  return (
                    <tr key={entry.code}>
                      <td class="min-w-0">
                        <span class="flex min-w-0 items-center gap-2">
                          <span class="min-w-0 flex-1 truncate font-mono max-sm:text-xs" title={entry.code}>
                            {entry.code}
                          </span>
                          <CopyButton value={entry.code} label={`Copy ${entry.code}`} />
                        </span>
                      </td>
                      <td class="truncate max-md:hidden" title={entry.kind}>
                        {entry.kind}
                      </td>
                      <td class="min-w-0 truncate max-md:hidden" title={entry.createdByEmail ?? ""}>
                        {entry.createdByEmail ?? "—"}
                      </td>
                      <td
                        class="min-w-0 truncate max-sm:hidden"
                        title={entry.redeemedByEmail ?? entry.note ?? status.label}
                      >
                        {entry.redeemedByEmail ?? entry.note ?? status.label}
                      </td>
                      <td class="whitespace-nowrap max-md:hidden" title={when(entry.createdAt)}>
                        {timeAgo(entry.createdAt)}
                      </td>
                      <td class="min-w-0">
                        {usable ? (
                          <button
                            type="button"
                            class="btn btn-ghost btn-xs"
                            disabled={revoking() === entry.code}
                            title={`Revoke ${entry.code}`}
                            onclick={() => void revoke(entry.code)}
                          >
                            {revoking() === entry.code ? "Revoking…" : "Revoke"}
                          </button>
                        ) : (
                          <span class={`badge badge-soft ${status.badge}`} title={status.label}>
                            {status.label}
                          </span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {page.next && (
          <div class="mt-2 min-w-0">
            <button type="button" class="btn btn-sm" onclick={onMore}>
              Load more
            </button>
          </div>
        )}
      </div>
    </section>
  );
}

function WaitlistCard({
  panel,
  onRetry,
}: {
  panel: Panel<WaitlistSummary>;
  onRetry: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading waitlist" />;
  }
  if (panel.kind === "error") {
    return (
      <div class="flex min-w-0 flex-col gap-3">
        <Err message={panel.message} />
        <div class="min-w-0">
          <button type="button" class="btn btn-sm" onclick={onRetry}>
            Retry
          </button>
        </div>
      </div>
    );
  }
  const entries = panel.value.entries;
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="Waitlist">
      <div class="card-body min-w-0">
        <h2 class="card-title m-0 min-w-0 text-base">Waitlist</h2>
        {entries.length === 0 ? (
          <EmptyState plain title="Nobody waiting">
            Signups that hit the closed beta land here.
          </EmptyState>
        ) : (
          <div class="min-w-0 overflow-x-auto">
            <table class="table table-fixed w-full">
              <colgroup>
                <col />
                <col class="w-24" />
                <col class="w-24" />
              </colgroup>
              <thead>
                <tr>
                  <th scope="col">Email</th>
                  <th scope="col">Tries</th>
                  <th scope="col">First seen</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((entry) => (
                  <tr key={entry.email}>
                    <td class="min-w-0 truncate font-mono" title={entry.email}>
                      {entry.email}
                    </td>
                    <td class="whitespace-nowrap">{entry.attempts}</td>
                    <td class="whitespace-nowrap" title={when(entry.first_seen)}>
                      {timeAgo(entry.first_seen)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </section>
  );
}
