// The `/console/app?app=<slug>` page: header with status, URL and
// visibility, the last 20 deploys, and recent logs with a refresh button.
// The slug rides the query string (one static shell for every app). Action
// log and metrics panels do not exist yet: their endpoints land in parallel
// branches, and nothing renders until they do.
import { atom } from "ilha";
import { watch } from "ilha";

import { api, shortSha, when, type AppDetail, type LogLine } from "./api.ts";

type State =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; app: AppDetail; logs: LogLine[]; logsError: string | null };

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

async function loadLogs(slug: string): Promise<LogLine[]> {
  const data = await api<{ lines: LogLine[] }>(
    `/api/console/apps/${encodeURIComponent(slug)}/logs?limit=100`,
  );
  return data.lines;
}

export function AppView() {
  const state = atom<State>({ kind: "loading" });
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

  watch.once(() => {
    if (typeof window === "undefined" || !slug) {
      return;
    }
    api<AppDetail>(`/api/console/apps/${encodeURIComponent(slug)}`)
      .then(async (app) => {
        const logs = await loadLogs(app.slug).catch(() => null);
        state.set({
          kind: "ready",
          app,
          logs: logs ?? [],
          logsError: logs === null ? "Couldn't load logs." : null,
        });
      })
      .catch((err: unknown) => {
        state.set({ kind: "error", message: String(err) });
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
