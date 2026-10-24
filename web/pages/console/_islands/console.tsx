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
  return (clean.slice(0, 1) || "?").toUpperCase();
}

// daisyUI mappings: status pills are `badge badge-soft`, rail dots are
// `indicator-item` dots with theme backgrounds.
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
  return "badge-ghost";
}

function statusDot(status: string): string {
  if (status === "success" || status === "running" || status === "ok") {
    return "bg-success";
  }
  if (
    status === "building" ||
    status === "deploying" ||
    status === "reviewing" ||
    status === "queued"
  ) {
    return "bg-warning";
  }
  if (status === "failed" || status === "error" || status === "blocked") {
    return "bg-error";
  }
  return "bg-base-300";
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
    window.addEventListener("keydown", (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        const current = ui();
        if (current.createOpen || current.accountOpen || current.drawer) {
          ui.set({ ...current, createOpen: false, accountOpen: false, drawer: false });
        }
      }
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
        <div class="skeleton rounded-box min-h-[calc(100dvh-24px)]" />
        <div class="skeleton rounded-box min-h-[60dvh]" />
      </div>
    );
  }
  if (appsPanel.kind === "error") {
    return (
      <div class="console-error" role="alert">
        <div class="alert alert-error alert-soft max-w-md">
          <span class="min-w-0">Couldn't load your apps: {appsPanel.message}</span>
        </div>
        <a class="btn btn-neutral" href="/console/sign-in">Sign in</a>
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
          class="rail-tile tooltip tooltip-right"
          data-tip="Outer console"
          href="/console"
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
              class="tooltip tooltip-right rounded-[14px]"
              data-tip={`${a.slug} · ${a.status}`}
              aria-label={`${a.slug}, ${a.status}`}
              aria-current={a.slug === r.app ? "page" : undefined}
              onclick={() => {
                if (a.slug !== r.app) {
                  navigate({ app: a.slug, view: "overview" });
                }
              }}
            >
              <span class="indicator">
                <span
                  class={`indicator-item indicator-bottom indicator-end h-2.5 w-2.5 rounded-full border-2 border-base-100 ${statusDot(a.status)}`}
                  aria-hidden="true"
                />
                <span class="avatar avatar-placeholder">
                  <span
                    class={`flex h-11 w-11 items-center justify-center rounded-[14px] text-xl font-bold ${a.slug === r.app ? "border border-base-300 bg-base-100 shadow-sm" : `tint-${tintIndex(a.slug)}`}`}
                  >
                    <span aria-hidden="true">
                      {monogram(a.slug)}
                    </span>
                  </span>
                </span>
              </span>
            </button>
          ))}
        </div>
        <div class={`dropdown dropdown-right${ui().createOpen ? " dropdown-open" : ""}`}>
          <button
            type="button"
            class="rail-tile tooltip tooltip-right"
            data-tip="How apps are created"
            aria-label="How apps are created"
            aria-expanded={ui().createOpen}
            onclick={() => patchUi({ createOpen: !ui().createOpen })}
          >
            <PlusIcon />
          </button>
          {ui().createOpen && (
            <div class="dropdown-content card card-border bg-base-100 w-64 p-4 text-left shadow-lg" role="dialog" aria-label="Creating apps">
              <div class="mb-1 flex min-w-0 items-center justify-between gap-2">
                <strong class="min-w-0 flex-1 truncate">Apps are created by your agent</strong>
                <button
                  type="button"
                  class="btn btn-ghost btn-square btn-xs shrink-0"
                  aria-label="Close"
                  onclick={() => patchUi({ createOpen: false })}
                >
                  <CloseIcon size={14} />
                </button>
              </div>
              <p class="my-1.5 min-w-0 text-sm opacity-70">
                There is no click-to-create here. Ask your agent to create an
                app over MCP:
              </p>
              <p class="mt-1.5 flex min-w-0 items-center gap-2">
                <code class="min-w-0 flex-1 truncate font-mono text-xs" title="https://outer.now/mcp">https://outer.now/mcp</code>
                <CopyButton
                  value="https://outer.now/mcp"
                  label="Copy MCP URL"
                />
              </p>
              <p class="mt-2 min-w-0 text-xs opacity-60">
                Press <kbd class="kbd kbd-xs">Esc</kbd> to close
              </p>
            </div>
          )}
        </div>
        <div class="rail-foot">
          <a class="rail-tile tooltip tooltip-right" data-tip="Docs" href="/" aria-label="Docs">
            <DocsIcon />
          </a>
          <div class={`dropdown dropdown-right dropdown-top${ui().accountOpen ? " dropdown-open" : ""}`}>
            <button
              type="button"
              class="tooltip tooltip-right rounded-[14px]"
              data-tip={email() ? `Account: ${email()}` : "Account"}
              aria-label={email() ? `Account: ${email()}` : "Account"}
              aria-expanded={ui().accountOpen}
              onclick={() => patchUi({ accountOpen: !ui().accountOpen })}
            >
              <span class="avatar avatar-placeholder">
                <span class="bg-base-300 flex h-11 w-11 items-center justify-center rounded-[14px] text-xl font-bold">
                  <span aria-hidden="true">
                    {(email() || "?").slice(0, 1).toUpperCase()}
                  </span>
                </span>
              </span>
            </button>
            {ui().accountOpen && (
              <ul class="dropdown-content menu bg-base-100 rounded-box border border-base-300 w-60 min-w-0 p-2 shadow-lg" role="menu" aria-label="Account">
                <li class="menu-title min-w-0">
                  <span class="block truncate" title={email()}>
                    {email() || "Account"}
                  </span>
                </li>
                <li>
                  <button
                    type="button"
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
                </li>
              </ul>
            )}
          </div>
        </div>
      </nav>

      {active && (
        <div class={`drawer min-w-0${ui().collapsed ? "" : " md:drawer-open"}`}>
          <input
            id="console-nav"
            type="checkbox"
            class="drawer-toggle"
            checked={ui().drawer}
            aria-label="App navigation toggle"
            onchange={(e: Event) => patchUi({ drawer: (e.target as HTMLInputElement).checked })}
          />
          <div class="drawer-content min-w-0">
            <main class="main" id="console-main" tabindex={-1}>
              <div class="main-head">
                <button
                  type="button"
                  class="btn btn-ghost btn-square btn-sm shrink-0 md:hidden"
                  aria-label="Open navigation"
                  onclick={() => patchUi({ drawer: true })}
                >
                  <MenuIcon />
                </button>
                <h1 class="main-title">{VIEW_TITLES[r.view]}</h1>
                {(r.view === "overview" || r.view === "metrics") && (
                  <div class="join ml-auto shrink-0" role="group" aria-label="Metrics range">
                    {RANGES.map((opt) => (
                      <button
                        key={opt.hours}
                        type="button"
                        class={`btn btn-sm join-item${r.range === opt.hours ? " btn-neutral" : ""}`}
                        title={opt.title}
                        aria-pressed={r.range === opt.hours}
                        onclick={() => navigate({ r: opt.hours } as Partial<Route>)}
                      >
                        {opt.label}
                      </button>
                    ))}
                  </div>
                )}
                {ui().collapsed && (
                  <button
                    type="button"
                    class="btn btn-ghost btn-square btn-sm shrink-0"
                    title="Expand sidebar"
                    aria-label="Expand sidebar"
                    onclick={() => patchUi({ collapsed: false })}
                  >
                    <MenuIcon />
                  </button>
                )}
              </div>
              <div class="tabs tabs-box tabs-sm mb-3 w-full min-w-0 overflow-x-auto md:hidden" role="tablist" aria-label="Views">
                {VIEWS.map((v) => (
                  <button
                    key={v.id}
                    type="button"
                    role="tab"
                    aria-selected={r.view === v.id}
                    class={`tab shrink-0${r.view === v.id ? " tab-active" : ""}`}
                    onclick={() => navigate({ view: v.id as ViewId })}
                  >
                    {v.label}
                  </button>
                ))}
              </div>
              <div class="main-body">
                {detailPanel.kind === "loading" && (
                  <div class="flex min-w-0 flex-col gap-3" role="status" aria-label="Loading app">
                    <div class="skeleton rounded-box h-30 w-full" />
                    <div class="skeleton rounded-box h-70 w-full" />
                    <div class="skeleton rounded-box h-30 w-full" />
                  </div>
                )}
                {detailPanel.kind === "error" && (
                  <div class="alert alert-error alert-soft" role="alert">
                    <span class="min-w-0">Couldn't load this app (unknown or not yours).</span>
                    <button
                      type="button"
                      class="btn btn-sm min-w-0 max-w-full shrink-0"
                      onclick={() => navigate({ app: list[0].slug, view: "overview" })}
                    >
                      <span class="truncate" title={`Back to ${list[0].slug}`}>
                        Back to {list[0].slug}
                      </span>
                    </button>
                  </div>
                )}
                {detailPanel.kind === "ready" && (
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
          <div class="drawer-side">
            <label for="console-nav" class="drawer-overlay" aria-label="Close navigation" />
            <aside class="side max-md:min-h-dvh min-w-0" aria-label="App navigation">
              <div class="side-head">
                <div class="side-id">
                  <p class="m-0 min-w-0 truncate text-[15px] font-bold" title={active.slug}>
                    {active.slug}
                  </p>
                  <a class="block truncate text-xs opacity-60 hover:opacity-100 hover:underline" href={active.url} title={active.url}>
                    {active.url.replace(/^https?:\/\//, "")}
                  </a>
                </div>
                <span class={`badge badge-soft ${statusBadge(active.status)} shrink-0`} title={active.status}>{active.status}</span>
                <button
                  type="button"
                  class="btn btn-ghost btn-square btn-sm hidden shrink-0 md:inline-flex"
                  title="Collapse sidebar"
                  aria-label="Collapse sidebar"
                  onclick={() => patchUi({ collapsed: true })}
                >
                  <CollapseIcon />
                </button>
              </div>
              <ul class="menu w-full min-w-0 gap-0.5 p-0">
                <li class="menu-title">App</li>
                {VIEWS.map((v) => (
                  <li key={v.id} class="min-w-0">
                    <button
                      type="button"
                      class={r.view === v.id ? "menu-active min-w-0" : "min-w-0"}
                      aria-current={r.view === v.id ? "page" : undefined}
                      onclick={() => navigate({ view: v.id as ViewId })}
                    >
                      <ViewIcon id={v.id as ViewId} />
                      <span class="min-w-0 flex-1 truncate">{v.label}</span>
                      {v.id === "errors" && openErrors > 0 && (
                        <span class="indicator shrink-0">
                          <span class="indicator-item badge badge-error badge-xs" aria-label={`${openErrors} open`}>
                            {openErrors}
                          </span>
                          <span class="block h-4 w-4" aria-hidden="true" />
                        </span>
                      )}
                    </button>
                  </li>
                ))}
                <li class="menu-title">Data</li>
                {(["D1", "R2", "Durable Objects"] as const).map((name) => (
                  <li key={name} class="menu-disabled min-w-0" title="Coming later">
                    <span class="min-w-0">
                      <DatabaseIcon />
                      <span class="truncate">{name}</span>
                      <span class="badge badge-ghost badge-xs ml-auto shrink-0">Soon</span>
                    </span>
                  </li>
                ))}
              </ul>
              <div class="side-foot">
                <div class="divider my-2" aria-hidden="true" />
                <p class="m-0 min-w-0 truncate font-mono text-xs opacity-60" title={active.url}>
                  {active.url}
                </p>
                <div class="mt-2 flex min-w-0 items-center gap-2">
                  <CopyButton value={active.url} label="Copy app URL" />
                  <a class="link min-w-0 flex-1 truncate" href={active.url} title={active.url}>
                    Open app
                  </a>
                </div>
              </div>
            </aside>
          </div>
        </div>
      )}
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
          onView={(view) => navigate({ view })}
        />
      );
    case "metrics":
      return <MetricsView metrics={props.metrics} />;
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
