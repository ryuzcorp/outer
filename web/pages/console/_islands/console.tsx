// Console shell: rail (app picker) + sidebar (app views) + main panel.
// Ported from Noite's apps/[id].tsx + $lib/app-detail/{shell,panel}.tsx —
// tabs ride `?t=`, the range rides `?r=` (24/168/720 h), the URL carries
// `?app=<slug>` too (one static shell for every runtime slug), components
// remount per app id, and every panel shows skeletons, empty states,
// relative times with absolute titles, and copy buttons. Noite's SSE
// stream-hub is NOT ported: the visible view polls every ~10 s instead.
import { atom } from "ilha";
import { watch } from "ilha";

import {
  api,
  type ActionsPage,
  type AppDetail,
  type ConsoleApp,
  type ErrorsSummary,
  type LogLine,
  type MetricsSummary,
  type SecretsSummary,
} from "./api.ts";
import {
  ActionsIcon,
  CloseIcon,
  CollapseIcon,
  DatabaseIcon,
  DeploysIcon,
  DocsIcon,
  ErrorsIcon,
  LogsIcon,
  MenuIcon,
  MetricsIcon,
  OuterMark,
  OverviewIcon,
  PlusIcon,
  SecretsIcon,
  SettingsIcon,
  SignOutIcon,
} from "./icons.tsx";
import {
  ActionsView,
  DeploysView,
  ErrorsView,
  LogsView,
  MetricsView,
  OverviewView,
  SecretsView,
  SettingsView,
} from "./views.tsx";

import {
  CopyButton,
  RANGES,
  VIEWS,
  readRoute,
  routeUrl,
  type ActionsPanel,
  type Panel,
  type Route,
  type ViewId,
} from "./common.tsx";

// --- Shared bits -------------------------------------------------------------

const TINTS = 8;

function tintIndex(slug: string): number {
  let h = 0;
  for (let i = 0; i < slug.length; i++) {
    h = (h * 31 + slug.charCodeAt(i)) >>> 0;
  }
  return h % TINTS;
}

function monogram(slug: string): string {
  const clean = slug.replace(/[^a-z0-9]/gi, "");
  return (clean.slice(0, 2) || "?").toUpperCase();
}

async function loadActionsPage(
  slug: string,
  before: string | null,
): Promise<ActionsPage> {
  const cursor = before === null ? "" : `&before=${encodeURIComponent(before)}`;
  return api<ActionsPage>(
    `/api/console/apps/${encodeURIComponent(slug)}/actions?limit=20${cursor}`,
  );
}

// --- Shell -------------------------------------------------------------------

interface Ui {
  collapsed: boolean;
  drawer: boolean;
  createOpen: boolean;
  accountOpen: boolean;
  expanded: string[];
}

const VIEW_TITLES: Record<ViewId, string> = {
  overview: "Overview",
  metrics: "Metrics",
  actions: "Agent actions",
  deploys: "Deploys",
  logs: "Logs",
  errors: "Errors",
  secrets: "Secrets",
  settings: "Settings",
};

