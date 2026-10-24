// Console views: one panel per sidebar item.
// Data shapes ported from Noite's $lib/app-detail/{metrics,deploys,errors,
// events,logs}.tsx; interaction details follow Noite (loading skeletons,
// empty states, relative times with absolute titles on hover, copy buttons
// for shas/URLs, keyboard-focusable rows). Charts are monochrome inline SVG:
// near-black series, light-gray comparison series, dashed vertical
// gridlines, minimal axes, muted tick labels. No `style=` attributes — the
// console CSP only hashes inline scripts/styles, so all sizing rides on
// SVG attributes.
import {
  shortSha,
  when,
  type AppDetail,
  type ConsoleDeploy,
  type ErrorsSummary,
  type LogLine,
  type MetricsBucket,
  type MetricsSummary,
  type SecretsSummary,
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
  ChevronDownIcon,
  ErrorsIcon,
  MetricsIcon,
  OverviewIcon,
} from "./icons.tsx";

// --- Small pieces ------------------------------------------------------------

function Loading({ label }: { label: string }) {
  return (
    <div class="stack" role="status" aria-label={label}>
      <div class="skeleton-card" />
      <div class="skeleton-chart" />
    </div>
  );
}

function Err({ message }: { message: string }) {
  return (
    <div class="card" role="alert">
      <p>Couldn't load this view: {message}</p>
    </div>
  );
}

function Empty({ children }: { children?: unknown }) {
  return (
    <div class="card" role="status">
      <p>{children}</p>
    </div>
  );
}

function compact(n: number): string {
  if (n >= 1000) {
    const v = (n / 1000).toFixed(1).replace(/\.0$/, "");
    return `${v}k`;
  }
  return `${n}`;
}

function statusClass(status: string): string {
  if (status === "success" || status === "running" || status === "ok") {
    return "is-ok";
  }
  if (
    status === "building" ||
    status === "deploying" ||
    status === "reviewing" ||
    status === "queued"
  ) {
    return "is-info";
  }
  if (status === "failed" || status === "error" || status === "blocked") {
    return "is-bad";
  }
  if (status === "sleeping") {
    return "is-muted";
  }
  return "is-neutral";
}

