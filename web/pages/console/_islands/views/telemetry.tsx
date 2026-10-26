// Console Admin tool-telemetry view (Wave 4, migration 0015): per-tool
// calls, p50/p95 latency, error rate and average bytes over 24h/7d, plus
// the calls-per-commit efficiency metric. Its own view file, so the
// Console-shell rework never touches it: console.tsx only adds the fetch,
// views.tsx only adds one card to the Admin page.
import { api } from "../api.ts";
import type { Panel } from "../common.tsx";
import { Err, Loading } from "../views.tsx";

export interface ToolTelemetry {
  tool: string;
  calls: number;
  p50_ms: number;
  p95_ms: number;
  error_rate: number;
  avg_bytes: number;
}

export interface TelemetryWindow {
  tools: ToolTelemetry[];
  calls_per_commit: number;
  sessions_with_commits: number;
  commits: number;
  calls: number;
}

export interface TelemetrySummary {
  day: TelemetryWindow;
  week: TelemetryWindow;
  generated_at: number;
}

export function fetchTelemetry(): Promise<TelemetrySummary> {
  return api<TelemetrySummary>("/api/console/admin/telemetry");
}

function WindowTable({ window }: { window: TelemetryWindow }) {
  if (window.tools.length === 0) {
    return <p class="m-0 opacity-60">No tool calls in this window yet.</p>;
  }
  return (
    <div class="min-w-0 overflow-x-auto">
      <table class="table table-sm min-w-0">
        <thead>
          <tr>
            <th scope="col">Tool</th>
            <th scope="col" class="text-right">Calls</th>
            <th scope="col" class="text-right">p50</th>
            <th scope="col" class="text-right">p95</th>
            <th scope="col" class="text-right">Errors</th>
            <th scope="col" class="text-right">Avg bytes</th>
          </tr>
        </thead>
        <tbody>
          {window.tools.map((t) => (
            <tr key={t.tool}>
              <td class="min-w-0 font-mono">{t.tool}</td>
              <td class="text-right">{t.calls.toLocaleString()}</td>
              <td class="text-right">{t.p50_ms}ms</td>
              <td class="text-right">{t.p95_ms}ms</td>
              <td class="text-right">{(t.error_rate * 100).toFixed(1)}%</td>
              <td class="text-right">{t.calls > 0 ? t.avg_bytes.toLocaleString() : "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function TelemetryCard({
  panel,
  onRetry,
}: {
  panel: Panel<TelemetrySummary>;
  onRetry: () => void;
}) {
  if (panel.kind === "loading") {
    return <Loading label="Loading tool telemetry" />;
  }
  if (panel.kind === "error") {
    return (
      <section class="card card-border bg-base-100 min-w-0" aria-label="Tool telemetry">
        <div class="card-body min-w-0">
          <h2 class="card-title m-0 min-w-0 text-base">Tool telemetry</h2>
          <Err message={panel.message} onRetry={onRetry} />
        </div>
      </section>
    );
  }
  const s = panel.value;
  return (
    <section class="card card-border bg-base-100 min-w-0" aria-label="Tool telemetry">
      <div class="card-body min-w-0">
        <h2 class="card-title m-0 min-w-0 text-base">Tool telemetry</h2>
        <p class="m-0 min-w-0 text-sm opacity-70">
          24h: {s.day.sessions_with_commits === 0 ? "no commits yet" : `${s.day.calls_per_commit.toFixed(1)} calls/commit over ${s.day.sessions_with_commits} session(s)`}
          {" · "}
          7d: {s.week.sessions_with_commits === 0 ? "no commits yet" : `${s.week.calls_per_commit.toFixed(1)} calls/commit over ${s.week.sessions_with_commits} session(s)`}
        </p>
        <h3 class="m-0 min-w-0 text-sm font-semibold">Last 24 hours</h3>
        <WindowTable window={s.day} />
        <h3 class="m-0 min-w-0 text-sm font-semibold">Last 7 days</h3>
        <WindowTable window={s.week} />
      </div>
    </section>
  );
}