export function Console() {
  const route = atom<Route>(readRoute());
  const apps = atom<Panel<ConsoleApp[]>>({ kind: "loading" });
  const email = atom("");
  const detail = atom<Panel<AppDetail>>({ kind: "loading" });
  const metrics = atom<Panel<{ hours: number; summary: MetricsSummary }>>({
    kind: "loading",
  });
  const actions = atom<ActionsPanel>({ kind: "loading" });
  const errors = atom<Panel<ErrorsSummary>>({ kind: "loading" });
  const logs = atom<Panel<LogLine[]>>({ kind: "loading" });
  const secrets = atom<Panel<SecretsSummary>>({ kind: "loading" });
  const ui = atom<Ui>({
    collapsed: false,
    drawer: false,
    createOpen: false,
    accountOpen: false,
    expanded: [],
  });

  // One generation per navigation: late responses from the previous app
  // never repaint the new one (Noite keys its cards per app id instead).
  let epoch = 0;

  function setTitle(r: Route, app: AppDetail | null): void {
    document.title =
      app && r.app
        ? `${r.app} · ${VIEW_TITLES[r.view]} · Outer console`
        : "Console · Outer";
  }

  function loadersFor(view: ViewId): Array<"metrics" | "actions" | "errors" | "logs" | "secrets"> {
    switch (view) {
      case "overview":
        return ["metrics", "actions", "errors"];
      case "metrics":
        return ["metrics"];
      case "actions":
        return ["actions"];
      case "deploys":
        return [];
      case "logs":
        return ["logs"];
      case "errors":
        return ["errors"];
      case "secrets":
        return ["secrets"];
      case "settings":
        return [];
    }
  }

  async function loadViewData(
    slug: string,
    view: ViewId,
    range: number,
    myEpoch: number,
    quiet: boolean,
  ): Promise<void> {
    const fresh = () => epoch === myEpoch;
    const put = <T,>(a: { set: (p: Panel<T>) => void }, p: Panel<T>): void => {
      if (fresh()) {
        a.set(p);
      }
    };
    const jobs: Array<Promise<void>> = [];
    if (loadersFor(view).includes("metrics")) {
      if (!quiet) {
        metrics.set({ kind: "loading" });
      }
      jobs.push(
        api<MetricsSummary>(
          `/api/console/apps/${encodeURIComponent(slug)}/metrics?hours=${range}`,
        )
          .then((summary) => {
            put(metrics, { kind: "ready", value: { hours: range, summary } });
          })
          .catch((err: unknown) => {
            if (!quiet) {
              put(metrics, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    if (loadersFor(view).includes("actions")) {
      if (!quiet) {
        actions.set({ kind: "loading" });
      }
      jobs.push(
        loadActionsPage(slug, null)
          .then((page) => {
            if (fresh()) {
              actions.set({
                kind: "ready",
                state: {
                  actions: page.actions,
                  next: page.next,
                  loadingMore: false,
                  moreError: null,
                },
              });
            }
          })
          .catch((err: unknown) => {
            if (!quiet) {
              put(actions, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    if (loadersFor(view).includes("errors") || (quiet && view !== "errors")) {
      if (!quiet && loadersFor(view).includes("errors")) {
        errors.set({ kind: "loading" });
      }
      jobs.push(
        api<ErrorsSummary>(
          `/api/console/apps/${encodeURIComponent(slug)}/errors`,
        )
          .then((summary) => {
            put(errors, { kind: "ready", value: summary });
          })
          .catch((err: unknown) => {
            if (!quiet && loadersFor(view).includes("errors")) {
              put(errors, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    if (loadersFor(view).includes("logs")) {
      if (!quiet) {
        logs.set({ kind: "loading" });
      }
      jobs.push(
        api<{ lines: LogLine[] }>(
          `/api/console/apps/${encodeURIComponent(slug)}/logs?limit=100`,
        )
          .then((data) => {
            put(logs, { kind: "ready", value: data.lines });
          })
          .catch((err: unknown) => {
            if (!quiet) {
              put(logs, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    if (loadersFor(view).includes("secrets")) {
      if (!quiet) {
        secrets.set({ kind: "loading" });
      }
      jobs.push(
        api<SecretsSummary>(
          `/api/console/apps/${encodeURIComponent(slug)}/secrets`,
        )
          .then((summary) => {
            put(secrets, { kind: "ready", value: summary });
          })
          .catch((err: unknown) => {
            if (!quiet) {
              put(secrets, { kind: "error", message: String(err) });
            }
          }),
      );
    }
    await Promise.all(jobs);
  }

  async function enter(r: Route, myEpoch: number): Promise<void> {
    if (!r.app) {
      return;
    }
    const fresh = () => epoch === myEpoch;
    detail.set({ kind: "loading" });
    metrics.set({ kind: "loading" });
    actions.set({ kind: "loading" });
    errors.set({ kind: "loading" });
    logs.set({ kind: "loading" });
    secrets.set({ kind: "loading" });
    try {
      const app = await api<AppDetail>(
        `/api/console/apps/${encodeURIComponent(r.app)}`,
      );
      if (!fresh()) {
        return;
      }
      detail.set({ kind: "ready", value: app });
      setTitle(r, app);
      await loadViewData(r.app, r.view, r.range, myEpoch, false);
    } catch (err: unknown) {
      if (fresh()) {
        const message = String(err);
        detail.set({ kind: "error", message });
        setTitle(r, null);
      }
    }
  }

  function navigate(next: Partial<Route>, replace = false): void {
    const prev = route();
    const r: Route = { ...prev, ...next };
    const url = routeUrl(r);
    if (replace) {
      window.history.replaceState(null, "", url);
    } else {
      window.history.pushState(null, "", url);
    }
    route.set(r);
    const current = ui();
    ui.set({ ...current, drawer: false, createOpen: false });
    epoch += 1;
    if (r.app !== prev.app) {
      void enter(r, epoch);
      return;
    }
    if (r.app) {
      void loadViewData(r.app, r.view, r.range, epoch, false);
    }
    const d = detail();
    if (d.kind === "ready") {
      setTitle(r, d.value);
    }
  }

  function loadMoreActions(): void {
    const current = actions();
    const r = route();
    if (current.kind !== "ready" || !current.state.next || !r.app) {
      return;
    }
    const slug = r.app;
    const before = current.state.next;
    actions.set({
      kind: "ready",
      state: { ...current.state, loadingMore: true, moreError: null },
    });
    loadActionsPage(slug, before)
      .then((page) => {
        const latest = actions();
        if (latest.kind !== "ready") {
          return;
        }
        actions.set({
          kind: "ready",
          state: {
            actions: [...latest.state.actions, ...page.actions],
            next: page.next,
            loadingMore: false,
            moreError: null,
          },
        });
      })
      .catch((err: unknown) => {
        const latest = actions();
        if (latest.kind !== "ready") {
          return;
        }
        actions.set({
          kind: "ready",
          state: { ...latest.state, loadingMore: false, moreError: String(err) },
        });
      });
  }

  watch.once(() => {
    if (typeof window === "undefined") {
      return;
    }
    // The island prerenders at build time (no window): re-sync the route
    // from the live URL on client boot, never trusting the restored atom.
    route.set(readRoute());
    api<{ apps: ConsoleApp[] }>("/api/console/apps")
      .then(async (data) => {
        apps.set({ kind: "ready", value: data.apps });
        try {
          const me = await api<{ email: string }>("/api/console/me");
          email.set(me.email);
        } catch {
          email.set("");
        }
        const r = route();
        if (!r.app) {
          if (data.apps.length > 0) {
            const first = data.apps[0].slug;
            const next: Route = { ...r, app: first };
            window.history.replaceState(null, "", routeUrl(next));
            route.set(next);
            epoch += 1;
            await enter(next, epoch);
          }
          return;
        }
        if (!data.apps.some((a) => a.slug === r.app)) {
          detail.set({
            kind: "error",
            message: "request failed: 404",
          });
          return;
        }
        epoch += 1;
        await enter(r, epoch);
      })
      .catch((err: unknown) => {
        apps.set({ kind: "error", message: String(err) });
        detail.set({ kind: "error", message: String(err) });
      });
    window.addEventListener("popstate", () => {
      const r = readRoute();
      route.set(r);
      if (!r.app) {
        return;
      }
      const known = apps();
      if (known.kind === "ready" && !known.value.some((a) => a.slug === r.app)) {
        detail.set({ kind: "error", message: "request failed: 404" });
        return;
      }
      epoch += 1;
      void enter(r, epoch);
    });
    // Noite's live feed, polling edition: refresh the visible view only.
    window.setInterval(() => {
      const r = route();
      if (!r.app || document.hidden) {
        return;
      }
      void loadViewData(r.app, r.view, r.range, epoch, true);
    }, 10_000);
  });

  const r = route();
  const appsPanel = apps();
  const detailPanel = detail();

  if (appsPanel.kind === "loading") {
    return (
      <div class="console-boot" role="status" aria-label="Loading console">
        <div class="skeleton-rail" />
        <div class="skeleton-side" />
        <div class="skeleton-main" />
      </div>
    );
  }
  if (appsPanel.kind === "error") {
    return (
      <div class="console-error" role="alert">
        <p>Couldn't load your apps: {appsPanel.message}</p>
        <a href="/console/sign-in">Sign in</a>
      </div>
    );
  }
  const list = appsPanel.value;
  if (list.length === 0) {
    return (
      <div class="console-empty" role="status">
        <OuterMark size={28} />
        <h1>No apps yet</h1>
        <p>
          Apps are created by your agent over MCP at{" "}
          <code>https://outer.now/mcp</code> — there is nothing to click here
          yet.
        </p>
      </div>
    );
  }
  const active =
    list.find((a) => a.slug === r.app) ??
    (detailPanel.kind === "ready" ? detailPanel.value : null);
  const errorsPanel = errors();
  const openErrors =
    errorsPanel.kind === "ready"
      ? errorsPanel.value.issues.filter((i) => i.status === "open").length
      : 0;

  const patchUi = (patch: Partial<Ui>) => ui.set({ ...ui(), ...patch });

  return (
    <div class={`console${ui().collapsed ? " is-collapsed" : ""}`}>
      <a class="skip-link" href="#console-main">
        Skip to content
      </a>
      <nav class="rail" aria-label="Applications">
        <a
          class="rail-tile rail-mark"
          href="/console"
          title="Outer console"
          aria-label="Outer console"
          onclick={(e: Event) => {
            e.preventDefault();
            navigate({ app: list[0].slug, view: "overview" });
          }}
        >
          <OuterMark />
        </a>
        <div class="rail-apps" role="list">
          {list.map((a) => (
            <button
              key={a.slug}
              type="button"
              role="listitem"
              class={`rail-tile rail-app tint-${tintIndex(a.slug)}${a.slug === r.app ? " is-active" : ""}`}
              title={`${a.slug} · ${a.status}`}
              aria-label={`${a.slug}, ${a.status}`}
              aria-current={a.slug === r.app ? "page" : undefined}
              onclick={() => {
                if (a.slug !== r.app) {
                  navigate({ app: a.slug, view: "overview" });
                }
              }}
            >
              <span class="rail-mono" aria-hidden="true">
                {monogram(a.slug)}
              </span>
              <span
                class={`status-dot is-${a.status}`}
                aria-hidden="true"
              />
            </button>
          ))}
        </div>
        <div class="rail-create">
          <button
            type="button"
            class="rail-tile"
            title="New app"
            aria-label="How apps are created"
            aria-expanded={ui().createOpen}
            onclick={() => patchUi({ createOpen: !ui().createOpen })}
          >
            <PlusIcon />
          </button>
          {ui().createOpen && (
            <div class="rail-pop" role="dialog" aria-label="Creating apps">
              <div class="rail-pop-head">
                <strong>Apps are created by your agent</strong>
                <button
                  type="button"
                  class="icon-btn"
                  aria-label="Close"
                  onclick={() => patchUi({ createOpen: false })}
                >
                  <CloseIcon size={14} />
                </button>
              </div>
              <p>
                There is no click-to-create here. Ask your agent to create an
                app over MCP:
              </p>
              <p class="rail-pop-row">
                <code>https://outer.now/mcp</code>
                <CopyButton
                  value="https://outer.now/mcp"
                  label="Copy MCP URL"
                />
              </p>
            </div>
          )}
        </div>
        <div class="rail-foot">
          <a class="rail-tile" href="/" title="Docs" aria-label="Docs">
            <DocsIcon />
          </a>
          <button
            type="button"
            class="rail-tile"
            title={email() || "Account"}
            aria-label={email() ? `Account: ${email()}` : "Account"}
            aria-expanded={ui().accountOpen}
            onclick={() => patchUi({ accountOpen: !ui().accountOpen })}
          >
            <span class="rail-mono" aria-hidden="true">
              {(email() || "?").slice(0, 1).toUpperCase()}
            </span>
          </button>
          {ui().accountOpen && (
            <div
              class="rail-pop rail-account"
              role="dialog"
              aria-label="Account"
            >
              <p class="rail-account-email" title={email()}>
                {email()}
              </p>
              <button
                type="button"
                class="menu-row"
                onclick={() => {
                  void fetch("/api/console/sign-out", {
                    method: "POST",
                  }).finally(() => {
                    window.location.assign("/console/sign-in");
                  });
                }}
              >
                <SignOutIcon size={16} />
                Sign out
              </button>
            </div>
          )}
        </div>
      </nav>

      {active && (
        <aside
          class={`side${ui().drawer ? " is-open" : ""}`}
          aria-label="App navigation"
        >
          <div class="side-head">
            <div class="side-id">
              <p class="side-name" title={active.slug}>
                {active.slug}
              </p>
              <a class="side-url" href={active.url} title={active.url}>
                {active.url.replace(/^https?:\/\//, "")}
              </a>
            </div>
            <span class={`pill is-${active.status}`}>{active.status}</span>
            <button
              type="button"
              class="icon-btn side-collapse"
              title="Collapse sidebar"
              aria-label="Collapse sidebar"
              onclick={() => patchUi({ collapsed: true })}
            >
              <CollapseIcon />
            </button>
          </div>
          <p class="side-label">App</p>
          <ul class="side-list">
            {VIEWS.map((v) => (
              <li key={v.id}>
                <button
                  type="button"
                  class={`side-item${r.view === v.id ? " is-active" : ""}`}
                  aria-current={r.view === v.id ? "page" : undefined}
                  onclick={() => navigate({ view: v.id as ViewId })}
                >
                  <ViewIcon id={v.id as ViewId} />
                  <span>{v.label}</span>
                  {v.id === "errors" && openErrors > 0 && (
                    <span class="count-badge" aria-label={`${openErrors} open`}>
                      {openErrors}
                    </span>
                  )}
                </button>
              </li>
            ))}
          </ul>
          <p class="side-label">Data</p>
          <ul class="side-list">
            {(["D1", "R2", "Durable Objects"] as const).map((name) => (
              <li key={name}>
                <span
                  class="side-item is-disabled"
                  aria-disabled="true"
                  title="Coming later"
                >
                  <DatabaseIcon />
                  <span>{name}</span>
                  <span class="soon-pill">Soon</span>
                </span>
              </li>
            ))}
          </ul>
        </aside>
      )}
      {ui().drawer && active && (
        <button
          type="button"
          class="scrim"
          aria-label="Close navigation"
          onclick={() => patchUi({ drawer: false })}
        />
      )}

      <main class="main" id="console-main" tabindex={-1}>
        <div class="main-head">
          {active && (
            <button
              type="button"
              class="icon-btn main-menu"
              aria-label="Open navigation"
              onclick={() => patchUi({ drawer: true })}
            >
              <MenuIcon />
            </button>
          )}
          <h1 class="main-title">{active ? VIEW_TITLES[r.view] : "Console"}</h1>
          {active && (r.view === "overview" || r.view === "metrics") && (
            <div
              class="range"
              role="group"
              aria-label="Metrics range"
            >
              {RANGES.map((opt) => (
                <button
                  key={opt.hours}
                  type="button"
                  class={`range-btn${r.range === opt.hours ? " is-active" : ""}`}
                  title={opt.title}
                  aria-pressed={r.range === opt.hours}
                  onclick={() => navigate({ r: opt.hours } as Partial<Route>)}
                >
                  {opt.label}
                </button>
              ))}
            </div>
          )}
          {active && ui().collapsed && (
            <button
              type="button"
              class="icon-btn"
              title="Expand sidebar"
              aria-label="Expand sidebar"
              onclick={() => patchUi({ collapsed: false })}
            >
              <MenuIcon />
            </button>
          )}
        </div>
        <div class="main-body">
          {!active && (
            <div class="card" role="status">
              <p>Select an app from the rail.</p>
            </div>
          )}
          {active && detailPanel.kind === "loading" && (
            <div class="stack" role="status" aria-label="Loading app">
              <div class="skeleton-card" />
              <div class="skeleton-chart" />
              <div class="skeleton-card" />
            </div>
          )}
          {active && detailPanel.kind === "error" && (
            <div class="card" role="alert">
              <p>Couldn't load this app (unknown or not yours).</p>
              <button
                type="button"
                class="btn"
                onclick={() => navigate({ app: list[0].slug, view: "overview" })}
              >
                Back to {list[0].slug}
              </button>
            </div>
          )}
          {active && detailPanel.kind === "ready" && (
            <ViewBody
              route={r}
              app={detailPanel.value}
              metrics={metrics()}
              actions={actions()}
              errors={errors()}
              logs={logs()}
              secrets={secrets()}
              expanded={ui().expanded}
              onToggleExpand={(id) => {
                const cur = ui().expanded;
                patchUi({
                  expanded: cur.includes(id)
                    ? cur.filter((x) => x !== id)
                    : [...cur, id],
                });
              }}
              onRange={(hours) => navigate({ r: hours } as Partial<Route>)}
              onLoadMore={loadMoreActions}
              onRefreshLogs={() => {
                const slug = route().app;
                if (!slug) {
                  return;
                }
                logs.set({ kind: "loading" });
                api<{ lines: LogLine[] }>(
                  `/api/console/apps/${encodeURIComponent(slug)}/logs?limit=100`,
                )
                  .then((data) => {
                    logs.set({ kind: "ready", value: data.lines });
                  })
                  .catch((err: unknown) => {
                    logs.set({ kind: "error", message: String(err) });
                  });
              }}
            />
          )}
        </div>
      </main>
    </div>
  );
}

function ViewIcon({ id }: { id: ViewId }) {
  switch (id) {
    case "overview":
      return <OverviewIcon />;
    case "metrics":
      return <MetricsIcon />;
    case "actions":
      return <ActionsIcon />;
    case "deploys":
      return <DeploysIcon />;
    case "logs":
      return <LogsIcon />;
    case "errors":
      return <ErrorsIcon />;
    case "secrets":
      return <SecretsIcon />;
    case "settings":
      return <SettingsIcon />;
  }
}

function ViewBody(props: {
  route: Route;
  app: AppDetail;
  metrics: Panel<{ hours: number; summary: MetricsSummary }>;
  actions: ActionsPanel;
  errors: Panel<ErrorsSummary>;
  logs: Panel<LogLine[]>;
  secrets: Panel<SecretsSummary>;
  expanded: string[];
  onToggleExpand: (id: string) => void;
  onRange: (hours: number) => void;
  onLoadMore: () => void;
  onRefreshLogs: () => void;
}) {
  switch (props.route.view) {
    case "overview":
      return (
        <OverviewView
          app={props.app}
          metrics={props.metrics}
          actions={props.actions}
          errors={props.errors}
          onRange={props.onRange}
          onView={(view) => navigate({ view })}
        />
      );
    case "metrics":
      return <MetricsView metrics={props.metrics} onRange={props.onRange} />;
    case "actions":
      return <ActionsView panel={props.actions} onMore={props.onLoadMore} />;
    case "deploys":
      return (
        <DeploysView
          app={props.app}
          expanded={props.expanded}
          onToggle={props.onToggleExpand}
        />
      );
    case "logs":
      return <LogsView panel={props.logs} onRefresh={props.onRefreshLogs} />;
    case "errors":
      return <ErrorsView panel={props.errors} />;
    case "secrets":
      return <SecretsView panel={props.secrets} />;
    case "settings":
      return <SettingsView app={props.app} />;
  }
}