function outcomeClass(outcome: string): string {
  if (outcome === "success" || outcome === "ok") {
    return "is-ok";
  }
  if (outcome === "failed" || outcome === "error") {
    return "is-bad";
  }
  if (outcome === "blocked") {
    return "is-warn";
  }
  return "is-neutral";
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

// --- Stat cards --------------------------------------------------------------

function StatCard({
  icon,
  label,
  value,
  pill,
  sub,
}: {
  icon?: unknown;
  label: string;
  value: string;
  pill?: string | null;
  sub?: string;
}) {
  return (
    <section class="card stat" aria-label={label}>
      <p class="stat-label">
        {icon}
        <span>{label}</span>
      </p>
      <p class="stat-value">
        {value}
        {pill != null && pill !== "" && (
          <span class="delta-pill">{pill}</span>
        )}
      </p>
      {sub && <p class="stat-sub">{sub}</p>}
    </section>
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
      <figcaption class="axis-ends" aria-hidden="true">
        <span>{first ? axisLabel(first.hour, grouped ? 168 : hours) : ""}</span>
        <span>{last ? axisLabel(last.hour, grouped ? 168 : hours) : ""}</span>
      </figcaption>
    </figure>
  );
}

// --- Overview ------------------------------------------------------------------

export function OverviewView({
  app,
  metrics,
  actions,
  errors,
  onRange,
  onView,
}: {
  app: AppDetail;
  metrics: Panel<{ hours: number; summary: MetricsSummary }>;
  actions: ActionsPanel;
  errors: Panel<ErrorsSummary>;
  onRange: (hours: number) => void;
  onView: (view: ViewId) => void;
}) {
  return (
    <div class="stack">
      {metrics.kind === "loading" && <Loading label="Loading overview" />}
      {metrics.kind === "error" && <Err message={metrics.message} />}
      {metrics.kind === "ready" && (
        <OverviewReady
          summary={metrics.value.summary}
          hours={metrics.value.hours}
          onRange={onRange}
        />
      )}
      <div class="grid-2">
        <section class="card" aria-label="Latest deploy">
          <div class="card-head">
            <h2>Latest deploy</h2>
            <button
              type="button"
              class="link-btn"
              onclick={() => onView("deploys")}
            >
              View all
            </button>
          </div>
          {app.deploys.length === 0 ? (
            <p class="muted" role="status">
              No deploys yet.
            </p>
          ) : (
            <DeployRow deploy={app.deploys[0]} />
          )}
        </section>
        <section class="card" aria-label="Latest agent actions">
          <div class="card-head">
            <h2>Latest agent actions</h2>
            <button
              type="button"
              class="link-btn"
              onclick={() => onView("actions")}
            >
              View all
            </button>
          </div>
          {actions.kind === "loading" && (
            <p class="muted" role="status">
              Loading actions…
            </p>
          )}
          {actions.kind === "error" && (
            <p class="muted" role="alert">
              Couldn't load actions: {actions.message}
            </p>
          )}
          {actions.kind === "ready" &&
            (actions.state.actions.length === 0 ? (
              <p class="muted" role="status">
                No agent actions yet.
              </p>
            ) : (
              <ul class="mini-list">
                {actions.state.actions.slice(0, 5).map((a) => (
                  <li key={a.id} class="mini-row">
                    <span class={`pill ${outcomeClass(a.outcome)}`}>
                      {a.outcome}
                    </span>
                    <span class="mono trim" title={a.summary}>
                      {a.summary || a.tool}
                    </span>
                    <span class="muted" title={when(a.at)}>
                      {timeAgo(a.at)}
                    </span>
                  </li>
                ))}
              </ul>
            ))}
        </section>
      </div>
      {errors.kind === "ready" && errors.value.issues.length > 0 && (
        <section class="card" aria-label="Open errors">
          <div class="card-head">
            <h2>Open errors</h2>
            <button
              type="button"
              class="link-btn"
              onclick={() => onView("errors")}
            >
              View all
            </button>
          </div>
          <ul class="mini-list">
            {errors.value.issues
              .filter((i) => i.status === "open")
              .slice(0, 3)
              .map((i) => (
                <li key={i.fingerprint} class="mini-row">
                  <span class="mono trim" title={i.message}>
                    {i.message}
                  </span>
                  <span class="muted">
                    {i.count.toLocaleString()}× ·{" "}
                    <span title={when(i.last_seen)}>{timeAgo(i.last_seen)}</span>
                  </span>
                </li>
              ))}
          </ul>
        </section>
      )}
    </div>
  );
}

function OverviewReady({
  summary,
  hours,
  onRange,
}: {
  summary: MetricsSummary;
  hours: number;
  onRange: (hours: number) => void;
}) {
  const errorRate = fmtRate(summary.s5xx, summary.requests);
  return (
    <div class="stack">
      <div class="stat-grid">
        <StatCard
          icon={<MetricsIcon size={16} />}
          label="Requests"
          value={summary.requests.toLocaleString()}
          pill={halfChange(summary.buckets)}
          sub="in range"
        />
        <StatCard
          icon={<OverviewIcon size={16} />}
          label="Visitors"
          value={summary.visitors.toLocaleString()}
          sub="daily uniques, summed"
        />
        <StatCard
          icon={<ErrorsIcon size={16} />}
          label="Error rate"
          value={errorRate}
          pill={summary.s5xx > 0 ? `${summary.s5xx.toLocaleString()} 5xx` : null}
          sub="server errors share"
        />
        <StatCard
          icon={<ActionsIcon size={16} />}
          label="p95 latency"
          value={`${summary.p95_ms} ms`}
          pill={`p50 ${summary.p50_ms} ms`}
          sub="response headers"
        />
      </div>
      <section class="card" aria-label="Requests chart">
        <div class="card-head">
          <h2>Requests</h2>
          <div class="legend">
            <span class="legend-item">
              <span class="swatch swatch-dark" aria-hidden="true" />
              This period
            </span>
            <span class="legend-item">
              <span class="swatch swatch-light" aria-hidden="true" />
              Errors (4xx + 5xx)
            </span>
          </div>
        </div>
        {summary.buckets.length === 0 ? (
          <p class="muted" role="status">
            No traffic in this range.
          </p>
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
  onRange,
}: {
  metrics: Panel<{ hours: number; summary: MetricsSummary }>;
  onRange: (hours: number) => void;
}) {
  if (metrics.kind === "loading") {
    return <Loading label="Loading metrics" />;
  }
  if (metrics.kind === "error") {
    return <Err message={metrics.message} />;
  }
  const { summary, hours } = metrics.value;
  const errorRate = fmtRate(summary.s5xx, summary.requests);
  return (
    <div class="stack">
      <div class="stat-grid">
        <StatCard
          icon={<MetricsIcon size={16} />}
          label="Requests"
          value={summary.requests.toLocaleString()}
          pill={halfChange(summary.buckets)}
          sub="in range"
        />
        <StatCard
          icon={<OverviewIcon size={16} />}
          label="Visitors"
          value={summary.visitors.toLocaleString()}
          sub="daily uniques, summed"
        />
        <StatCard
          icon={<ErrorsIcon size={16} />}
          label="Error rate"
          value={errorRate}
          pill={summary.s5xx > 0 ? `${summary.s5xx.toLocaleString()} 5xx` : null}
          sub={`${summary.s4xx.toLocaleString()} client errors (4xx)`}
        />
        <StatCard
          icon={<ActionsIcon size={16} />}
          label="p95 latency"
          value={`${summary.p95_ms} ms`}
          pill={`p50 ${summary.p50_ms} ms`}
          sub="response headers"
        />
      </div>
      <section class="card" aria-label="Hourly requests">
        <div class="card-head">
          <h2>Hourly requests</h2>
        </div>
        {summary.buckets.length === 0 ? (
          <p class="muted" role="status">
            No traffic in this range.
          </p>
        ) : (
          <Bars buckets={summary.buckets} hours={hours} />
        )}
      </section>
      <section class="card" aria-label="Top paths">
        <div class="card-head">
          <h2>Top paths</h2>
        </div>
        {summary.top_paths.length === 0 ? (
          <p class="muted" role="status">
            No paths recorded in this range.
          </p>
        ) : (
          <div class="table-wrap">
            <table>
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
                    <td class="mono trim" title={p.path}>
                      {p.path}
                    </td>
                    <td>{p.requests.toLocaleString()}</td>
                    <td>{p.s5xx.toLocaleString()}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}

// --- Actions -------------------------------------------------------------------
// Ported from Noite's events.tsx: newest-first feed, cursor pagination.

export function ActionsView({
  panel,
  onMore,
}: {
  panel: ActionsPanel;
  onMore: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading agent actions" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} />;
  }
  const { actions, next, loadingMore, moreError } = panel.state;
  if (actions.length === 0) {
    return <Empty>No agent actions yet.</Empty>;
  }
  return (
    <section class="card" aria-label="Agent actions">
      <div class="table-wrap">
        <table>
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
                <td class="nowrap" title={when(a.at)}>
                  {timeAgo(a.at)}
                </td>
                <td>{a.by}</td>
                <td class="mono">{a.tool}</td>
                <td class="mono wrap" title={a.summary}>
                  {a.summary}
                </td>
                <td>
                  <span class={`pill ${outcomeClass(a.outcome)}`}>
                    {a.outcome}
                  </span>
                  {a.error_code && (
                    <span class="mono muted"> {a.error_code}</span>
                  )}
                </td>
                <td class="nowrap">{`${(a.duration_ms / 1000).toFixed(1)}s`}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {moreError && (
        <p class="muted" role="alert">
          Couldn't load more actions: {moreError}
        </p>
      )}
      {next && (
        <button
          type="button"
          class="btn"
          disabled={loadingMore}
          onclick={onMore}
        >
          {loadingMore ? "Loading…" : "Load more"}
        </button>
      )}
    </section>
  );
}

// --- Deploys -------------------------------------------------------------------
// Ported from Noite's deploys.tsx: status pill, sha + copy, expandable row.

function DeployRow({ deploy }: { deploy: ConsoleDeploy }) {
  return (
    <div class="deploy-row">
      <span class={`pill ${statusClass(deploy.status)}`}>{deploy.status}</span>
      <span class="mono" title={deploy.sha}>
        {shortSha(deploy.sha)}
      </span>
      <CopyButton value={deploy.sha} label="Copy full SHA" />
      <span class="muted nowrap" title={when(deploy.created_at)}>
        {timeAgo(deploy.created_at)}
      </span>
      <span class="muted nowrap">
        {deploy.duration_ms == null
          ? ""
          : `${(deploy.duration_ms / 1000).toFixed(1)}s`}
      </span>
    </div>
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
    return <Empty>No deploys yet.</Empty>;
  }
  return (
    <div class="stack">
      {app.deploys.map((d) => {
        const open = expanded.includes(d.deploy_id);
        return (
          <section class="card" key={d.deploy_id} aria-label={`Deploy ${shortSha(d.sha)}`}>
            <button
              type="button"
              class="deploy-head"
              aria-expanded={open}
              onclick={() => onToggle(d.deploy_id)}
            >
              <span class={`pill ${statusClass(d.status)}`}>{d.status}</span>
              <span class="mono" title={d.sha}>
                {shortSha(d.sha)}
              </span>
              <span class="muted nowrap" title={when(d.created_at)}>
                {timeAgo(d.created_at)}
              </span>
              <span class="muted nowrap">
                {d.duration_ms == null
                  ? ""
                  : `${(d.duration_ms / 1000).toFixed(1)}s`}
              </span>
              <span class="deploy-chevron" aria-hidden="true">
                <ChevronDownIcon />
              </span>
            </button>
            {open && (
              <div class="deploy-detail">
                <div class="detail-row">
                  <span class="muted">Commit</span>
                  <span>
                    <span class="mono">{d.sha}</span>{" "}
                    <CopyButton value={d.sha} label="Copy full SHA" />
                  </span>
                </div>
                <div class="detail-row">
                  <span class="muted">Created</span>
                  <span title={when(d.created_at)}>{when(d.created_at)}</span>
                </div>
                <div class="detail-row">
                  <span class="muted">Findings</span>
                  <span>{d.findings_count}</span>
                </div>
                <div class="detail-row">
                  <span class="muted">URL</span>
                  <span>
                    <a class="link" href={app.url}>
                      {app.url}
                    </a>{" "}
                    <CopyButton value={app.url} label="Copy app URL" />
                  </span>
                </div>
                {d.message && (
                  <p class="deploy-message" role="alert">
                    {d.message}
                  </p>
                )}
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
}: {
  panel: Panel<LogLine[]>;
  onRefresh: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading logs" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} />;
  }
  const lines = panel.value;
  return (
    <section class="card" aria-label="Logs">
      <div class="card-head">
        <h2>
          Recent logs{" "}
          <span class="muted">
            ({lines.length.toLocaleString()} lines)
          </span>
        </h2>
        <button type="button" class="btn" onclick={onRefresh}>
          Refresh
        </button>
      </div>
      {lines.length === 0 ? (
        <p class="muted" role="status">
          No log lines yet.
        </p>
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
    </section>
  );
}

// --- Errors --------------------------------------------------------------------
// Ported from Noite's errors.tsx: grouped issues with counts + first/last
// seen, plus the trailing-hour proxy 5xx count.

export function ErrorsView({ panel }: { panel: Panel<ErrorsSummary> }) {
  if (panel.kind === "loading") {
    return <Loading label="Loading errors" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} />;
  }
  const { issues, s5xx_last_hour } = panel.value;
  if (issues.length === 0) {
    return (
      <Empty>
        {`No errors recorded.${s5xx_last_hour > 0 ? ` ${s5xx_last_hour} proxy 5xx in the last hour, with no grouped issue.` : ""}`}
      </Empty>
    );
  }
  return (
    <section class="card" aria-label="Errors">
      <div class="card-head">
        <h2>Errors</h2>
        <span class="muted">
          {s5xx_last_hour.toLocaleString()} proxy 5xx in the last hour
        </span>
      </div>
      <div class="table-wrap">
        <table>
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
                <td class="nowrap" title={when(issue.last_seen)}>
                  {timeAgo(issue.last_seen)}
                </td>
                <td class="mono wrap">
                  {issue.message}
                  {issue.frame && (
                    <span class="muted">
                      <br />
                      {issue.frame}
                    </span>
                  )}
                </td>
                <td class="nowrap">{issue.count.toLocaleString()}×</td>
                <td class="nowrap" title={when(issue.first_seen)}>
                  {timeAgo(issue.first_seen)}
                </td>
                <td>
                  <span class={`pill ${outcomeClass(issue.status)}`}>
                    {issue.status}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// --- Secrets -------------------------------------------------------------------

export function SecretsView({ panel }: { panel: Panel<SecretsSummary> }) {
  if (panel.kind === "loading") {
    return <Loading label="Loading secrets" />;
  }
  if (panel.kind === "error") {
    return <Err message={panel.message} />;
  }
  const secrets = panel.value.secrets;
  return (
    <div class="stack">
      <section class="card" aria-label="About secrets">
        <p class="muted">
          Names only — values never leave the server. Your agent manages them
          over MCP (`list_secrets`, `set_secret`, `unset_secret`); they reach
          the app as Worker env vars on the next deploy.
        </p>
      </section>
      {secrets.length === 0 ? (
        <Empty>No secrets set.</Empty>
      ) : (
        <section class="card" aria-label="Secrets">
          <div class="table-wrap">
            <table>
              <thead>
                <tr>
                  <th scope="col">Name</th>
                  <th scope="col">Updated</th>
                </tr>
              </thead>
              <tbody>
                {secrets.map((s) => (
                  <tr key={s.name}>
                    <td>
                      <span class="mono">{s.name}</span>{" "}
                      <CopyButton value={s.name} label={`Copy ${s.name}`} />
                    </td>
                    <td class="nowrap" title={when(s.updated_at)}>
                      {timeAgo(s.updated_at)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      )}
    </div>
  );
}

// --- Settings --------------------------------------------------------------------

export function SettingsView({ app }: { app: AppDetail }) {
  return (
    <div class="stack">
      <section class="card" aria-label="Settings">
        <p class="muted">
          Read-only for now — your agent manages these over MCP.
        </p>
        <dl class="def-list">
          <div class="detail-row">
            <dt class="muted">App</dt>
            <dd>
              <span class="mono">{app.slug}</span>{" "}
              <CopyButton value={app.slug} label="Copy app slug" />
            </dd>
          </div>
          <div class="detail-row">
            <dt class="muted">URL</dt>
            <dd>
              <a class="link" href={app.url}>
                {app.url}
              </a>{" "}
              <CopyButton value={app.url} label="Copy app URL" />
            </dd>
          </div>
          <div class="detail-row">
            <dt class="muted">Visibility</dt>
            <dd>{app.visibility}</dd>
          </div>
          <div class="detail-row">
            <dt class="muted">Status</dt>
            <dd>
              <span class={`pill ${statusClass(app.status)}`}>
                {app.status}
              </span>
            </dd>
          </div>
          <div class="detail-row">
            <dt class="muted">Your role</dt>
            <dd>{app.role}</dd>
          </div>
          <div class="detail-row">
            <dt class="muted">Last request</dt>
            <dd title={app.last_request_at ? when(app.last_request_at) : ""}>
              {app.last_request_at ? timeAgo(app.last_request_at) : "never"}
            </dd>
          </div>
        </dl>
      </section>
    </div>
  );
}
